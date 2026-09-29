use crate::csv::records;
use crate::item::Item;

/// The columns an export must have. Their order is whatever the header says.
const COLUMNS: [&str; 5] = ["sku", "name", "category", "quantity", "unit_price"];

/// Load every item from a CSV export with a header row.
pub fn load(text: &str) -> Result<Vec<Item>, String> {
    let mut rows = records(text).into_iter();
    let header = rows.next().ok_or("empty file")?;
    let index = |column: &str| {
        header
            .iter()
            .position(|h| h.eq_ignore_ascii_case(column))
            .ok_or(format!("no `{column}` column"))
    };
    let at: Vec<usize> = COLUMNS.iter().map(|c| index(c)).collect::<Result<_, _>>()?;
    let mut items = Vec::new();
    for (n, row) in rows.enumerate() {
        let field = |i: usize| row.get(at[i]).map(String::as_str).unwrap_or("");
        let quantity = field(3)
            .parse()
            .map_err(|_| format!("row {}: bad quantity `{}`", n + 1, field(3)))?;
        let unit_cents = parse_price(field(4))
            .ok_or(format!("row {}: bad unit_price `{}`", n + 1, field(4)))?;
        items.push(Item {
            sku: field(0).to_string(),
            name: field(1).to_string(),
            category: field(2).to_string(),
            quantity,
            unit_cents,
        });
    }
    Ok(items)
}

/// `4.20` or `4` as cents.
fn parse_price(text: &str) -> Option<u64> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, "0"));
    let frac = format!("{frac:0<2}");
    if frac.len() != 2 {
        return None;
    }
    Some(whole.parse::<u64>().ok()? * 100 + frac.parse::<u64>().ok()?)
}
