use crate::entry::{Entry, Kind};

/// Sums per kind.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub deposits: f64,
    pub withdrawals: f64,
}

pub fn totals(entries: &[Entry]) -> Totals {
    let mut totals = Totals::default();
    for entry in entries {
        match entry.kind {
            Kind::Deposit => totals.deposits += entry.amount,
            Kind::Withdrawal => totals.withdrawals += entry.amount,
        }
    }
    totals
}

/// What is left: every deposit minus every withdrawal.
pub fn balance(entries: &[Entry]) -> f64 {
    let mut balance = 0.0;
    for entry in entries {
        match entry.kind {
            Kind::Deposit => balance += entry.amount,
            Kind::Withdrawal => balance -= entry.amount,
        }
    }
    balance
}
