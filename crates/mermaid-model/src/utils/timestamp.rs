use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike};

/// Format a timestamp as a relative, human-readable string.
///
/// Examples:
/// - "Today at 6:41pm"
/// - "Yesterday at 9:26pm"
/// - "March 21st, 2025 at 8:16pm"
///
/// `today` is a parameter rather than a `Local::now()` read: which of those
/// three branches a timestamp takes is the whole output, so reading the clock
/// here would make the rendered string a function of the machine rather than
/// of the arguments. The caller in the render tree passes the date derived
/// from the injected `state.now` (see `ChatWidget::today`), which is what lets
/// a frame be reproduced — under `--replay`, or by the snapshot suite, or on
/// another machine — instead of being relabelled by whatever day it is now.
#[must_use]
pub fn format_relative_timestamp(timestamp: DateTime<Local>, today: NaiveDate) -> String {
    let msg_date = timestamp.date_naive();

    let time_str = format_time(timestamp);

    // Check if today
    if msg_date == today {
        return format!("Today at {time_str}");
    }

    // Check if yesterday
    let yesterday = today.pred_opt().unwrap_or(today);
    if msg_date == yesterday {
        return format!("Yesterday at {time_str}");
    }

    // Older dates: "March 21st, 2025 at 8:16pm"
    let date_str = format_date(msg_date);
    format!("{date_str} at {time_str}")
}

/// Format time as "6:41pm" (12-hour with AM/PM)
fn format_time(timestamp: DateTime<Local>) -> String {
    let hour = timestamp.hour();
    let minute = timestamp.minute();

    let (hour_12, period) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };

    format!("{hour_12:}:{minute:02}{period}")
}

/// Format date as "March 21st, 2025"
fn format_date(date: chrono::NaiveDate) -> String {
    let month = date.format("%B").to_string(); // Full month name

    // Use Datelike trait methods to get day and year
    let day = date.day();
    let year = date.year();

    let ordinal = match day {
        1 | 21 | 31 => "st",
        2 | 22 => "nd",
        3 | 23 => "rd",
        _ => "th",
    };

    format!("{month} {day}{ordinal}, {year}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn test_format_time() {
        // Create a timestamp for 6:41 PM
        let ts = Local::now().with_hour(18).unwrap().with_minute(41).unwrap();

        let time = format_time(ts);
        assert_eq!(time, "6:41pm");
    }

    #[test]
    fn test_format_time_am() {
        let ts = Local::now().with_hour(9).unwrap().with_minute(26).unwrap();

        let time = format_time(ts);
        assert_eq!(time, "9:26am");
    }

    #[test]
    fn test_format_date() {
        use chrono::NaiveDate;

        // March 21st
        let date = NaiveDate::from_ymd_opt(2025, 3, 21).unwrap();
        let formatted = format_date(date);
        assert_eq!(formatted, "March 21st, 2025");

        // March 22nd
        let date = NaiveDate::from_ymd_opt(2025, 3, 22).unwrap();
        let formatted = format_date(date);
        assert_eq!(formatted, "March 22nd, 2025");

        // March 23rd
        let date = NaiveDate::from_ymd_opt(2025, 3, 23).unwrap();
        let formatted = format_date(date);
        assert_eq!(formatted, "March 23rd, 2025");

        // March 24th
        let date = NaiveDate::from_ymd_opt(2025, 3, 24).unwrap();
        let formatted = format_date(date);
        assert_eq!(formatted, "March 24th, 2025");
    }

    #[test]
    fn test_format_relative_today() {
        let now = Local::now();
        let relative = format_relative_timestamp(now, now.date_naive());
        assert!(relative.starts_with("Today at"));
    }

    /// The branch is chosen against the passed date, not the machine's. Pinning
    /// both sides is the point: before `today` was a parameter these three
    /// assertions could not be written at all, because two of the three
    /// branches were unreachable from a test on any given day.
    #[test]
    fn the_branch_follows_the_passed_date_not_the_clock() {
        use chrono::TimeZone;
        let stamp = Local
            .with_ymd_and_hms(2026, 1, 2, 15, 4, 5)
            .earliest()
            .expect("fixture wall clock exists in the local timezone");
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).expect("real date");

        assert_eq!(
            format_relative_timestamp(stamp, day(2026, 1, 2)),
            "Today at 3:04pm"
        );
        assert_eq!(
            format_relative_timestamp(stamp, day(2026, 1, 3)),
            "Yesterday at 3:04pm"
        );
        assert_eq!(
            format_relative_timestamp(stamp, day(2026, 3, 21)),
            "January 2nd, 2026 at 3:04pm"
        );
    }
}
