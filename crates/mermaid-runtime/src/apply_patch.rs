//! Pure `apply_patch` engine — parser, graduated fuzzy matcher, and applier —
//! adapted from OpenAI Codex's `apply-patch` crate.
//!
//! It lives in the runtime crate (rather than beside the tool) so BOTH the main
//! crate's `apply_patch` tool AND the approval-replay path can apply patches
//! without duplicating the engine. It is PURE: parsing and applying operate on
//! `&str`; all file I/O stays with the callers (via the confined pathguard).
//!
//! Trimmed from Codex: no Lark grammar, no `local_shell`/heredoc extraction, no
//! lenient GPT-4.1 mode, no streaming — the patch arrives as one string.

use std::path::PathBuf;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT: &str = "@@";

// ─── parser ─────────────────────────────────────────────────────────

/// A patch parse failure, rendered back to the model so it can fix the envelope.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The overall envelope is malformed (missing markers, no hunks, stray line).
    Invalid(String),
    /// A line inside an update/add hunk is malformed.
    InvalidHunk { message: String, line_number: usize },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid patch: {m}"),
            Self::InvalidHunk {
                message,
                line_number,
            } => write!(f, "invalid hunk at patch line {line_number}: {message}"),
        }
    }
}

/// One file operation in a patch.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Hunk {
    /// Create a new file with `contents`.
    AddFile { path: PathBuf, contents: String },
    /// Delete an existing file.
    DeleteFile { path: PathBuf },
    /// Edit (and optionally rename to `move_path`) an existing file.
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateFileChunk>,
    },
}

/// One contiguous edit region within an `UpdateFile` hunk. `old_lines` is the
/// span to locate (context + removed lines, in file order); `new_lines` is its
/// replacement (context + added lines).
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct UpdateFileChunk {
    /// Optional `@@` anchor line used to disambiguate the region's location.
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// The region must sit at the end of the file (tolerant of trailing newline).
    pub is_end_of_file: bool,
}

fn is_file_marker(line: &str) -> bool {
    line.starts_with(ADD_FILE) || line.starts_with(DELETE_FILE) || line.starts_with(UPDATE_FILE)
}

