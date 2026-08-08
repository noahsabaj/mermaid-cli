//! Shared interactive-confirmation gate for destructive / untrusted actions.
//!
//! One fail-closed y/N primitive, reused by `mermaid update` (#110), `mermaid
//! restore` (#113), and the MCP untrusted-package gate (#10) so the
//! TTY-detection + `--yes`/`--force` policy lives in exactly one place.

use std::io::{self, IsTerminal, Write};

use anyhow::{Result, bail};

/// Whether `s` is `y`/`yes` (case-insensitive). Default is NO — anything else,
/// including empty input / EOF, is declined.
#[must_use]
pub fn is_affirmative(s: &str) -> bool {
    s.eq_ignore_ascii_case("y") || s.eq_ignore_ascii_case("yes")
}

/// Fail-closed policy: refuse a destructive/untrusted action when there is no
/// interactive terminal to confirm at and the explicit opt-in was not passed.
/// Also defeats `yes | mermaid …`, since a pipe is not a TTY.
#[must_use]
pub fn should_refuse_noninteractive(is_tty: bool, assume_yes: bool) -> bool {
    !is_tty && !assume_yes
}

/// Confirm a destructive action (default NO). `assume_yes` (a `--yes`/`--force`
/// flag) is the explicit opt-in for scripted use; without it a non-interactive
/// session refuses rather than proceeding silently. `prompt` is printed
/// verbatim, followed by ` [y/N]: `.
pub fn confirm_or_refuse(prompt: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if should_refuse_noninteractive(io::stdin().is_terminal(), assume_yes) {
        bail!(
            "Refusing to proceed without confirmation in a non-interactive session. \
             Re-run in an interactive terminal, or pass --force/--yes to allow it."
        );
    }
    print!("{prompt} [y/N]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(is_affirmative(input.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affirmative_accepts_only_y_yes() {
        assert!(is_affirmative("y"));
        assert!(is_affirmative("Y"));
        assert!(is_affirmative("yes"));
        assert!(is_affirmative("YES"));
        assert!(!is_affirmative("n"));
        assert!(!is_affirmative(""));
        assert!(!is_affirmative("sure"));
    }

    #[test]
    fn refuses_noninteractive_without_optin() {
        // No TTY + no --yes → refuse (fail closed).
        assert!(should_refuse_noninteractive(false, false));
        // --yes overrides the missing TTY.
        assert!(!should_refuse_noninteractive(false, true));
        // A real TTY is fine either way.
        assert!(!should_refuse_noninteractive(true, false));
    }

    #[test]
    fn confirm_returns_true_immediately_when_assume_yes() {
        // assume_yes short-circuits before any stdin/TTY interaction.
        assert!(confirm_or_refuse("do the thing?", true).expect("assume_yes is infallible"));
    }
}
