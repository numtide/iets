//! The URL-literal lint Nix has (`lint-url-literals`), with Nix's messages.
//! Hooks: the parser notes a URL literal; the compiler reports it.

pub fn url_literal(text: &[u8], error: bool) -> String {
    let text = String::from_utf8_lossy(text);
    if error {
        format!("URL literals are disallowed. Consider using a string literal \"{text}\" instead")
    } else {
        format!("URL literals are discouraged. Consider using a string literal \"{text}\" instead")
    }
}
