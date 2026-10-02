//! Rule types and the decision function. No I/O: callers supply the rules, usage and time.
#![allow(dead_code)]

use std::collections::BTreeMap;

use chrono::{Days, Duration, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};

pub type CategoryId = i64;

/// An allowed stretch of a day, in minutes after local midnight. The end is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stretch {
    pub start_min: u16,
    pub end_min: u16,
}

/// The rule for one account on one weekday.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayRule {
    /// False: the computer is allowed all day. True: only during `stretches`.
    pub restricted: bool,
    #[serde(default)]
    pub stretches: Vec<Stretch>,
    /// Minutes per category. A category that isn't listed has no limit.
    #[serde(default)]
    pub budgets: BTreeMap<CategoryId, u32>,
}

/// A one-off blackout that applies to the account being decided, in local time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlackoutSpan {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Computer {
    Allowed,
    OutsideSchedule,
    Blackout { until: NaiveDateTime, note: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryStatus {
    pub category: CategoryId,
    pub used_secs: i64,
    pub left_secs: i64,
    pub used_up: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub computer: Computer,
    /// Budgeted categories only.
    pub categories: Vec<CategoryStatus>,
    /// When the clock alone could next change `computer`. Running out of budget isn't predicted.
    pub next_change: NaiveDateTime,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Invalid {
    pub field: &'static str,
    pub message: String,
}

fn invalid(field: &'static str, message: impl Into<String>) -> Invalid {
    Invalid {
        field,
        message: message.into(),
    }
}

pub fn validate_day(rule: &DayRule) -> Result<(), Invalid> {
    let mut sorted = rule.stretches.clone();
    sorted.sort_by_key(|s| s.start_min);
    for s in &sorted {
        if s.start_min >= s.end_min {
            return Err(invalid("stretches", "a stretch must end after it starts"));
        }
        if s.end_min > 1440 {
            return Err(invalid("stretches", "a stretch can't end after midnight"));
        }
    }
    if sorted.windows(2).any(|w| w[1].start_min < w[0].end_min) {
        return Err(invalid("stretches", "stretches must not overlap"));
    }
    if rule.budgets.values().any(|&minutes| minutes > 1440) {
        return Err(invalid("budgets", "a budget can't be more than 24 hours"));
    }
    Ok(())
}

pub fn validate_blackout(start: NaiveDateTime, end: NaiveDateTime) -> Result<(), Invalid> {
    if end <= start {
        return Err(invalid("end", "a blackout must end after it starts"));
    }
    Ok(())
}

pub fn decide(
    day: &DayRule,
    blackouts: &[BlackoutSpan],
    used_secs: &BTreeMap<CategoryId, i64>,
    now: NaiveDateTime,
) -> Decision {
    let midnight = now.date().and_time(NaiveTime::MIN);
    let minute = (now.time().num_seconds_from_midnight() / 60) as u16;

    let active = blackouts
        .iter()
        .filter(|b| b.start <= now && now < b.end)
        .max_by_key(|b| b.end);
    let in_stretch = !day.restricted
        || day
            .stretches
            .iter()
            .any(|s| s.start_min <= minute && minute < s.end_min);
    let computer = match active {
        Some(b) => Computer::Blackout {
            until: b.end,
            note: b.note.clone(),
        },
        None if !in_stretch => Computer::OutsideSchedule,
        None => Computer::Allowed,
    };

    // Tomorrow's rule takes over at midnight, so that is always a possible change
    let mut next_change = midnight + Days::new(1);
    let mut consider = |t: NaiveDateTime| {
        if t > now && t < next_change {
            next_change = t;
        }
    };
    for b in blackouts {
        consider(b.start);
        consider(b.end);
    }
    if day.restricted {
        for s in &day.stretches {
            consider(midnight + Duration::minutes(i64::from(s.start_min)));
            consider(midnight + Duration::minutes(i64::from(s.end_min)));
        }
    }

    let categories = day
        .budgets
        .iter()
        .map(|(&category, &minutes)| {
            let used = used_secs.get(&category).copied().unwrap_or(0);
            let left_secs = (i64::from(minutes) * 60 - used).max(0);
            CategoryStatus {
                category,
                used_secs: used,
                left_secs,
                used_up: left_secs == 0,
            }
        })
        .collect();

    Decision {
        computer,
        categories,
        next_change,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 5)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    fn day(stretches: &[(u16, u16)]) -> DayRule {
        DayRule {
            restricted: true,
            stretches: stretches
                .iter()
                .map(|&(start_min, end_min)| Stretch { start_min, end_min })
                .collect(),
            budgets: BTreeMap::new(),
        }
    }

    fn none() -> BTreeMap<CategoryId, i64> {
        BTreeMap::new()
    }

    #[test]
    fn unrestricted_day_is_allowed_until_midnight() {
        let d = decide(&DayRule::default(), &[], &none(), at(3, 0));
        assert_eq!(d.computer, Computer::Allowed);
        assert_eq!(d.next_change, at(0, 0) + Days::new(1));
        assert!(d.categories.is_empty());
    }

    #[test]
    fn stretch_start_is_inclusive_and_end_is_exclusive() {
        let rule = day(&[(375, 1200)]); // 6:15am to 8:00pm
        assert_eq!(
            decide(&rule, &[], &none(), at(6, 14)).computer,
            Computer::OutsideSchedule
        );
        assert_eq!(
            decide(&rule, &[], &none(), at(6, 15)).computer,
            Computer::Allowed
        );
        assert_eq!(
            decide(&rule, &[], &none(), at(19, 59)).computer,
            Computer::Allowed
        );
        assert_eq!(
            decide(&rule, &[], &none(), at(20, 0)).computer,
            Computer::OutsideSchedule
        );
    }

    #[test]
    fn next_change_is_the_nearest_stretch_edge() {
        let rule = day(&[(375, 450), (960, 1200)]); // 6:15–7:30, 16:00–20:00
        assert_eq!(decide(&rule, &[], &none(), at(5, 0)).next_change, at(6, 15));
        assert_eq!(decide(&rule, &[], &none(), at(7, 0)).next_change, at(7, 30));
        assert_eq!(
            decide(&rule, &[], &none(), at(12, 0)).next_change,
            at(16, 0)
        );
        assert_eq!(
            decide(&rule, &[], &none(), at(21, 0)).next_change,
            at(0, 0) + Days::new(1)
        );
    }

    #[test]
    fn restricted_day_without_stretches_is_never_allowed() {
        let d = decide(&day(&[]), &[], &none(), at(12, 0));
        assert_eq!(d.computer, Computer::OutsideSchedule);
    }

    #[test]
    fn stretch_can_end_at_midnight() {
        let rule = day(&[(1320, 1440)]);
        let d = decide(&rule, &[], &none(), at(23, 59));
        assert_eq!(d.computer, Computer::Allowed);
        assert_eq!(d.next_change, at(0, 0) + Days::new(1));
    }

    #[test]
    fn blackout_wins_over_schedule_and_reports_its_end() {
        let span = BlackoutSpan {
            start: at(17, 0),
            end: at(19, 0),
            note: "dinner".into(),
        };
        let rule = day(&[(375, 1200)]);
        let d = decide(&rule, std::slice::from_ref(&span), &none(), at(18, 0));
        assert_eq!(
            d.computer,
            Computer::Blackout {
                until: at(19, 0),
                note: "dinner".into()
            }
        );
        assert_eq!(d.next_change, at(19, 0));
        // Before it starts, its start is the next change
        assert_eq!(
            decide(&rule, std::slice::from_ref(&span), &none(), at(16, 0)).next_change,
            at(17, 0)
        );
        // Its end is exclusive
        assert_eq!(
            decide(&rule, &[span], &none(), at(19, 0)).computer,
            Computer::Allowed
        );
    }

    #[test]
    fn blackout_spanning_days_holds_outside_the_schedule_too() {
        let span = BlackoutSpan {
            start: at(0, 0) - Days::new(1),
            end: at(0, 0) + Days::new(2),
            note: String::new(),
        };
        let d = decide(
            &day(&[(375, 1200)]),
            std::slice::from_ref(&span),
            &none(),
            at(22, 0),
        );
        assert_eq!(
            d.computer,
            Computer::Blackout {
                until: span.end,
                note: String::new()
            }
        );
    }

    #[test]
    fn overlapping_blackouts_report_the_later_end() {
        let a = BlackoutSpan {
            start: at(10, 0),
            end: at(12, 0),
            note: "a".into(),
        };
        let b = BlackoutSpan {
            start: at(11, 0),
            end: at(14, 0),
            note: "b".into(),
        };
        let d = decide(&DayRule::default(), &[a, b], &none(), at(11, 30));
        assert_eq!(
            d.computer,
            Computer::Blackout {
                until: at(14, 0),
                note: "b".into()
            }
        );
    }

    #[test]
    fn budget_left_and_used_up() {
        let mut rule = DayRule::default();
        rule.budgets.insert(1, 60);
        let used = |secs| BTreeMap::from([(1, secs)]);

        let d = decide(&rule, &[], &used(600), at(12, 0));
        assert_eq!(
            d.categories,
            vec![CategoryStatus {
                category: 1,
                used_secs: 600,
                left_secs: 3000,
                used_up: false
            }]
        );
        let d = decide(&rule, &[], &used(3600), at(12, 0));
        assert_eq!(
            d.categories[0],
            CategoryStatus {
                category: 1,
                used_secs: 3600,
                left_secs: 0,
                used_up: true
            }
        );
        // Over budget never goes negative
        assert_eq!(
            decide(&rule, &[], &used(5000), at(12, 0)).categories[0].left_secs,
            0
        );
        // No usage recorded yet
        assert_eq!(
            decide(&rule, &[], &none(), at(12, 0)).categories[0].left_secs,
            3600
        );
    }

    #[test]
    fn zero_budget_is_used_up_from_the_start() {
        let mut rule = DayRule::default();
        rule.budgets.insert(1, 0);
        let d = decide(&rule, &[], &none(), at(0, 0));
        assert!(d.categories[0].used_up);
    }

    #[test]
    fn unbudgeted_categories_are_not_listed() {
        let d = decide(
            &DayRule::default(),
            &[],
            &BTreeMap::from([(1, 500)]),
            at(12, 0),
        );
        assert!(d.categories.is_empty());
    }

    #[test]
    fn validation() {
        assert!(validate_day(&day(&[(375, 450), (960, 1200)])).is_ok());
        assert!(validate_day(&day(&[(0, 1440)])).is_ok());
        assert_eq!(
            validate_day(&day(&[(600, 600)])).unwrap_err().field,
            "stretches"
        );
        assert_eq!(
            validate_day(&day(&[(700, 600)])).unwrap_err().field,
            "stretches"
        );
        assert_eq!(
            validate_day(&day(&[(0, 1441)])).unwrap_err().field,
            "stretches"
        );
        assert_eq!(
            validate_day(&day(&[(300, 600), (599, 700)]))
                .unwrap_err()
                .field,
            "stretches"
        );
        // Touching stretches don't overlap
        assert!(validate_day(&day(&[(300, 600), (600, 700)])).is_ok());
        let mut over = DayRule::default();
        over.budgets.insert(1, 1441);
        assert_eq!(validate_day(&over).unwrap_err().field, "budgets");
        assert!(validate_blackout(at(10, 0), at(11, 0)).is_ok());
        assert_eq!(
            validate_blackout(at(11, 0), at(11, 0)).unwrap_err().field,
            "end"
        );
    }

    #[test]
    fn decision_json_shape() {
        let d = decide(&day(&[]), &[], &none(), at(12, 0));
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["computer"]["state"], "outside_schedule");
        assert_eq!(json["next_change"], "2026-10-06T00:00:00");
    }
}
