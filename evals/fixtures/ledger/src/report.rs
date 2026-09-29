use crate::balance::{balance, totals};
use crate::entry::Entry;

/// A money amount as it appears in the summary.
pub fn money(amount: f64) -> String {
    format!("{amount:.2}")
}

/// The summary `ledger <file>` prints.
pub fn render(entries: &[Entry]) -> String {
    let totals = totals(entries);
    let mut out = String::new();
    out.push_str(&format!("Entries: {}\n", entries.len()));
    out.push_str(&format!("Deposits: {}\n", money(totals.deposits)));
    out.push_str(&format!("Withdrawals: {}\n", money(totals.withdrawals)));
    out.push_str(&format!("Balance: {}\n", money(balance(entries))));
    out
}
