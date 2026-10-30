//! Anonymous memory through `VirtualAlloc`.
//!
//! Windows charges committed memory against a system-wide limit, even if
//! nothing touches it. The big tables here are mostly empty, so committing
//! them up front would use the limit up after a few of them.
//!
//! A lazy mapping is therefore only reserved. A handler commits one chunk
//! whenever a touch faults, much like demand paging does on Unix.
//!
//! A stack commits its top and a low zone. The kernel grows it through a
//! guard page, as it does a thread's.

use std::io;
use std::mem;
use std::ptr;
use std::sync::Once;

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_GUARD,
    PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

pub fn page_size() -> usize {
    // SAFETY: the struct is the one the call fills.
    unsafe {
        let mut info: SYSTEM_INFO = mem::zeroed();
        GetSystemInfo(&mut info);
        info.dwPageSize as usize
    }
}

/// Bytes of `[base, base + len)` that are in the working set, in whole pages.
pub fn resident(base: *mut u8, len: usize) -> usize {
    let page = page_size();
    let mut pages: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..len / page)
        .map(|i| {
            // SAFETY: all-zero is a valid value of this plain struct.
            let mut p: PSAPI_WORKING_SET_EX_INFORMATION = unsafe { mem::zeroed() };
            p.VirtualAddress = base.wrapping_add(i * page).cast();
            p
        })
        .collect();
    let bytes = (pages.len() * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32;
    // SAFETY: the buffer holds `bytes` bytes of the structs the call fills.
    let ok = unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), pages.as_mut_ptr().cast(), bytes) };
    if ok == 0 {
        return 0;
    }
    // SAFETY: the call filled the attributes, whose first bit is `Valid`.
    let valid =
        |p: &PSAPI_WORKING_SET_EX_INFORMATION| unsafe { p.VirtualAttributes.Flags & 1 != 0 };
    pages.iter().filter(|p| valid(p)).count() * page
}

/// How much one fault commits. This counts against the commit limit at once,
/// but pages only use memory when they are touched.
const CHUNK: usize = 1 << 20;

const ACCESS_VIOLATION: i32 = 0xC000_0005_u32 as i32;

static HANDLER: Once = Once::new();

