//! Quoting of words for shell command lines and scripts.

/// POSIX single-quote escaping for the use as a single word: wraps in
/// single quotes and escapes any embedded single quotes with the '\''
/// sequence.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Remove the shell quoting of a single word, the inverse of `quote`. Also
/// supports double quotes and backslash escapes.
pub(crate) fn unquote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => out.extend(chars.by_ref().take_while(|&c| c != '\'')),
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => out.extend(chars.next()),
                        c => out.push(c),
                    }
                }
            }
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_single_quotes() {
        assert_eq!(quote("/nix/bin/crun"), "'/nix/bin/crun'");
        assert_eq!(quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    #[test]
    fn unquote_inverts_quote() {
        for s in [
            "",
            "plain",
            "a b",
            "it's",
            "'''",
            "\"quoted\"",
            "back\\slash",
        ] {
            assert_eq!(unquote(&quote(s)), s);
        }
        assert_eq!(unquote(r#""a \"b\"""#), r#"a "b""#);
        assert_eq!(unquote(r"a\ b"), "a b");
    }
}
