//! Internal utility functions shared across recon-search backends.

/// Escape a literal string so it is safe to use as a regex pattern.
///
/// Backslash-prefixes all regex metacharacters: `\.*+?()[]{}|^$`
pub(crate) fn regex_escape(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len() + 8);
    for c in pattern.chars() {
        if r"\.*+?()[]{}|^$".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_plain_text() {
        assert_eq!(regex_escape("hello"), "hello");
    }

    #[test]
    fn escape_dot() {
        assert_eq!(regex_escape("foo.bar"), r"foo\.bar");
    }

    #[test]
    fn escape_star() {
        assert_eq!(regex_escape("a*b"), r"a\*b");
    }

    #[test]
    fn escape_parens() {
        assert_eq!(regex_escape("fn(x)"), r"fn\(x\)");
    }

    #[test]
    fn escape_caret_dollar() {
        assert_eq!(regex_escape("^start$"), r"\^start\$");
    }

    #[test]
    fn escape_plus_question() {
        assert_eq!(regex_escape("a+b?"), r"a\+b\?");
    }

    #[test]
    fn escape_all_metacharacters() {
        let meta = r"\.*+?()[]{}|^$";
        let escaped = regex_escape(meta);
        // Every metachar must be escaped — no bare metachar should remain
        assert!(escaped.contains(r"\\"));
        assert!(escaped.contains(r"\*"));
        assert!(escaped.contains(r"\^"));
    }

    #[test]
    fn empty_string() {
        assert_eq!(regex_escape(""), "");
    }
}