/// Parse a complete patch into its hunks. Strict: requires the Begin/End markers
/// (tolerating surrounding blank lines) and rejects stray lines.
pub fn parse_patch(patch: &str) -> Result<Vec<Hunk>, ParseError> {
    let raw: Vec<&str> = patch.lines().collect();
    let start = raw
        .iter()
        .position(|l| l.trim_end() == BEGIN_PATCH)
        .ok_or_else(|| ParseError::Invalid(format!("missing '{BEGIN_PATCH}' line")))?;
    let end_rel = raw[start + 1..]
        .iter()
        .position(|l| l.trim_end() == END_PATCH)
        .ok_or_else(|| ParseError::Invalid(format!("missing '{END_PATCH}' line")))?;
    let body = &raw[start + 1..start + 1 + end_rel];

    let mut hunks = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        if let Some(path) = line.strip_prefix(ADD_FILE) {
            let contents = parse_add_contents(body, &mut i)?;
            hunks.push(Hunk::AddFile {
                path: PathBuf::from(path.trim()),
                contents,
            });
        } else if let Some(path) = line.strip_prefix(DELETE_FILE) {
            hunks.push(Hunk::DeleteFile {
                path: PathBuf::from(path.trim()),
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE_FILE) {
            i += 1;
            let move_path = body
                .get(i)
                .and_then(|l| l.strip_prefix(MOVE_TO))
                .map(|dst| {
                    i += 1;
                    PathBuf::from(dst.trim())
                });
            let chunks = parse_update_chunks(body, &mut i)?;
            hunks.push(Hunk::UpdateFile {
                path: PathBuf::from(path.trim()),
                move_path,
                chunks,
            });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            return Err(ParseError::Invalid(format!(
                "unexpected line outside a hunk: {line:?}"
            )));
        }
    }
    if hunks.is_empty() {
        return Err(ParseError::Invalid("patch contains no hunks".to_string()));
    }
    Ok(hunks)
}

fn parse_add_contents(body: &[&str], i: &mut usize) -> Result<String, ParseError> {
    *i += 1;
    let mut lines: Vec<&str> = Vec::new();
    while *i < body.len() && !is_file_marker(body[*i]) {
        let l = body[*i];
        let content = l.strip_prefix('+').ok_or_else(|| ParseError::InvalidHunk {
            message: format!("expected a '+' line in an Add File hunk, got {l:?}"),
            line_number: *i,
        })?;
        lines.push(content);
        *i += 1;
    }
    Ok(lines.join("\n"))
}

fn parse_update_chunks(body: &[&str], i: &mut usize) -> Result<Vec<UpdateFileChunk>, ParseError> {
    let mut chunks: Vec<UpdateFileChunk> = Vec::new();
    let mut current: Option<UpdateFileChunk> = None;
    while *i < body.len() && !is_file_marker(body[*i]) {
        let l = body[*i];
        if l == EMPTY_CHANGE_CONTEXT || l.starts_with(CHANGE_CONTEXT) {
            if let Some(c) = current.take() {
                chunks.push(c);
            }
            current = Some(UpdateFileChunk {
                change_context: l.strip_prefix(CHANGE_CONTEXT).map(str::to_string),
                ..Default::default()
            });
        } else if l == EOF_MARKER {
            current
                .get_or_insert_with(UpdateFileChunk::default)
                .is_end_of_file = true;
        } else {
            let c = current.get_or_insert_with(UpdateFileChunk::default);
            match l.chars().next() {
                Some('+') => c.new_lines.push(l[1..].to_string()),
                Some('-') => c.old_lines.push(l[1..].to_string()),
                Some(' ') => {
                    let s = l[1..].to_string();
                    c.old_lines.push(s.clone());
                    c.new_lines.push(s);
                },
                None => {
                    c.old_lines.push(String::new());
                    c.new_lines.push(String::new());
                },
                _ => {
                    return Err(ParseError::InvalidHunk {
                        message: format!("unexpected line in an update hunk: {l:?}"),
                        line_number: *i,
                    });
                },
            }
        }
        *i += 1;
    }
    if let Some(c) = current.take() {
        chunks.push(c);
    }
    if chunks.is_empty() {
        return Err(ParseError::InvalidHunk {
            message: "update hunk has no changes".to_string(),
            line_number: *i,
        });
    }
    Ok(chunks)
}

// ─── matcher ────────────────────────────────────────────────────────

/// A located match: the starting line index, and whether it was byte-exact.
struct SeekHit {
    index: usize,
    exact: bool,
}

/// Find `pattern` within `lines` at or after `start`, in decreasing strictness:
/// exact, trailing-whitespace-insensitive, full-trim, then Unicode-normalized.
/// When `eof` is set, the search anchors at the end of the file first.
fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<SeekHit> {
    if pattern.is_empty() {
        return Some(SeekHit {
            index: start,
            exact: true,
        });
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof {
        lines.len() - pattern.len()
    } else {
        start
    };
    let last = lines.len().saturating_sub(pattern.len());

    for i in search_start..=last {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(SeekHit {
                index: i,
                exact: true,
            });
        }
    }
    for i in search_start..=last {
        if pattern
            .iter()
            .enumerate()
            .all(|(p, pat)| lines[i + p].trim_end() == pat.trim_end())
        {
            return Some(SeekHit {
                index: i,
                exact: false,
            });
        }
    }
    for i in search_start..=last {
        if pattern
            .iter()
            .enumerate()
            .all(|(p, pat)| lines[i + p].trim() == pat.trim())
        {
            return Some(SeekHit {
                index: i,
                exact: false,
            });
        }
    }
    for i in search_start..=last {
        if pattern
            .iter()
            .enumerate()
            .all(|(p, pat)| normalise(&lines[i + p]) == normalise(pat))
        {
            return Some(SeekHit {
                index: i,
                exact: false,
            });
        }
    }
    None
}

/// Normalize typographic punctuation and spaces to their ASCII equivalents.
fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

// ─── applier ────────────────────────────────────────────────────────

/// Why applying an update hunk failed. Rendered back to the model.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// A `@@` context anchor could not be located in the file.
    ContextNotFound(String),
    /// The `old_lines` span could not be located in the file.
    LinesNotFound(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContextNotFound(c) => write!(f, "could not find context line '{c}'"),
            Self::LinesNotFound(l) => write!(f, "could not find the lines to replace:\n{l}"),
        }
    }
}

