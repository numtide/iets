//! Windows paths in the evaluator's form, and back.
//!
//! The evaluator knows one separator and one kind of absolute path, which
//! starts with `/`. A drive becomes the first component, so `C:\Users\x` is
//! `/C:/Users/x` and `\\host\share\x` is `/UNC/host/share/x`. Only the file
//! system boundary converts, and everything in between stays as it is.
//!
//! These work on bytes and touch nothing, so they compile and run on every
//! OS. Only Windows calls them.
//!
//! Device paths (`\\.\pipe\x`) and verbatim paths that name neither a drive
//! nor a share (`\\?\Volume{..}\x`) are not understood. They only get their
//! separators swapped, so they survive a round trip but mean nothing else.

fn is_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

/// `C:` followed by a separator. A bare `C:` is the current directory of
/// drive C, which no absolute path names.
fn is_native_drive(p: &[u8]) -> bool {
    p.len() >= 3 && p[0].is_ascii_alphabetic() && p[1] == b':' && is_sep(p[2])
}

/// A whole component `C:`, which is what a drive is in the evaluator's form.
fn is_drive_component(c: &[u8]) -> bool {
    c.len() == 2 && c[0].is_ascii_alphabetic() && c[1] == b':'
}

fn push_slashed(out: &mut Vec<u8>, rest: &[u8]) {
    out.extend(rest.iter().map(|&b| if b == b'\\' { b'/' } else { b }));
}

fn push_drive(out: &mut Vec<u8>, p: &[u8]) {
    out.push(b'/');
    out.push(p[0].to_ascii_uppercase());
    out.push(b':');
    push_slashed(out, &p[2..]);
}

fn push_unc(out: &mut Vec<u8>, rest: &[u8]) {
    out.extend_from_slice(b"/UNC/");
    push_slashed(out, rest);
}

/// A native path as the evaluator names it.
pub fn to_internal(native: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(native.len() + 5);
    if native.len() >= 2 && is_sep(native[0]) && is_sep(native[1]) {
        let rest = &native[2..];
        match rest {
            [b'?', s, v @ ..] if is_sep(*s) => {
                if v.len() > 3 && v[..3].eq_ignore_ascii_case(b"UNC") && is_sep(v[3]) {
                    push_unc(&mut out, &v[4..]);
                } else if is_native_drive(v) {
                    push_drive(&mut out, v);
                } else {
                    push_slashed(&mut out, native);
                }
            }
            [b'.', s, ..] if is_sep(*s) => push_slashed(&mut out, native),
            _ => push_unc(&mut out, rest),
        }
    } else if is_native_drive(native) {
        push_drive(&mut out, native);
    } else {
        push_slashed(&mut out, native);
    }
    out
}

/// An evaluator path as Windows names it.
pub fn to_native(internal: &[u8]) -> Vec<u8> {
    let swap = |b: &u8| if *b == b'/' { b'\\' } else { *b };
    if internal.starts_with(b"//./") || internal.starts_with(b"//?/") {
        return internal.iter().map(swap).collect();
    }
    // A leading `//` would read as a network share
    let slashes = internal.iter().take_while(|b| **b == b'/').count();
    let p = &internal[slashes.saturating_sub(1)..];
    if let Some(rest) = p.strip_prefix(b"/UNC/") {
        let mut out = br"\\".to_vec();
        out.extend(rest.iter().map(swap));
        return out;
    }
    if let Some(d) = p.strip_prefix(b"/")
        && (d.len() == 2 || d.len() > 2 && d[2] == b'/')
        && is_drive_component(&d[..2])
    {
        let mut out = vec![d[0], b':'];
        out.extend(d[2..].iter().map(swap));
        if d.len() == 2 {
            out.push(b'\\');
        }
        return out;
    }
    p.iter().map(swap).collect()
}

