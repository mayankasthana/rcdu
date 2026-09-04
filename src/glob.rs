//! Minimal shell-style glob matching for `--exclude` patterns, in the spirit of `fnmatch`.
//!
//! Supports `*` (any run, including empty), `?` (one char), and `[...]` character classes with
//! ranges and `!`/`^` negation. Patterns are matched against an entry's basename, which covers
//! the common cases: `node_modules`, `.git`, `*.log`, `*cache*`.

/// Returns true if `name` matches the glob `pattern`.
pub fn matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    rec(&p, &s)
}

fn rec(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => {
            // Match zero chars here, or consume one char and keep the '*'.
            rec(&p[1..], s) || (!s.is_empty() && rec(p, &s[1..]))
        }
        Some('?') => !s.is_empty() && rec(&p[1..], &s[1..]),
        Some('[') => {
            if s.is_empty() {
                return false;
            }
            let (matched, rest) = match_class(p, s[0]);
            matched && rec(rest, &s[1..])
        }
        Some(&c) => !s.is_empty() && s[0] == c && rec(&p[1..], &s[1..]),
    }
}

/// Parse a `[...]` class starting at `p[0] == '['`, testing it against `c`.
/// Returns whether it matched and the slice following the closing `]`.
fn match_class(p: &[char], c: char) -> (bool, &[char]) {
    let mut i = 1;
    let mut negate = false;
    if matches!(p.get(i), Some('!') | Some('^')) {
        negate = true;
        i += 1;
    }
    let class_start = i;
    let mut matched = false;
    // A ']' immediately after the (optional) negation is a literal.
    while i < p.len() && (p[i] != ']' || i == class_start) {
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if c >= p[i] && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    let rest = if i < p.len() { &p[i + 1..] } else { &p[i..] };
    (matched ^ negate, rest)
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn basics() {
        assert!(matches("node_modules", "node_modules"));
        assert!(!matches("node_modules", "node_module"));
        assert!(matches("*.log", "error.log"));
        assert!(matches("*.log", ".log"));
        assert!(!matches("*.log", "log.txt"));
        assert!(matches("*cache*", "my_cache_dir"));
        assert!(matches("file?.txt", "file1.txt"));
        assert!(!matches("file?.txt", "file12.txt"));
        assert!(matches("*", "anything"));
        assert!(matches("[abc]at", "cat"));
        assert!(!matches("[abc]at", "rat"));
        assert!(matches("[a-z]*", "hello"));
        assert!(matches("[!0-9]*", "abc"));
        assert!(!matches("[!0-9]*", "9abc"));
    }
}
