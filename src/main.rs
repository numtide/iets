use std::env;
use std::process::exit;

use iets::cli::run::{classic_name, dispatch};
use iets::sys;

// musl's malloc costs up to 30% (docs/explanation/static-build.md)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `mi_option_allow_thp` in mimalloc v3, which libmimalloc-sys has no name for.
const MI_OPTION_ALLOW_THP: libmimalloc_sys::mi_option_t = 43;

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
    let session = iets::session::Session::new();
    session.log.write_to_stderr();
    let _entered = session.enter();
    let argv: Vec<String> = env::args_os().skip(1).map(iets::argv::encode).collect();
    let classic = env::args_os().next().and_then(|a| classic_name(&a));
    exit(dispatch(&session, argv, classic))
}
