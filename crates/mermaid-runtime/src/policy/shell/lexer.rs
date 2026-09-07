//! A POSIX-shell lexer, self-contained and with no policy knowledge.
//!
//! Split out because it has none: it depends on nothing above it, and it is
//! what decides where one command ends and the next begins. Heredocs, command
//! substitution and quote-blind splitting are the subtle parts, and their tests
//! are the ones with real regression value.

/// One heredoc's body text, captured by [`split_command`] so body lines never
/// masquerade as command segments (`cat <<'EOF'` followed by prose used to
/// classify every prose line as an unknown command head — the worst-segment
/// rule then denied a read-only command).
pub(crate) struct HeredocBody {
    pub(crate) body: String,
    /// Bare delimiter (`<<EOF`): the shell expands `$(…)`/backticks in the
    /// body, so the classifier must scan it. Quoted or escaped delimiter
    /// (`<<'EOF'`, `<<"EOF"`, `<<\EOF`): the body is literal data.
    pub(crate) expands: bool,
}

/// The segments `sh -c` would run, plus the heredoc bodies those segments
/// consumed. Returned as one value on purpose: a caller that looks only at
/// `segments` silently loses every command carried in a heredoc, which is
/// exactly how the reverse-shell hard block and the `Allow`-override anchor
/// were bypassed. There is deliberately no `segments`-only helper.
pub(crate) struct SplitCommand {
    pub(crate) segments: Vec<String>,
    pub(crate) heredocs: Vec<HeredocBody>,
}

/// A heredoc redirection queued by the scanner until its body starts at the
/// next unquoted newline; `body` accumulates that heredoc's data lines.
pub(crate) struct PendingHeredoc {
    pub(crate) delimiter: String,
    /// `<<-`: leading tabs are stripped from body lines and the terminator.
    pub(crate) strip_tabs: bool,
    pub(crate) expands: bool,
    pub(crate) body: String,
}

/// Parse a heredoc operator at `chars[i..]` (`i` points at the first `<`):
/// push the operator text into `current` (the tokens stay in the segment —
/// they are inert in `classify_segment`), queue the pending heredoc, and
/// return the index after the delimiter word. Shell semantics for the
/// delimiter: ANY quoting or escaping anywhere in the word (`<<'EOF'`,
/// `<<E'O'F`, `<<\EOF`) disables body expansion, and the quotes themselves
/// are not part of the delimiter.
pub(crate) fn scan_heredoc_operator(
    chars: &[char],
    mut i: usize,
    current: &mut String,
    pending: &mut std::collections::VecDeque<PendingHeredoc>,
) -> usize {
    current.push_str("<<");
    i += 2;
    let mut strip_tabs = false;
    if chars.get(i) == Some(&'-') {
        strip_tabs = true;
        current.push('-');
        i += 1;
    }
    while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
        current.push(chars[i]);
        i += 1;
    }
    let mut delimiter = String::new();
    let mut quoted = false;
    while let Some(&c) = chars.get(i) {
        match c {
            '\'' | '"' => {
                quoted = true;
                current.push(c);
                i += 1;
                while let Some(&d) = chars.get(i) {
                    current.push(d);
                    i += 1;
                    if d == c {
                        break;
                    }
                    delimiter.push(d);
                }
            },
            '\\' => {
                quoted = true;
                current.push(c);
                i += 1;
                if let Some(&d) = chars.get(i) {
                    current.push(d);
                    delimiter.push(d);
                    i += 1;
                }
            },
            c if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>') => break,
            _ => {
                current.push(c);
                delimiter.push(c);
                i += 1;
            },
        }
    }
    // Fail closed: only treat this as a heredoc when the body can actually
    // terminate. See [`heredoc_terminates`].
    if !delimiter.is_empty() && heredoc_terminates(chars, i, &delimiter, strip_tabs) {
        pending.push_back(PendingHeredoc {
            delimiter,
            strip_tabs,
            expands: !quoted,
            body: String::new(),
        });
    }
    i
}

