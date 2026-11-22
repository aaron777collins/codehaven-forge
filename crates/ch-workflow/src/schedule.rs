//! Cron schedules. Five fields, UTC, as in Actions.

use chrono::{DateTime, Utc};
use croner::Cron;

fn parse(expr: &str) -> Result<Cron, String> {
    let e = expr.trim();
    if e.split_whitespace().count() != 5 {
        return Err("a schedule has five fields: minute hour day-of-month month day-of-week".into());
    }
    Cron::new(e).parse().map_err(|err| err.to_string())
}

pub fn validate(expr: &str) -> Result<(), String> {
    parse(expr).map(|_| ())
}

/// The first time strictly after `after` that the schedule fires. `None` when the schedule
/// never fires again (for example 30 February).
pub fn next_after(expr: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let c = parse(expr).ok()?;
    c.find_next_occurrence(&after, false).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn next_fire_times() {
        assert_eq!(next_after("0 3 * * *", t(2026, 1, 1, 2, 0)), Some(t(2026, 1, 1, 3, 0)));
        assert_eq!(next_after("0 3 * * *", t(2026, 1, 1, 3, 0)), Some(t(2026, 1, 2, 3, 0)), "strictly after");
        assert_eq!(next_after("*/15 * * * *", t(2026, 1, 1, 0, 7)), Some(t(2026, 1, 1, 0, 15)));
        assert_eq!(next_after("30 6 * * 1", t(2026, 1, 1, 0, 0)), Some(t(2026, 1, 5, 6, 30)), "a Monday");
        assert_eq!(next_after("0 0 1 * *", t(2026, 1, 31, 12, 0)), Some(t(2026, 2, 1, 0, 0)));
    }

    #[test]
    fn validation() {
        assert!(validate("0 3 * * *").is_ok());
        assert!(validate("*/5 * * * 1-5").is_ok());
        assert!(validate("0 3 * *").is_err(), "four fields");
        assert!(validate("0 0 3 * * *").is_err(), "six fields: no seconds");
        assert!(validate("61 * * * *").is_err());
        assert!(validate("nonsense").is_err());
        assert!(validate("").is_err());
    }
}
