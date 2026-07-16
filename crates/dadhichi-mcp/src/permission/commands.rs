//! Shell-command analysis for permission matching.
//!
//! Permission rules for the `Bash` class match against a command *string*, but
//! a real command can chain several operations (`ls && rm -rf /`). To evaluate
//! `deny`/`ask` rules fairly we split a command into segments and check each
//! one, mirroring the documented behaviour: a single denied segment rejects the
//! whole command, while `allow` rules match only the command as a whole.
//!
//! Two fixed lists back the built-in auto-approvals and the always-prompt
//! safeguard. Neither is a security boundary — they are conveniences layered on
//! top of the grant and rule checks.

/// Split a command into simple segments on `&&`, `||`, `;`, `|`, and newlines.
///
/// Returns `None` when the command contains constructs this splitter cannot
/// safely reason about — command substitution `$(…)`, backticks, subshells,
/// backgrounding `&`, or redirection — signalling that the command must be
/// treated as a single opaque unit (and prompt) rather than approved segment by
/// segment.
pub fn split_segments(command: &str) -> Option<Vec<String>> {
    if command.contains("$(")
        || command.contains('`')
        || command.contains('(')
        || command.contains('>')
        || command.contains('<')
    {
        return None;
    }
    // A lone '&' (backgrounding) is unsafe; '&&' is a normal separator.
    if has_lone_amp(command) {
        return None;
    }

    let mut segments = Vec::new();
    let mut current = String::new();
    let bytes: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let two = if i + 1 < bytes.len() {
            Some((c, bytes[i + 1]))
        } else {
            None
        };
        match two {
            Some(('&', '&')) | Some(('|', '|')) => {
                push_segment(&mut segments, &mut current);
                i += 2;
                continue;
            }
            _ => {}
        }
        if c == ';' || c == '|' || c == '\n' {
            push_segment(&mut segments, &mut current);
            i += 1;
            continue;
        }
        current.push(c);
        i += 1;
    }
    push_segment(&mut segments, &mut current);
    Some(segments)
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let seg = current.trim().to_string();
    if !seg.is_empty() {
        segments.push(seg);
    }
    current.clear();
}

fn has_lone_amp(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = chars.get(i + 1).copied();
            if prev != Some('&') && next != Some('&') {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Environment-assignment and wrapper prefixes peeled before inspecting a
/// segment's primary command, so `RUST_LOG=debug timeout 5 rm x` is recognised
/// as an `rm`.
const WRAPPERS: &[&str] = &["timeout", "nice", "ionice", "chrt", "stdbuf", "env"];

/// The primary command word of a segment, after stripping leading `VAR=value`
/// assignments and a fixed set of process wrappers. Returns `""` for an empty
/// segment.
pub fn primary_command(segment: &str) -> &str {
    let mut rest = segment.trim();
    loop {
        let mut words = rest.splitn(2, char::is_whitespace);
        let first = words.next().unwrap_or("");
        // Strip a leading environment assignment (WORD=...).
        if is_env_assignment(first) {
            rest = words.next().unwrap_or("").trim_start();
            continue;
        }
        // Peel a known wrapper and any of its own flag/numeric arguments
        // (e.g. `timeout 5`, `nice -n 10`) so the wrapped command surfaces.
        if WRAPPERS.contains(&first) {
            if let Some(tail) = words.next() {
                let mut tail = tail.trim_start();
                loop {
                    let mut tw = tail.splitn(2, char::is_whitespace);
                    let w = tw.next().unwrap_or("");
                    let is_flag_or_number = w.starts_with('-')
                        || (!w.is_empty() && w.chars().all(|c| c.is_ascii_digit()));
                    if is_flag_or_number {
                        tail = tw.next().unwrap_or("").trim_start();
                    } else {
                        break;
                    }
                }
                rest = tail;
                continue;
            }
        }
        return first;
    }
}

fn is_env_assignment(word: &str) -> bool {
    match word.find('=') {
        Some(eq) if eq > 0 => word[..eq]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        _ => false,
    }
}

/// Commands that always prompt even when a remembered grant or the read-only
/// list would otherwise cover them. Matched on the primary command word, plus
/// the two-word special case `git push`.
pub fn is_dangerous(segment: &str) -> bool {
    const DANGEROUS: &[&str] = &[
        "rm", "chmod", "chown", "chgrp", "chattr", "pkill", "kill", "killall",
    ];
    let primary = primary_command(segment);
    if DANGEROUS.contains(&primary) {
        return true;
    }
    // `git push` is dangerous; other git subcommands are not.
    primary == "git" && second_word(segment) == Some("push")
}

/// Whether a segment's primary command is a recognised read-only operation.
/// Word-boundary matched via `primary_command`, so `ls` never matches `lsof`.
pub fn is_read_only_command(segment: &str) -> bool {
    const READ_ONLY: &[&str] = &[
        // Filesystem viewing.
        "ls", "cat", "pwd", "date", "whoami", "hostname", "uptime", "ps", "head", "tail", "wc",
        "sort", "uniq", "tr", "cut", // Search / inspection.
        "grep", "rg",
    ];
    let primary = primary_command(segment);
    if READ_ONLY.contains(&primary) {
        return true;
    }
    match primary {
        "git" => matches!(
            second_word(segment),
            Some("status" | "branch" | "log" | "diff" | "show" | "ls-files" | "rev-parse")
        ),
        "cargo" => second_word(segment) == Some("check"),
        "kubectl" => matches!(second_word(segment), Some("get" | "logs" | "describe")),
        _ => false,
    }
}

/// The second whitespace-delimited word of a segment, after wrapper/env peeling.
fn second_word(segment: &str) -> Option<&str> {
    // Re-derive the primary boundary, then read the next word.
    let primary = primary_command(segment);
    let idx = segment.find(primary)? + primary.len();
    segment[idx..].split_whitespace().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_all_separators() {
        assert_eq!(
            split_segments("ls && cd x || echo hi ; pwd | wc").unwrap(),
            vec!["ls", "cd x", "echo hi", "pwd", "wc"]
        );
    }

    #[test]
    fn refuses_unsafe_constructs() {
        assert!(split_segments("echo $(whoami)").is_none());
        assert!(split_segments("echo `id`").is_none());
        assert!(split_segments("cat x > y").is_none());
        assert!(split_segments("sleep 1 &").is_none());
        assert!(split_segments("(cd x && ls)").is_none());
    }

    #[test]
    fn primary_peels_env_and_wrappers() {
        assert_eq!(primary_command("RUST_LOG=debug cargo test"), "cargo");
        assert_eq!(primary_command("timeout 5 rm -rf x"), "rm");
        assert_eq!(primary_command("env FOO=1 nice ls"), "ls");
        assert_eq!(primary_command("ls -la"), "ls");
    }

    #[test]
    fn dangerous_list() {
        assert!(is_dangerous("rm -rf /"));
        assert!(is_dangerous("timeout 5 rm x"));
        assert!(is_dangerous("git push origin main"));
        assert!(!is_dangerous("git status"));
        assert!(!is_dangerous("ls"));
    }

    #[test]
    fn read_only_word_boundary() {
        assert!(is_read_only_command("ls -la"));
        assert!(!is_read_only_command("lsof")); // not a false prefix match
        assert!(is_read_only_command("git status"));
        assert!(!is_read_only_command("git push"));
        assert!(is_read_only_command("cargo check"));
        assert!(!is_read_only_command("cargo build"));
    }
}
