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

    /// `to_internal`: a drive or share becomes the first component.
    #[rstest]
    #[case::drive(r"C:\Users\x", "/C:/Users/x")]
    #[case::drive_slash("C:/Users/x", "/C:/Users/x")]
    #[case::drive_lowercase(r"c:\Users", "/C:/Users")]
    #[case::drive_root(r"C:\", "/C:/")]
    #[case::bare_drive("C:", "C:")]
    #[case::drive_relative("C:foo", "C:foo")]
    #[case::mixed_separators(
        r"C:\Users\RUNNER~1\AppData/Local\Temp/repo/read.nix",
        "/C:/Users/RUNNER~1/AppData/Local/Temp/repo/read.nix"
    )]
    #[case::share(r"\\host\share\x", "/UNC/host/share/x")]
    #[case::verbatim_share(r"\\?\UNC\host\share\x", "/UNC/host/share/x")]
    #[case::verbatim_share_lowercase(r"\\?\unc\host\share\x", "/UNC/host/share/x")]
    #[case::verbatim_drive(r"\\?\C:\x", "/C:/x")]
    #[case::share_forward_slashes("//host/share/x", "/UNC/host/share/x")]
    #[case::share_mixed_slashes(r"/\host\share", "/UNC/host/share")]
    #[case::relative_unc(r"UNC\x", "UNC/x")]
    #[case::relative_unc_lowercase(r"unc\x", "unc/x")]
    #[case::relative(r"a\b\c", "a/b/c")]
    #[case::relative_up(r"..\a", "../a")]
    #[case::rooted(r"\nonexistent\x", "/nonexistent/x")]
    #[case::not_a_drive("ab:/x", "ab:/x")]
    #[case::device(r"\\.\pipe\x", "//./pipe/x")]
    #[case::volume(r"\\?\Volume{1}\x", "//?/Volume{1}/x")]
    fn to_internal_form(#[case] p: &str, #[case] want: &str) {
        assert_eq!(i(p), want);
    }

    #[rstest]
    fn the_evaluators_form_is_left_alone(
        #[values("/C:/Users/x", "/UNC/host/share", "/", "a/b", "/nonexistent")] p: &str,
    ) {
        assert_eq!(i(p), p);
    }

    /// `to_native`: the inverse.
    #[rstest]
    #[case::drive("/C:/Users/x", r"C:\Users\x")]
    #[case::drive_root("/C:/", r"C:\")]
    #[case::drive_alone("/C:", r"C:\")]
    #[case::bare_drive("C:", "C:")]
    #[case::share("/UNC/host/share/x", r"\\host\share\x")]
    #[case::relative("a/b", r"a\b")]
    #[case::rooted("/nonexistent/x", r"\nonexistent\x")]
    #[case::root("/", r"\")]
    #[case::device("//./pipe/x", r"\\.\pipe\x")]
    #[case::volume("//?/Volume{1}/x", r"\\?\Volume{1}\x")]
    #[case::double_slash("//x", r"\x")]
    #[case::double_slash_drive("///C:/x", r"C:\x")]
    #[case::double_slash_unc("//UNC/host/share", r"\\host\share")]
    fn to_native_form(#[case] p: &str, #[case] want: &str) {
        assert_eq!(n(p), want);
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
