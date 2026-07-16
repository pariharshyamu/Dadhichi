//! A small, dependency-free glob matcher for permission patterns.
//!
//! The `dadhichi-mcp` crate is deliberately dependency-light, so rather than
//! pull in `globset` we implement exactly the gitignore-style subset the
//! permission rules document — no more, no less, so the semantics are pinned
//! and testable:
//!
//! - `*`  — any run of characters. Stops at `/` unless `cross_slash` is set.
//! - `?`  — exactly one character (never `/` unless `cross_slash`).
//! - `**` — any run of characters *including* `/` (spans path segments).
//! - `[abc]` / `[a-z]` — a character class; a leading `!` or `^` negates it.
//!
//! `cross_slash` distinguishes the two rule flavours: **path** globs
//! (`Read(src/**)`) keep `*` within a segment, while **command / name** globs
//! (`Bash(git *)`, `MCPTool(server.*)`) let `*` match anything, slashes and
//! spaces included.

/// One parsed glob token.
enum Tok {
    Lit(char),
    Any,       // ?
    Star,      // *
    DoubleStar, // ** (not followed by '/')
    // `**/` — zero or more whole path segments, matching zero directories too
    // (so `**/.env` matches `.env` as well as `sub/.env`).
    DoubleStarSlash,
    Class { negated: bool, items: Vec<ClassItem> },
}

enum ClassItem {
    Ch(char),
    Range(char, char),
}

fn tokenize(pattern: &str) -> Vec<Tok> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    // `**/` is a single "zero or more segments" token; a bare
                    // `**` (end of pattern or `a/**`) crosses slashes greedily.
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        toks.push(Tok::DoubleStarSlash);
                        i += 3;
                    } else {
                        toks.push(Tok::DoubleStar);
                        i += 2;
                    }
                } else {
                    toks.push(Tok::Star);
                    i += 1;
                }
            }
            '?' => {
                toks.push(Tok::Any);
                i += 1;
            }
            '[' => {
                // Parse a character class up to the closing ']'. If it never
                // closes, treat the '[' as a literal (lenient, fail-safe).
                if let Some((item, next)) = parse_class(&chars, i) {
                    toks.push(item);
                    i = next;
                } else {
                    toks.push(Tok::Lit('['));
                    i += 1;
                }
            }
            c => {
                toks.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    toks
}

fn parse_class(chars: &[char], start: usize) -> Option<(Tok, usize)> {
    // chars[start] == '['
    let mut i = start + 1;
    let negated = matches!(chars.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    while i < chars.len() {
        match chars[i] {
            ']' => return Some((Tok::Class { negated, items }, i + 1)),
            c => {
                // A range like a-z: current char, '-', end char (end != ']').
                if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
                    items.push(ClassItem::Range(c, chars[i + 2]));
                    i += 3;
                } else {
                    items.push(ClassItem::Ch(c));
                    i += 1;
                }
            }
        }
    }
    None // unterminated
}

fn class_matches(items: &[ClassItem], negated: bool, c: char) -> bool {
    let hit = items.iter().any(|item| match item {
        ClassItem::Ch(x) => *x == c,
        ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
    });
    hit != negated
}

/// Whether `text` matches the glob `pattern`. See the module docs for the
/// supported syntax and the meaning of `cross_slash`.
pub fn glob_match(pattern: &str, text: &str, cross_slash: bool) -> bool {
    let toks = tokenize(pattern);
    let chars: Vec<char> = text.chars().collect();
    matches(&toks, &chars, cross_slash)
}

fn matches(toks: &[Tok], text: &[char], cross: bool) -> bool {
    let Some((head, rest)) = toks.split_first() else {
        // Pattern exhausted: match iff text is also exhausted.
        return text.is_empty();
    };

    match head {
        Tok::Lit(c) => !text.is_empty() && text[0] == *c && matches(rest, &text[1..], cross),
        Tok::Any => {
            !text.is_empty() && (cross || text[0] != '/') && matches(rest, &text[1..], cross)
        }
        Tok::Class { negated, items } => {
            !text.is_empty()
                && (cross || text[0] != '/')
                && class_matches(items, *negated, text[0])
                && matches(rest, &text[1..], cross)
        }
        Tok::Star => star(rest, text, cross, cross),
        // `**` always crosses '/', regardless of the pattern flavour.
        Tok::DoubleStar => star(rest, text, cross, true),
        // `**/` matches zero or more whole segments: try here (zero dirs), then
        // advance past each following '/' and retry.
        Tok::DoubleStarSlash => {
            let mut i = 0;
            loop {
                if matches(rest, &text[i..], cross) {
                    return true;
                }
                match text[i..].iter().position(|&c| c == '/') {
                    Some(rel) => i += rel + 1,
                    None => return false,
                }
            }
        }
    }
}

/// Match a `*`/`**`: try consuming 0, 1, 2, … characters, stopping the run at a
/// '/' when `run_crosses` is false. `cross` is threaded through to the tail.
fn star(rest: &[Tok], text: &[char], cross: bool, run_crosses: bool) -> bool {
    let mut i = 0;
    loop {
        if matches(rest, &text[i..], cross) {
            return true;
        }
        if i == text.len() {
            return false;
        }
        if !run_crosses && text[i] == '/' {
            return false;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn star_stops_at_slash_for_paths() {
        assert!(glob_match("src/*", "src/main.rs", false));
        assert!(!glob_match("src/*", "src/nested/mod.rs", false));
    }

    #[test]
    fn double_star_crosses_slashes() {
        assert!(glob_match("src/**", "src/nested/mod.rs", false));
        assert!(glob_match("**/.env", ".env", false));
        assert!(glob_match("**/.env", "sub/dir/.env", false));
    }

    #[test]
    fn command_star_crosses_everything() {
        assert!(glob_match("git *", "git commit -m x", true));
        assert!(glob_match("git * main", "git checkout main", true));
        assert!(glob_match("rm -rf *", "rm -rf /", true));
    }

    #[test]
    fn question_matches_one_char() {
        assert!(glob_match("a?c", "abc", false));
        assert!(!glob_match("a?c", "ac", false));
        assert!(!glob_match("a?c", "a/c", false)); // ? doesn't cross '/'
    }

    #[test]
    fn char_classes_and_negation() {
        assert!(glob_match("[abc]", "b", false));
        assert!(!glob_match("[abc]", "d", false));
        assert!(glob_match("[a-z]", "m", false));
        assert!(glob_match("[!a]", "b", false));
        assert!(!glob_match("[!a]", "a", false));
        assert!(glob_match("[^a]", "b", false)); // ^ negates like !
    }

    #[test]
    fn extensions() {
        assert!(glob_match("**/*.pem", "certs/key.pem", false));
        assert!(glob_match("*.rs", "main.rs", false));
        assert!(!glob_match("*.rs", "main.py", false));
    }

    #[test]
    fn literal_bracket_when_unterminated() {
        // An unterminated '[' is treated literally rather than failing.
        assert!(glob_match("a[b", "a[b", false));
    }
}
