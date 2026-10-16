use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Local, Timelike};

/// A parsed five-field Unix cron schedule using numeric fields only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: CronField,
    hour: CronField,
    day_of_month: CronField,
    month: CronField,
    day_of_week: CronField,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CronField {
    // Cron's largest field is minute (0..=59), so a u64 represents every
    // value without allocating a tree or doing a logarithmic lookup.
    values: u64,
    /// True when the source field begins with `*`, including stepped wildcards.
    wildcard: bool,
}

impl CronSchedule {
    /// Parse a strict, numeric five-field Unix cron expression.
    ///
    /// Supported field expressions are `*`, comma-separated values, ranges,
    /// and `/step` suffixes on any of those bases (for example `*/5`, `1-10/2`,
    /// or `5/10`). Weekday 0 and 7 both represent Sunday.
    pub fn parse(expression: &str) -> Result<Self> {
        let mut fields = expression.split_ascii_whitespace();
        let (Some(minute), Some(hour), Some(day_of_month), Some(month), Some(day_of_week)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            bail!("cron expression must contain exactly five fields");
        };
        if fields.next().is_some() {
            bail!("cron expression must contain exactly five fields");
        }
        Ok(Self {
            minute: CronField::parse(minute, 0, 59, false, "minute")?,
            hour: CronField::parse(hour, 0, 23, false, "hour")?,
            day_of_month: CronField::parse(day_of_month, 1, 31, false, "day of month")?,
            month: CronField::parse(month, 1, 12, false, "month")?,
            day_of_week: CronField::parse(day_of_week, 0, 7, true, "day of week")?,
        })
    }

    /// Return whether a local datetime matches this schedule.
    pub fn matches(&self, datetime: &DateTime<Local>) -> bool {
        if !self.minute.contains(datetime.minute())
            || !self.hour.contains(datetime.hour())
            || !self.month.contains(datetime.month())
        {
            return false;
        }
        let dom_matches = self.day_of_month.contains(datetime.day());
        let dow_matches = self
            .day_of_week
            .contains(datetime.weekday().num_days_from_sunday());

        // Cron ORs day-of-month and weekday only when both fields are restricted.
        // Otherwise both field predicates apply; this preserves stepped wildcards.
        if self.day_of_month.wildcard || self.day_of_week.wildcard {
            dom_matches && dow_matches
        } else {
            dom_matches || dow_matches
        }
    }
}

impl CronField {
    fn parse(raw: &str, min: u32, max: u32, sunday_alias: bool, label: &str) -> Result<Self> {
        if raw.is_empty() {
            bail!("cron {label} field is empty");
        }
        let wildcard = raw.starts_with('*');
        let mut values = 0u64;
        for item in raw.split(',') {
            if item.is_empty() {
                bail!("cron {label} field contains an empty list item");
            }
            let mut step_parts = item.split('/');
            let base = step_parts.next().unwrap();
            let step = match step_parts.next() {
                Some(step) => {
                    if step_parts.next().is_some() || step.is_empty() {
                        bail!("invalid step in cron {label} field item {item:?}");
                    }
                    let step = parse_number(step, label)?;
                    if step == 0 {
                        bail!("cron {label} step must be greater than zero");
                    }
                    step
                }
                None => 1,
            };

            let (start, end) = if base == "*" {
                (min, max)
            } else if let Some((from, to)) = base.split_once('-') {
                if to.contains('-') || from.is_empty() || to.is_empty() {
                    bail!("invalid range {base:?} in cron {label} field");
                }
                let from = parse_number(from, label)?;
                let to = parse_number(to, label)?;
                validate_bound(from, min, max, label)?;
                validate_bound(to, min, max, label)?;
                if from > to {
                    bail!("reversed range {base:?} in cron {label} field");
                }
                (from, to)
            } else {
                let value = parse_number(base, label)?;
                validate_bound(value, min, max, label)?;
                // A stepped single value expands from that value through the field maximum.
                (value, if item.contains('/') { max } else { value })
            };

            let mut value = start;
            while value <= end {
                let selected = if sunday_alias && value == 7 { 0 } else { value };
                values |= 1u64 << selected;
                match value.checked_add(step) {
                    Some(next) => value = next,
                    None => break,
                }
            }
        }
        if values == 0 {
            bail!("cron {label} field selects no values");
        }
        Ok(Self { values, wildcard })
    }