/// How many leading components of an evaluator path `..` cannot remove.
/// Windows keeps `C:\..` at `C:\` and `\\host\share\..` at the share.
pub fn anchor_len(comps: &[&[u8]]) -> usize {
    match comps {
        [first, ..] if is_drive_component(first) => 1,
        [first, ..] if *first == b"UNC" => comps.len().min(3),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[cfg(windows)]
    use crate::compile::canon_path;

    fn i(s: &str) -> String {
        String::from_utf8(to_internal(s.as_bytes())).unwrap()
    }

    fn n(s: &str) -> String {
        String::from_utf8(to_native(s.as_bytes())).unwrap()
    }

    #[test]
    fn a_drive_is_the_first_component() {
        assert_eq!(i(r"C:\Users\x"), "/C:/Users/x");
        assert_eq!(i("C:/Users/x"), "/C:/Users/x");
        assert_eq!(i(r"c:\Users"), "/C:/Users");
        assert_eq!(i(r"C:\"), "/C:/");
    }

    #[test]
    fn a_bare_drive_is_not_its_root() {
        assert_eq!(i("C:"), "C:");
        assert_eq!(i("C:foo"), "C:foo");
        assert_eq!(n("C:"), "C:");
    }

    #[test]
    fn mixed_separators_agree() {
        assert_eq!(
            i(r"C:\Users\RUNNER~1\AppData/Local\Temp/repo/read.nix"),
            "/C:/Users/RUNNER~1/AppData/Local/Temp/repo/read.nix"
        );
    }

    #[test]
    fn network_shares_sit_under_unc() {
        assert_eq!(i(r"\\host\share\x"), "/UNC/host/share/x");
        assert_eq!(i(r"\\?\UNC\host\share\x"), "/UNC/host/share/x");
        assert_eq!(i(r"\\?\unc\host\share\x"), "/UNC/host/share/x");
        assert_eq!(i(r"\\?\C:\x"), "/C:/x");
    }

    #[test]
    fn forward_slashes_make_a_share_too() {
        assert_eq!(i("//host/share/x"), "/UNC/host/share/x");
        assert_eq!(i(r"/\host\share"), "/UNC/host/share");
    }

    #[test]
    fn a_relative_name_that_starts_with_unc_is_not_a_share() {
        assert_eq!(i(r"UNC\x"), "UNC/x");
        assert_eq!(i(r"unc\x"), "unc/x");
    }

    #[test]
    fn relative_and_rooted_paths_only_change_separator() {
        assert_eq!(i(r"a\b\c"), "a/b/c");
        assert_eq!(i(r"..\a"), "../a");
        assert_eq!(i(r"\nonexistent\x"), "/nonexistent/x");
        assert_eq!(i("ab:/x"), "ab:/x");
    }

    #[test]
    fn device_paths_are_not_shares_and_survive_a_round_trip() {
        assert_eq!(i(r"\\.\pipe\x"), "//./pipe/x");
        assert_eq!(n("//./pipe/x"), r"\\.\pipe\x");
        assert_eq!(i(r"\\?\Volume{1}\x"), "//?/Volume{1}/x");
        assert_eq!(n("//?/Volume{1}/x"), r"\\?\Volume{1}\x");
    }

    #[rstest]
    fn the_evaluators_form_is_left_alone(
        #[values("/C:/Users/x", "/UNC/host/share", "/", "a/b", "/nonexistent")] p: &str,
    ) {
        assert_eq!(i(p), p);
    }

    #[test]
    fn native_names_come_back() {
        assert_eq!(n("/C:/Users/x"), r"C:\Users\x");
        assert_eq!(n("/C:/"), r"C:\");
        assert_eq!(n("/C:"), r"C:\");
        assert_eq!(n("/UNC/host/share/x"), r"\\host\share\x");
        assert_eq!(n("a/b"), r"a\b");
        assert_eq!(n("/nonexistent/x"), r"\nonexistent\x");
        assert_eq!(n("/"), r"\");
    }

    #[test]
    fn a_leading_double_slash_is_not_a_share() {
        assert_eq!(n("//x"), r"\x");
        assert_eq!(n("///C:/x"), r"C:\x");
        assert_eq!(n("//UNC/host/share"), r"\\host\share");
    }

    #[rstest]
    fn a_round_trip_is_stable(
        #[values(
            r"C:\Users\x",
            r"C:\",
            r"\\host\share\a\b",
            r"a\b",
            r"..\a",
            r"\\.\pipe\x"
        )]
        p: &str,
    ) {
        let once = to_internal(p.as_bytes());
        assert_eq!(to_internal(&to_native(&once)), once);
    }

    #[test]
    fn dot_dot_stops_at_the_anchor() {
        let a = |p: &[&str]| anchor_len(&p.iter().map(|c| c.as_bytes()).collect::<Vec<_>>());
        assert_eq!(a(&["C:", "x"]), 1);
        assert_eq!(a(&["UNC", "host", "share", "x"]), 3);
        assert_eq!(a(&["UNC", "host"]), 2);
        assert_eq!(a(&["usr", "C:"]), 0);
        assert_eq!(a(&[]), 0);
    }

    #[cfg(windows)]
    #[test]
    fn canon_path_keeps_the_drive() {
        assert_eq!(canon_path(b"/C:/a/../../.."), b"/C:");
        assert_eq!(
            canon_path(b"/UNC/host/share/a/../../.."),
            b"/UNC/host/share"
        );
    }
}
