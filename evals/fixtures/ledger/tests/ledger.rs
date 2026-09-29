use ledger::entry::Kind;
use ledger::parse::{parse_ledger, parse_line};
use ledger::report::render;

#[test]
fn parses_a_line() {
    let entry = parse_line("2026-01-02 deposit 12.50 paycheck, part one")
        .unwrap()
        .unwrap();
    assert_eq!(entry.date, "2026-01-02");
    assert_eq!(entry.kind, Kind::Deposit);
    assert_eq!(entry.memo, "paycheck, part one");
}

#[test]
fn skips_comments_and_blank_lines() {
    assert_eq!(parse_line("# a comment").unwrap(), None);
    assert_eq!(parse_line("   ").unwrap(), None);
}

#[test]
fn errors_name_the_line() {
    let err = parse_ledger("2026-01-01 deposit 1\n2026-01-02 gift 5\n").unwrap_err();
    assert!(err.starts_with("line 2:"), "{err}");
}

#[test]
fn rejects_bad_amounts() {
    assert!(parse_line("2026-01-01 deposit twelve").is_err());
    assert!(parse_line("2026-01-01 deposit -3").is_err());
}

#[test]
fn renders_a_summary() {
    let entries = parse_ledger(
        "2026-01-01 deposit 100 start\n2026-01-02 withdrawal 20.50 lunch\n",
    )
    .unwrap();
    assert_eq!(
        render(&entries),
        "Entries: 2\nDeposits: 100.00\nWithdrawals: 20.50\nBalance: 79.50\n"
    );
}
