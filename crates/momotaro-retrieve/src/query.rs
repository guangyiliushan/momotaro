//! Query string normalization and escaping.

/// Characters with syntactic meaning in tantivy query syntax.
const ESCAPE_CHARS: &[char] = &[
    '+', '-', '=', '>', '<', '!', '(', ')', '{', '}', '[', ']', '^', '"', '~', '*', '?', ':', '\\',
    '/',
];

/// Prefixes each query-syntax character in `raw` with a backslash so the
/// string is parsed as literal text.
pub fn escape_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if ESCAPE_CHARS.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Trims `raw` and collapses internal whitespace runs to single spaces.
pub fn normalize_query(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_prefixes_every_special_char() {
        assert_eq!(escape_query(r#"a+b-c"d"#), r#"a\+b\-c\"d"#);
        assert_eq!(escape_query("x:y"), r#"x\:y"#);
        assert_eq!(escape_query(r"back\slash"), r#"back\\slash"#);
        assert_eq!(escape_query("plain"), "plain");
        assert_eq!(escape_query(""), "");
    }

    #[test]
    fn escape_keeps_cjk_and_letters() {
        assert_eq!(escape_query("傅里叶"), "傅里叶");
        assert_eq!(escape_query("bert 模型"), "bert 模型");
    }

    #[test]
    fn normalize_trims_and_collapses() {
        assert_eq!(normalize_query("  hello   world  "), "hello world");
        assert_eq!(normalize_query("single"), "single");
        assert_eq!(normalize_query(""), "");
        assert_eq!(normalize_query(" \t\n "), "");
    }
}
