//! Scratchpad containment: whether a command token provably stays inside the
//! session scratchpad.

use std::path::Path;

/// Whether one token of a command run inside the scratchpad provably stays
/// there. Rules, all fail-closed:
/// - `..` anywhere: rejected (can climb out of the scratch cwd).
/// - `:/` anywhere: rejected (URL / remote-host / list-of-paths shapes).
/// - Drive-designator shape (`C:x`, `c:\x`): rejected on every platform —
///   on Windows it targets a drive root or a per-drive cwd, never scratch.
/// - No path separator: fine — a bare word, flag, or PATH-resolved argv0.
/// - Rooted: must sit lexically inside the scratchpad. `has_root`, not
///   `is_absolute` — on Windows `/etc/passwd` is rooted but not "absolute"
///   (no drive prefix), yet still escapes the scratch cwd via the drive
///   root, so every rooted token gets the containment check.
/// - Relative with a separator: accepted only as a PLAIN path (no leading
///   `-`, no `=`) so flag-embedded paths (`-C/etc`, `--directory=/etc`,
///   `VAR=/etc`) can't smuggle a target past the rooted check.
#[must_use]
pub fn token_provably_in_scratch(token: &str, scratch: &Path) -> bool {
    if token.contains("..") || token.contains(":/") {
        return false;
    }
    let bytes = token.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return false;
    }
    if !token.contains(['/', '\\']) {
        return true;
    }
    if Path::new(token).has_root() {
        return Path::new(token).starts_with(scratch);
    }
    !token.starts_with('-') && !token.contains('=')
}