/// Does `delimiter` appear as a standalone terminator line in `chars[from..]`?
///
/// This is a NECESSARY condition for the heredoc to terminate, and it is what
/// makes phantom heredocs fail closed. An unquoted `<<` that is not really a
/// heredoc operator — deprecated `$[1<<2]` arithmetic, a `<<` inside a
/// comment, an exotic quoting shape the scanner misreads — produces a
/// delimiter that never appears on its own line (`2]`), so the operator stays
/// ordinary text and the lines after it remain REAL segments instead of being
/// swallowed as inert data. That swallowing was a read-only/plan-mode bypass:
/// `echo $[1<<2]\ngit push origin main` classified as `ReadOnly`.
///
/// A genuinely unterminated heredoc is refused by the same rule. The shell
/// would read its body to EOF, so this is stricter than the shell — but
/// classifying that text as commands is the safe direction, and a command
/// whose heredoc never closes is malformed anyway.
///
/// A false positive (the delimiter line exists but belongs to an earlier
/// heredoc's body) only keeps the normal heredoc path, so this can tighten
/// classification but never loosen it.
pub(crate) fn heredoc_terminates(
    chars: &[char],
    from: usize,
    delimiter: &str,
    strip_tabs: bool,
) -> bool {
    let mut i = from;
    while i < chars.len() {
        let (line, next) = read_line(chars, i);
        let compare = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if compare == delimiter {
            return true;
        }
        i = next;
    }
    false
}

/// The line starting at `chars[i]` (up to, excluding, the next `\n`) and the
/// index just past that newline (or `chars.len()` at EOF).
pub(crate) fn read_line(chars: &[char], i: usize) -> (String, usize) {
    let mut j = i;
    while j < chars.len() && chars[j] != '\n' {
        j += 1;
    }
    let line: String = chars[i..j].iter().collect();
    (line, (j + 1).min(chars.len()))
}

/// One substitution the shell would expand: `$(…)`, backtick `` `…` ``,
/// `<(…)`/`>(…)`, and the arithmetic forms `$((…))` and deprecated `$[…]`.
pub(crate) struct Substitution {
    /// The whole span INCLUDING its delimiters. Heredoc detection is
    /// suppressed inside these: `echo $((1<<2))` must not misfire a phantom
    /// heredoc and swallow the lines after it as "body" (a hidden `git push`
    /// line would then classify as data — a downgrade hole).
    pub(crate) outer: std::ops::Range<usize>,
    /// The body span EXCLUDING its delimiters — the command text callers
    /// re-classify under bounded recursion.
    pub(crate) inner: std::ops::Range<usize>,
}

/// The one quote/escape-aware walk behind BOTH [`substitution_spans`] and
/// [`extract_substitutions`]. Deliberately a single function: one caller
/// decides where heredoc detection is suppressed and the other decides what
/// gets re-classified, so any drift between two copies of this walk is a
/// downgrade hole. (They were two near-identical copies; #F-review.)
///
/// `quote_blind` disables single-quote skipping for heredoc bodies, which have
/// no shell quoting context — inside an expanding `<<EOF`, `'$(git push)'`
/// still executes. Backslash escaping is honored either way.
pub(crate) fn scan_substitutions(chars: &[char], quote_blind: bool) -> Vec<Substitution> {
    /// Scan a bracketed body from `open` (index of the opening delimiter),
    /// returning the index of the matching close (or `chars.len()`).
    fn close_of(chars: &[char], open: usize, opener: char, closer: char) -> usize {
        let mut depth = 1u32;
        let mut j = open + 1;
        while j < chars.len() {
            if chars[j] == opener {
                depth += 1;
            } else if chars[j] == closer {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            j += 1;
        }
        j
    }

    let mut out = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' if !quote_blind => {
                in_single = true;
                i += 1;
            },
            '\\' => i += 2, // skip the escaped char
            '`' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '`' {
                    if chars[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                out.push(Substitution {
                    outer: i..(j + 1).min(chars.len()),
                    inner: (i + 1).min(chars.len())..j.min(chars.len()),
                });
                i = j + 1;
            },
            '$' | '<' | '>' if chars.get(i + 1) == Some(&'(') => {
                // Covers `$((…))` arithmetic for free: the inner body is the
                // parenthesized expression, which the caller re-classifies.
                let j = close_of(chars, i + 1, '(', ')');
                out.push(Substitution {
                    outer: i..(j + 1).min(chars.len()),
                    inner: (i + 2).min(chars.len())..j.min(chars.len()),
                });
                i = j + 1;
            },
            // Deprecated arithmetic `$[expr]`. Without this the `<<` in
            // `echo $[1<<2]` reads as a heredoc operator and swallows every
            // following line as inert data (a read-only bypass).
            '$' if chars.get(i + 1) == Some(&'[') => {
                let j = close_of(chars, i + 1, '[', ']');
                out.push(Substitution {
                    outer: i..(j + 1).min(chars.len()),
                    inner: (i + 2).min(chars.len())..j.min(chars.len()),
                });
                i = j + 1;
            },
            _ => i += 1,
        }
    }
    out
}