/// The result of applying chunks: the new file text and whether any chunk had to
/// be located fuzzily (whitespace/Unicode-normalized rather than byte-exact).
pub struct AppliedFile {
    pub new_contents: String,
    pub fuzzy: bool,
}

/// Apply `chunks` to `original`, returning the new file contents.
pub fn derive_new_contents(
    original: &str,
    chunks: &[UpdateFileChunk],
) -> Result<AppliedFile, ApplyError> {
    let mut lines: Vec<String> = original.split('\n').map(String::from).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let (replacements, fuzzy) = compute_replacements(&lines, chunks)?;
    let mut new_lines = apply_replacements(lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(AppliedFile {
        new_contents: new_lines.join("\n"),
        fuzzy,
    })
}

type Replacement = (usize, usize, Vec<String>);

fn compute_replacements(
    original_lines: &[String],
    chunks: &[UpdateFileChunk],
) -> Result<(Vec<Replacement>, bool), ApplyError> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0usize;
    let mut fuzzy = false;

    for chunk in chunks {
        if let Some(ctx) = &chunk.change_context {
            match seek_sequence(original_lines, std::slice::from_ref(ctx), line_index, false) {
                Some(hit) => {
                    fuzzy |= !hit.exact;
                    line_index = hit.index + 1;
                },
                None => return Err(ApplyError::ContextNotFound(ctx.clone())),
            }
        }

        if chunk.old_lines.is_empty() {
            let idx = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        match found {
            Some(hit) => {
                fuzzy |= !hit.exact;
                replacements.push((hit.index, pattern.len(), new_slice.to_vec()));
                line_index = hit.index + pattern.len();
            },
            None => return Err(ApplyError::LinesNotFound(chunk.old_lines.join("\n"))),
        }
    }

    replacements.sort_by_key(|(i, _, _)| *i);
    Ok((replacements, fuzzy))
}

fn apply_replacements(mut lines: Vec<String>, replacements: &[Replacement]) -> Vec<String> {
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        let start_idx = *start_idx;
        for _ in 0..*old_len {
            if start_idx < lines.len() {
                lines.remove(start_idx);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(start_idx + offset, new_line.clone());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn chunk(ctx: Option<&str>, old: &[&str], new: &[&str], eof: bool) -> UpdateFileChunk {
        UpdateFileChunk {
            change_context: ctx.map(str::to_string),
            old_lines: v(old),
            new_lines: v(new),
            is_end_of_file: eof,
        }
    }

    #[test]
    fn parses_update_add_delete_move() {
        let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@ fn main()\n-    old();\n+    new();\n*** End Patch\n";
        assert_eq!(
            parse_patch(patch).unwrap(),
            vec![Hunk::UpdateFile {
                path: PathBuf::from("src/main.rs"),
                move_path: None,
                chunks: vec![chunk(
                    Some("fn main()"),
                    &["    old();"],
                    &["    new();"],
                    false
                )],
            }]
        );
        assert_eq!(
            parse_patch("*** Begin Patch\n*** Add File: a.txt\n+x\n+y\n*** End Patch").unwrap(),
            vec![Hunk::AddFile {
                path: PathBuf::from("a.txt"),
                contents: "x\ny".to_string(),
            }]
        );
        let mv =
            "*** Begin Patch\n*** Update File: old.rs\n*** Move to: new.rs\n-a\n+b\n*** End Patch";
        match &parse_patch(mv).unwrap()[0] {
            Hunk::UpdateFile { move_path, .. } => {
                assert_eq!(move_path.as_deref(), Some(std::path::Path::new("new.rs")))
            },
            other => panic!("expected UpdateFile, got {other:?}"),
        }
    }

    #[test]
    fn context_lines_go_to_both_sides_and_eof_flag() {
        match &parse_patch("*** Begin Patch\n*** Update File: x\n import foo\n+bar\n*** End Patch")
            .unwrap()[0]
        {
            Hunk::UpdateFile { chunks, .. } => {
                assert_eq!(chunks[0].old_lines, v(&["import foo"]));
                assert_eq!(chunks[0].new_lines, v(&["import foo", "bar"]));
            },
            other => panic!("expected UpdateFile, got {other:?}"),
        }
        match &parse_patch(
            "*** Begin Patch\n*** Update File: x\n+quux\n*** End of File\n*** End Patch",
        )
        .unwrap()[0]
        {
            Hunk::UpdateFile { chunks, .. } => assert!(chunks[0].is_end_of_file),
            other => panic!("expected UpdateFile, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_markers_and_empty() {
        assert!(parse_patch("no markers").is_err());
        assert!(parse_patch("*** Begin Patch\n*** End Patch").is_err());
    }

    #[test]
    fn seek_reports_exact_vs_fuzzy_and_eof() {
        assert!(
            seek_sequence(&v(&["foo", "bar"]), &v(&["bar"]), 0, false)
                .unwrap()
                .exact
        );
        let fuzzy = seek_sequence(&v(&["foo   "]), &v(&["foo"]), 0, false).unwrap();
        assert!(!fuzzy.exact);
        assert!(seek_sequence(&v(&["only"]), &v(&["a", "b"]), 0, false).is_none());
        assert_eq!(
            seek_sequence(&v(&["a", "x", "b", "x"]), &v(&["x"]), 0, true)
                .unwrap()
                .index,
            3
        );
    }

    #[test]
    fn applies_exact_fuzzy_anchor_and_eof() {
        assert_eq!(
            derive_new_contents("a\nold\nc\n", &[chunk(None, &["old"], &["new"], false)])
                .unwrap()
                .new_contents,
            "a\nnew\nc\n"
        );
        let f = derive_new_contents("a\nold   \nc\n", &[chunk(None, &["old"], &["new"], false)])
            .unwrap();
        assert_eq!(f.new_contents, "a\nnew\nc\n");
        assert!(f.fuzzy);
        // Anchor picks the SECOND identical block.
        let out = derive_new_contents(
            "fn a() {\n    x();\n}\nfn b() {\n    x();\n}\n",
            &[chunk(Some("fn b() {"), &["    x();"], &["    y();"], false)],
        )
        .unwrap();
        assert_eq!(
            out.new_contents,
            "fn a() {\n    x();\n}\nfn b() {\n    y();\n}\n"
        );
        assert_eq!(
            derive_new_contents("a\nb\n", &[chunk(None, &[], &["c"], true)])
                .unwrap()
                .new_contents,
            "a\nb\nc\n"
        );
    }

    #[test]
    fn apply_errors_on_missing_context_or_lines() {
        assert!(matches!(
            derive_new_contents("a\n", &[chunk(Some("nope"), &["a"], &["b"], false)]),
            Err(ApplyError::ContextNotFound(c)) if c == "nope"
        ));
        assert!(matches!(
            derive_new_contents("a\n", &[chunk(None, &["zzz"], &["b"], false)]),
            Err(ApplyError::LinesNotFound(_))
        ));
    }
}
