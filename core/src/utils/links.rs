//! Links pairs send each other during a screen share: the viewer sends one,
//! the sharer's browser opens it.

/// Longest link accepted, generous for real URLs.
const MAX_LINK_LENGTH: usize = 2048;

/// The web link in `text`, if it is one: an http(s) URL without spaces or
/// control characters. Anything else is refused, so a pair can't open
/// files or other apps (`file:`, custom schemes) on the sharer's Mac.
pub fn shareable_link(text: &str) -> Option<String> {
    let link = text.trim();
    let lowercase = link.to_ascii_lowercase();
    let rest = lowercase
        .strip_prefix("https://")
        .or_else(|| lowercase.strip_prefix("http://"))?;
    let host_present = rest.chars().next().is_some_and(|c| c != '/');
    let clean = !link.chars().any(|c| c.is_whitespace() || c.is_control());
    (host_present && clean && link.len() <= MAX_LINK_LENGTH).then(|| link.to_string())
}

#[cfg(test)]
mod tests {
    use super::shareable_link;

    #[test]
    fn accepts_web_links() {
        assert_eq!(
            shareable_link("  https://github.com/360-geo/hopp/pull/1 \n"),
            Some("https://github.com/360-geo/hopp/pull/1".to_string())
        );
        assert_eq!(
            shareable_link("HTTP://localhost:3000/a?b=c#d"),
            Some("HTTP://localhost:3000/a?b=c#d".to_string())
        );
    }

    #[test]
    fn refuses_everything_else() {
        for text in [
            "",
            "github.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vscode://open?file=x",
            "https://",
            "https:///path",
            "https://example.com/a b",
            "https://example.com/\u{0}",
        ] {
            assert_eq!(shareable_link(text), None, "{text:?}");
        }
        assert_eq!(
            shareable_link(&format!("https://a.com/{}", "x".repeat(3000))),
            None
        );
    }
}
