//! Pure surgical search-and-replace editing engine.
//!
//! Provides high-precision string and block replacements with graduated
//! fuzzy matching (exact -> trailing whitespace -> full trim -> unicode normalization),
//! strict uniqueness validation, and multiline replacement.
//!
//! Reused by both the live `edit_file` tool and the approval replay path.

use crate::apply_patch::{AppliedFile, SeekHit, normalise};

/// Errors encountered when performing a search-and-replace edit.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplaceError {
    /// The target text was not found in the file.
    TargetNotFound,
    /// The target text matched multiple locations, and `allow_multiple` was false.
    AmbiguousMatch { count: usize },
    /// Target content was empty.
    EmptyTarget,
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotFound => write!(f, "could not find target_content in file"),
            Self::AmbiguousMatch { count } => write!(
                f,
                "target_content found {count} times; provide more surrounding context to disambiguate, or set allow_multiple=true"
            ),
            Self::EmptyTarget => write!(f, "target_content cannot be empty"),
        }
    }
}

/// Find all match locations for `pattern` in `lines` at the highest possible strictness level.
fn find_all_matches(lines: &[String], pattern: &[String]) -> (Vec<SeekHit>, bool) {
    if pattern.is_empty() || pattern.len() > lines.len() {
        return (Vec::new(), false);
    }
    let last = lines.len().saturating_sub(pattern.len());

    // Pass 1: exact match
    let mut exact_matches = Vec::new();
    let mut i = 0;
    while i <= last {
        if lines[i..i + pattern.len()] == *pattern {
            exact_matches.push(SeekHit {
                index: i,
                exact: true,
            });
            i += pattern.len();
        } else {
            i += 1;
        }
    }
    if !exact_matches.is_empty() {
        return (exact_matches, false);
    }

    // Pass 2: trailing whitespace insensitive
    let mut trim_end_matches = Vec::new();
    let mut i = 0;
    while i <= last {
        if pattern
            .iter()
            .enumerate()
            .all(|(p, pat)| lines[i + p].trim_end() == pat.trim_end())
        {
            trim_end_matches.push(SeekHit {
                index: i,
                exact: false,
            });
            i += pattern.len();
        } else {
            i += 1;
        }
    }
    if !trim_end_matches.is_empty() {
        return (trim_end_matches, true);
    }

    // Pass 3: full trim
    let mut trim_matches = Vec::new();
    let mut i = 0;
    while i <= last {
        if pattern
            .iter()
            .enumerate()
            .all(|(p, pat)| lines[i + p].trim() == pat.trim())
        {
            trim_matches.push(SeekHit {
                index: i,
                exact: false,
            });
            i += pattern.len();
        } else {
            i += 1;
        }
    }
    if !trim_matches.is_empty() {
        return (trim_matches, true);
    }

    // Pass 4: unicode normalized
    let mut norm_matches = Vec::new();
    let mut i = 0;
    while i <= last {
        if pattern
            .iter()
            .enumerate()
            .all(|(p, pat)| normalise(&lines[i + p]) == normalise(pat))
        {
            norm_matches.push(SeekHit {
                index: i,
                exact: false,
            });
            i += pattern.len();
        } else {
            i += 1;
        }
    }
    (norm_matches, true)
}

/// Replace occurrences of `target` with `replacement` in `original`.
///
/// # Errors
///
/// Returns [`ReplaceError::EmptyTarget`] if `target` is empty,
/// [`ReplaceError::TargetNotFound`] if no matches exist under exact or fuzzy rules,
/// or [`ReplaceError::AmbiguousMatch`] if multiple matches exist and `allow_multiple` is `false`.
pub fn replace_content(
    original: &str,
    target: &str,
    replacement: &str,
    allow_multiple: bool,
) -> Result<AppliedFile, ReplaceError> {
    if target.is_empty() {
        return Err(ReplaceError::EmptyTarget);
    }

    let has_crlf = original.contains("\r\n");
    let line_sep = if has_crlf { "\r\n" } else { "\n" };

    let mut original_lines: Vec<String> = original
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let mut target_lines: Vec<String> = target
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if target_lines.last().is_some_and(String::is_empty) {
        target_lines.pop();
    }

    let mut repl_lines: Vec<String> = replacement
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if repl_lines.last().is_some_and(String::is_empty) {
        repl_lines.pop();
    }

    let (matches, fuzzy) = find_all_matches(&original_lines, &target_lines);

    if matches.is_empty() {
        return Err(ReplaceError::TargetNotFound);
    }

    if matches.len() > 1 && !allow_multiple {
        return Err(ReplaceError::AmbiguousMatch {
            count: matches.len(),
        });
    }

    let target_len = target_lines.len();
    for hit in matches.iter().rev() {
        for _ in 0..target_len {
            if hit.index < original_lines.len() {
                original_lines.remove(hit.index);
            }
        }
        for (offset, new_line) in repl_lines.iter().enumerate() {
            original_lines.insert(hit.index + offset, new_line.clone());
        }
    }

    if !original_lines.last().is_some_and(String::is_empty) {
        original_lines.push(String::new());
    }

    Ok(AppliedFile {
        new_contents: original_lines.join(line_sep),
        fuzzy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_single_replacement() {
        let original = "fn main() {\n    old_code();\n}\n";
        let res = replace_content(original, "    old_code();", "    new_code();", false).unwrap();
        assert_eq!(res.new_contents, "fn main() {\n    new_code();\n}\n");
        assert!(!res.fuzzy);
    }

    #[test]
    fn multiline_replacement() {
        let original = "start\nline 1\nline 2\nend\n";
        let res = replace_content(original, "line 1\nline 2", "replaced lines", false).unwrap();
        assert_eq!(res.new_contents, "start\nreplaced lines\nend\n");
    }

    #[test]
    fn fuzzy_whitespace_replacement() {
        let original = "start\n    indent_code();   \nend\n";
        let res =
            replace_content(original, "indent_code();", "    updated_code();", false).unwrap();
        assert_eq!(res.new_contents, "start\n    updated_code();\nend\n");
        assert!(res.fuzzy);
    }

    #[test]
    fn unicode_normalized_replacement() {
        let original = "let s = \u{201C}hello\u{201D};\n";
        let res =
            replace_content(original, "let s = \"hello\";", "let s = \"world\";", false).unwrap();
        assert_eq!(res.new_contents, "let s = \"world\";\n");
        assert!(res.fuzzy);
    }

    #[test]
    fn target_not_found_errors() {
        let original = "alpha\nbeta\n";
        assert_eq!(
            replace_content(original, "gamma", "delta", false),
            Err(ReplaceError::TargetNotFound)
        );
    }

    #[test]
    fn ambiguous_match_errors_unless_allowed() {
        let original = "dup\nline\ndup\n";
        assert_eq!(
            replace_content(original, "dup", "unique", false),
            Err(ReplaceError::AmbiguousMatch { count: 2 })
        );

        let res = replace_content(original, "dup", "unique", true).unwrap();
        assert_eq!(res.new_contents, "unique\nline\nunique\n");
    }

    #[test]
    fn empty_target_errors() {
        let original = "something\n";
        assert_eq!(
            replace_content(original, "", "replacement", false),
            Err(ReplaceError::EmptyTarget)
        );
    }
}