/// Char ranges of every unquoted substitution span — the positions where
/// heredoc detection must be suppressed. See [`scan_substitutions`].
pub(crate) fn substitution_spans(chars: &[char]) -> Vec<std::ops::Range<usize>> {
    scan_substitutions(chars, false)
        .into_iter()
        .map(|s| s.outer)
        .collect()
}

/// Split `command` into the segments `sh -c` would run AND capture heredoc
/// bodies as data. The scanner semantics match the old `split_into_segments`
/// exactly (quotes, escapes, glued operators, redirect `&` forms); the one
/// addition is heredoc awareness. Note the backstop that keeps this safe even
/// where parsing is imperfect: `contains_destructive_pattern` runs on the RAW
/// command text before any segmentation, so a destructive command inside any
/// heredoc body — quoted, unterminated, or otherwise — still hard-denies.
#[expect(
    clippy::too_many_lines,
    reason = "a hand-rolled shell scanner: one loop whose arms are the quote states and operator \
     characters, all sharing the cursor, quote flags, and heredoc queue; the heredoc invariants \
     (bodies consumed to EOF, whole SplitCommand returned) are audited by reading the loop in one \
     piece, which cutting it into per-arm helpers would undo"
)]
pub(crate) fn split_command(command: &str) -> SplitCommand {
    fn flush(segments: &mut Vec<String>, current: &mut String) {
        let seg = current.trim();
        if !seg.is_empty() {
            segments.push(seg.to_string());
        }
        current.clear();
    }

    let chars: Vec<char> = command.chars().collect();
    let subst_spans = substitution_spans(&chars);
    let in_subst = |i: usize| subst_spans.iter().any(|r| r.contains(&i));

    let mut segments = Vec::new();
    let mut heredocs = Vec::new();
    let mut pending: std::collections::VecDeque<PendingHeredoc> = std::collections::VecDeque::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            current.push(c);
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            current.push(c);
            if c == '\\' {
                if let Some(&n) = chars.get(i + 1) {
                    current.push(n);
                    i += 1;
                }
            } else if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                current.push(c);
                i += 1;
            },
            '"' => {
                in_double = true;
                current.push(c);
                i += 1;
            },
            '\\' => {
                current.push(c);
                if let Some(&n) = chars.get(i + 1) {
                    current.push(n);
                    i += 1;
                }
                i += 1;
            },
            '<' if chars.get(i + 1) == Some(&'<') && !in_subst(i) => {
                if chars.get(i + 2) == Some(&'<') {
                    // `<<<` here-string: single-line, no body to consume, and
                    // `redirect_target_after` never treats it as a write
                    // (it only strips `>` prefixes). Pass through as text.
                    current.push_str("<<<");
                    i += 3;
                } else {
                    i = scan_heredoc_operator(&chars, i, &mut current, &mut pending);
                }
            },
            // An unquoted `#` starting a word begins a comment the shell never
            // executes — and a `<<` inside one must not start a heredoc. Skip
            // to (not past) the newline so the newline arm still runs.
            '#' if current.is_empty() || current.ends_with(char::is_whitespace) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            },
            ';' => {
                flush(&mut segments, &mut current);
                i += 1;
            },
            '\n' => {
                flush(&mut segments, &mut current);
                i += 1;
                // Body lines belong to the queued heredocs, in order — they
                // are DATA, never segments. An unterminated heredoc consumes
                // to EOF (shell read-to-end semantics); the raw destructive
                // scan already covered whatever the swallowed text says.
                while !pending.is_empty() {
                    if i >= chars.len() {
                        while let Some(h) = pending.pop_front() {
                            heredocs.push(HeredocBody {
                                body: h.body,
                                expands: h.expands,
                            });
                        }
                        break;
                    }
                    let (line, next) = read_line(&chars, i);
                    i = next;
                    let h = pending.front_mut().expect("checked non-empty");
                    let compare = if h.strip_tabs {
                        line.trim_start_matches('\t')
                    } else {
                        line.as_str()
                    };
                    if compare == h.delimiter {
                        let done = pending.pop_front().expect("checked non-empty");
                        heredocs.push(HeredocBody {
                            body: done.body,
                            expands: done.expands,
                        });
                    } else {
                        h.body.push_str(compare);
                        h.body.push('\n');
                    }
                }
            },
            '|' => {
                flush(&mut segments, &mut current);
                i += 1;
                if matches!(chars.get(i), Some('|') | Some('&')) {
                    i += 1;
                }
            },
            '&' => {
                // `>&`, `&>`, `2>&1` are redirects, not command separators.
                if current.trim_end().ends_with('>') || chars.get(i + 1) == Some(&'>') {
                    current.push(c);
                } else {
                    flush(&mut segments, &mut current);
                    if chars.get(i + 1) == Some(&'&') {
                        i += 1;
                    }
                }
                i += 1;
            },
            _ => {
                current.push(c);
                i += 1;
            },
        }
    }
    flush(&mut segments, &mut current);
    // Heredocs still pending at EOF never saw a newline (e.g. `cat <<EOF`
    // alone): empty bodies.
    for h in pending {
        heredocs.push(HeredocBody {
            body: h.body,
            expands: h.expands,
        });
    }
    SplitCommand { segments, heredocs }
}

