/// What an entry does to the balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Deposit,
    Withdrawal,
}

/// One line of the ledger.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// `YYYY-MM-DD`, kept as written.
    pub date: String,
    pub kind: Kind,
    /// Always positive; `kind` says which way it moves the balance.
    pub amount: f64,
    pub memo: String,
}
