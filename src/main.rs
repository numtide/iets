use std::env;
use std::process::exit;

use iets::cli::run::{classic_name, dispatch};
use iets::sys;

// musl's malloc costs up to 30% (docs/explanation/static-build.md)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `mi_option_allow_thp` in mimalloc v3, which libmimalloc-sys has no name for.
const MI_OPTION_ALLOW_THP: libmimalloc_sys::mi_option_t = 43;

/// A command whose process may end with a heap of many GB: its exit code goes
/// back before the kernel has freed it (`sys::detach_exit`). Not classic mode,
/// which can exec Nix, nor an eval-jobs worker, whose collector waits for it.
fn detaches(argv: &[String], classic: bool) -> bool {
    !classic
        && matches!(
            argv.first().map(String::as_str),
            Some("eval" | "build" | "eval-jobs")
        )
        && !argv.iter().any(|a| a == "--worker")
        && env::var_os("IETS_WAIT_EXIT").is_none()
}

fn main() {
    // mimalloc advises every arena it reserves huge, outside the gate in
    // sys::advise_huge. Cleared after its start, the option stops that for
    // later arenas only: cleared before, it disables THP for the process.
    // SAFETY: an option store.
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_ALLOW_THP, 0) };
    let probe = Box::new(0u8);
    sys::note_allocator_arena(&raw const *probe);
    drop(probe);
    sys::tty::restore_sigpipe();
    let argv: Vec<String> = env::args_os().skip(1).map(iets::argv::encode).collect();
    let classic = env::args_os().next().and_then(|a| classic_name(&a));
    // before any thread: the session starts none, but the work after it does
    let note = if detaches(&argv, classic.is_some()) {
        sys::detach_exit()
    } else {
        None
    };
    let session = iets::session::Session::new();
    session.log.write_to_stderr();
    let _entered = session.enter();
    let code = dispatch(&session, argv, classic);
    match note {
        Some(n) => n.exit(code),
        None => exit(code),
    }
}
