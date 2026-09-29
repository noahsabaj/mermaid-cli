use crate::entry::{Entry, Kind};

/// Parse one line: `<date> <kind> <amount> [memo...]`.
///
/// Blank lines and lines starting with `#` are not entries.
pub fn parse_line(line: &str) -> Result<Option<Entry>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut parts = line.splitn(4, char::is_whitespace);
    let date = parts.next().ok_or("missing date")?;
    let kind = match parts.next().ok_or("missing kind")? {
        "deposit" => Kind::Deposit,
        "withdrawal" => Kind::Withdrawal,
        other => return Err(format!("unknown kind `{other}`")),
    };
    let amount = parse_amount(parts.next().ok_or("missing amount")?)?;
    let memo = parts.next().unwrap_or("").trim().to_string();
    Ok(Some(Entry {
        date: date.to_string(),
        kind,
        amount,
        memo,
    }))
}

/// A non-negative amount such as `12`, `12.5` or `12.34`.
pub fn parse_amount(text: &str) -> Result<f64, String> {
    let amount: f64 = text
        .parse()
        .map_err(|_| format!("bad amount `{text}`"))?;
    if amount < 0.0 {
        return Err(format!("negative amount `{text}`"));
    }
    Ok(amount)
}

/// Parse a whole ledger. Errors name the 1-based line.
pub fn parse_ledger(text: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        match parse_line(line) {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(why) => return Err(format!("line {}: {why}", index + 1)),
        }
    }
    Ok(entries)
}