    fn contains(&self, value: u32) -> bool {
        value < u64::BITS && self.values & (1u64 << value) != 0
    }
}

fn parse_number(value: &str, label: &str) -> Result<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid number {value:?} in cron {label} field");
    }
    value
        .parse()
        .map_err(|_| anyhow!("number {value:?} is too large in cron {label} field"))
}

fn validate_bound(value: u32, min: u32, max: u32, label: &str) -> Result<()> {
    if !(min..=max).contains(&value) {
        bail!("cron {label} value {value} is outside {min}..={max}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("valid local timestamp")
    }

    #[test]
    fn matches_lists_ranges_steps_and_sunday_aliases() {
        let schedule = CronSchedule::parse("0,30 9-17/2 1-31/2 1,3,12 0,7").unwrap();
        assert!(schedule.matches(&local(2024, 1, 7, 9, 30))); // Sunday
        assert!(schedule.matches(&local(2024, 3, 3, 11, 0))); // Sunday, stepped hour
        assert!(!schedule.matches(&local(2024, 2, 4, 9, 0))); // Month not selected
        assert!(!schedule.matches(&local(2024, 1, 7, 10, 0))); // Minute not selected
    }

    #[test]
    fn matches_highest_minute_bit_and_both_sunday_spellings() {
        let sunday_zero = CronSchedule::parse("59 23 31 12 0").unwrap();
        let sunday_seven = CronSchedule::parse("59 23 31 12 7").unwrap();
        for schedule in [&sunday_zero, &sunday_seven] {
            assert!(schedule.matches(&local(2024, 12, 29, 23, 59))); // Sunday
            assert!(!schedule.matches(&local(2024, 12, 30, 23, 59)));
            assert!(schedule.matches(&local(2024, 12, 31, 23, 59))); // DOM matches
            assert!(!schedule.matches(&local(2024, 12, 29, 23, 58)));
        }
    }

    #[test]
    fn restricted_day_fields_use_or_and_wildcard_fields_keep_and_semantics() {
        let restricted = CronSchedule::parse("0 12 1 * 1").unwrap();
        assert!(restricted.matches(&local(2024, 1, 1, 12, 0))); // Both match
        assert!(restricted.matches(&local(2024, 1, 8, 12, 0))); // Monday only
        assert!(restricted.matches(&local(2024, 2, 1, 12, 0))); // First day only
        assert!(!restricted.matches(&local(2024, 1, 2, 12, 0)));

        let wildcard_steps = CronSchedule::parse("0 12 */2 * */2").unwrap();
        assert!(wildcard_steps.matches(&local(2024, 1, 7, 12, 0))); // DOM 7 and Sunday
        assert!(!wildcard_steps.matches(&local(2024, 1, 1, 12, 0))); // DOM only
        assert!(!wildcard_steps.matches(&local(2024, 1, 2, 12, 0))); // DOW only
    }

    #[test]
    fn rejects_bad_field_count_steps_bounds_and_ranges() {
        for expression in [
            "* * * *",
            "* * * * * *",
            "*/0 * * * *",
            "60 * * * *",
            "0 24 * * *",
            "0 0 0 * *",
            "0 0 * 13 *",
            "0 0 * * 8",
            "0 0 10-2 * *",
            "0 0 1--2 * *",
            "0 0 1,,2 * *",
            "0 0 1/ * *",
            "0 0 1/2/3 * *",
            "-1 0 * * *",
            "JAN * * * *",
        ] {
            assert!(
                CronSchedule::parse(expression).is_err(),
                "accepted {expression:?}"
            );
        }
    }
}
