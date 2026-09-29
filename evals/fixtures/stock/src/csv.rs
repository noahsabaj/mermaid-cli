//! Just enough CSV for the shop's exports.

/// Split one record into its fields, trimming surrounding whitespace.
pub fn split_record(line: &str) -> Vec<String> {
    line.split(',').map(|field| field.trim().to_string()).collect()
}

/// Every non-blank line of `text` as a record.
pub fn records(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(split_record)
        .collect()
}
