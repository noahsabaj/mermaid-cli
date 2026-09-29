use std::collections::BTreeMap;

use crate::item::Item;

/// Total value per category, in cents, alphabetical by category.
pub fn by_category(items: &[Item]) -> BTreeMap<String, u64> {
    let mut totals = BTreeMap::new();
    for item in items {
        *totals.entry(item.category.clone()).or_insert(0) += item.value_cents();
    }
    totals
}

fn dollars(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// The report `stock <file>` prints.
pub fn render(items: &[Item]) -> String {
    let mut out = String::new();
    let mut total = 0;
    for (category, cents) in by_category(items) {
        out.push_str(&format!("{category}: {}\n", dollars(cents)));
        total += cents;
    }
    out.push_str(&format!("Total: {}\n", dollars(total)));
    out
}