/// Maximum depth for recursively classifying command/process substitution
/// bodies, so deeply nested `$( $( … ) )` can't drive unbounded recursion.
pub(crate) const MAX_SUBST_DEPTH: u8 = 4;

/// Extract the inner command text of every *unquoted* command/process
/// substitution in `command`: `$(…)`, backtick `` `…` ``, and `<(…)` / `>(…)`.
/// The shell executes these as commands, so the classifier and the destructive
/// hard-deny must see them too — `echo $(rm -rf ~)` is really `rm -rf ~`, not a
/// benign `echo` (#F1). Single-quoted regions are skipped (there the shell
/// treats `$(`/backticks literally); double-quoted regions are NOT (a
/// substitution inside double quotes is still expanded). Nested parens are
/// tracked so the body of `$(a $(b))` is captured whole and re-scanned by the
/// caller's bounded recursion.
pub(crate) fn extract_substitutions(command: &str) -> Vec<String> {
    extract_substitutions_inner(command, false)
}

/// [`extract_substitutions`] with single-quote skipping disabled. Heredoc
/// bodies have no shell quoting context — inside an expanding (`<<EOF`)
/// heredoc, a `'$(git push)'` still executes the substitution, so the
/// quote-aware walk would be a masking hole there. Backslash escaping stays:
/// `\$(…)` genuinely suppresses expansion in a heredoc body.
pub(crate) fn extract_substitutions_quote_blind(command: &str) -> Vec<String> {
    extract_substitutions_inner(command, true)
}

pub(crate) fn extract_substitutions_inner(command: &str, quote_blind: bool) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    scan_substitutions(&chars, quote_blind)
        .into_iter()
        .map(|s| chars[s.inner].iter().collect())
        .collect()
}

/// Lexically collapse `.`/`..` in a POSIX-style path so an interior `..` can't
/// disguise a catastrophic root: `/etc/../etc` resolves to `/etc` (#F3). No
/// filesystem access — this is the obfuscation-defeating companion to the
/// trailing-slash/glob stripping in [`is_dangerous_root`].
pub(crate) fn collapse_parent_refs(p: &str) -> String {
    let absolute = p.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for comp in p.split('/') {
        match comp {
            "" | "." => {},
            ".." => {
                if stack.is_empty() || matches!(stack.last(), Some(&"..")) {
                    // For an absolute path, `..` at root stays at root (the shell
                    // can't go above `/`), so drop it — otherwise `/etc/../../..`
                    // would leave a stray `..` and dodge the root check. Relative
                    // paths keep the leading `..` (it's meaningful).
                    if !absolute {
                        stack.push("..");
                    }
                } else {
                    stack.pop();
                }
            },
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

pub(crate) fn tokenize(command: &str) -> Vec<String> {
    shell_words::split(command)
        .unwrap_or_else(|_| command.split_whitespace().map(str::to_string).collect())
}

pub(crate) fn basename(arg: &str) -> &str {
    arg.rsplit(['/', '\\']).next().unwrap_or(arg)
}
