//! WASI has no terminal control and no signals. Output stays plain.

pub fn size() -> Option<(usize, usize)> {
    None
}

pub fn cursor() -> Option<(usize, usize)> {
    None
}

pub fn guard_region(_top: usize) {}

pub fn is_root() -> bool {
    false
}

pub fn restore_sigpipe() {}