/// Commits the chunk around a touch of reserved memory, then retries the
/// access. Any other fault is passed on, including one on a fenced page.
unsafe extern "system" fn commit_on_touch(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes valid pointers to the handler.
    let record = unsafe { &*(*info).ExceptionRecord };
    if record.ExceptionCode != ACCESS_VIOLATION || record.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let addr = record.ExceptionInformation[1] as *const u8;
    // SAFETY: the struct is the one the call fills.
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { mem::zeroed() };
    let got = unsafe { VirtualQuery(addr.cast(), &mut mbi, size_of::<MEMORY_BASIC_INFORMATION>()) };
    if got == 0 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // Two threads can fault on one chunk. The second finds it already
    // committed and only needs to retry.
    if mbi.State == MEM_COMMIT && mbi.Protect == PAGE_READWRITE {
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if mbi.State != MEM_RESERVE {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let start = (addr as usize) & !(CHUNK - 1);
    let start = start.max(mbi.BaseAddress as usize);
    let end = (start + CHUNK).min(mbi.BaseAddress as usize + mbi.RegionSize);
    // SAFETY: the range lies inside a region this process reserved.
    let p = unsafe { VirtualAlloc(start as *const _, end - start, MEM_COMMIT, PAGE_READWRITE) };
    if p.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    EXCEPTION_CONTINUE_EXECUTION
}

/// `len` zeroed bytes, readable and writable. A `lazy` mapping is only
/// reserved and gets committed chunk by chunk as it is touched.
pub fn map(len: usize, lazy: bool) -> io::Result<*mut u8> {
    let mut how = MEM_RESERVE;
    if lazy {
        HANDLER.call_once(|| {
            // SAFETY: the handler is a plain function that lives for the process.
            unsafe { AddVectoredExceptionHandler(1, Some(commit_on_touch)) };
        });
    } else {
        how |= MEM_COMMIT;
    }
    // SAFETY: a fresh allocation aliases nothing.
    let p = unsafe { VirtualAlloc(ptr::null(), len, how, PAGE_READWRITE) };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(p.cast())
}

/// Low bytes of a stack that stay committed, for the overflow exception to run in.
pub const STACK_ZONE: usize = 256 << 10;
/// Bytes at the low end of a stack whose faults mean an overflow.
pub fn fault_span() -> usize {
    STACK_ZONE
}

const STACK_INIT: usize = 256 << 10;
const STACK_HEAD: usize = 64 << 10;

fn arm_guard(at: *mut u8) {
    let mut old = 0;
    // SAFETY: the page is committed memory of the caller's.
    unsafe {
        VirtualProtect(
            at.cast(),
            page_size(),
            PAGE_READWRITE | PAGE_GUARD,
            &mut old,
        )
    };
}

/// `len` bytes for a stack. One too small for the layout is committed in full.
pub fn map_stack(len: usize) -> io::Result<*mut u8> {
    if len < 2 * (STACK_INIT + STACK_ZONE) {
        return map(len, false);
    }
    // SAFETY: a fresh allocation aliases nothing.
    let p = unsafe { VirtualAlloc(ptr::null(), len, MEM_RESERVE, PAGE_READWRITE) }.cast::<u8>();
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    let top = p.wrapping_add(len - STACK_INIT);
    // SAFETY: both ranges lie inside the reservation just made.
    let ok = unsafe {
        !VirtualAlloc(top.cast(), STACK_INIT, MEM_COMMIT, PAGE_READWRITE).is_null()
            && !VirtualAlloc(p.cast(), STACK_ZONE, MEM_COMMIT, PAGE_READWRITE).is_null()
    };
    if !ok {
        let e = io::Error::last_os_error();
        // SAFETY: the allocation is ours and unused.
        unsafe { unmap(p, len) };
        return Err(e);
    }
    arm_guard(top.wrapping_add(STACK_HEAD));
    Ok(p)
}

/// Returns the pages of an idle stack except the top `keep` bytes.
///
/// # Safety
/// `[base, base + len)` is a stack from `map_stack` that nothing runs on.
pub unsafe fn trim_stack(base: *mut u8, len: usize, keep: usize) {
    if len < 2 * (STACK_INIT + STACK_ZONE) {
        // SAFETY: the range is inside the mapping and nothing uses it.
        unsafe { reset(base.add(STACK_ZONE), len - STACK_ZONE - keep) };
        return;
    }
    let init = STACK_INIT.max(keep);
    let top = base.wrapping_add(len - init);
    // SAFETY: the ranges lie inside the stack and nothing uses them.
    unsafe {
        VirtualFree(
            base.add(STACK_ZONE).cast(),
            len - STACK_ZONE - init,
            MEM_DECOMMIT,
        );
        if init > keep {
            reset(top, init - keep);
        }
    }
    arm_guard(top.wrapping_add(STACK_HEAD));
}

/// Releases the whole allocation that starts at `p`. Windows takes no length.
///
/// # Safety
/// `p` is the start of a mapping of the caller's that nothing uses any more.
pub unsafe fn unmap(p: *mut u8, _len: usize) {
    let _ = unsafe { VirtualFree(p.cast(), 0, MEM_RELEASE) };
}

/// Makes the low `len` bytes of a mapping fault when touched.
///
/// # Safety
/// The range is inside a mapping of the caller's and nothing needs it.
pub unsafe fn fence(p: *mut u8, len: usize) {
    let mut old = 0;
    let _ = unsafe { VirtualProtect(p.cast(), len, PAGE_NOACCESS, &mut old) };
}

/// Decommits the range. The address space stays reserved and a touch faults.
/// Returns `false` and changes nothing if that fails.
///
/// # Safety
/// The range is a mapping of the caller's and nothing may touch it again.
pub unsafe fn revoke(p: *mut u8, len: usize) -> bool {
    unsafe { VirtualFree(p.cast(), len, MEM_DECOMMIT) != 0 }
}

/// Hands the pages back and commits fresh zero pages in their place.
///
/// # Safety
/// The range is a mapping of the caller's and nothing in it is needed.
pub unsafe fn reset(p: *mut u8, len: usize) {
    unsafe {
        if VirtualFree(p.cast(), len, MEM_DECOMMIT) != 0 {
            let _ = VirtualAlloc(p.cast(), len, MEM_COMMIT, PAGE_READWRITE);
        }
    }
}
