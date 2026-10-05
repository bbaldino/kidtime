//! Rule types and the decision function. No I/O: callers supply the rules, usage and time.

use std::collections::BTreeMap;

use chrono::{Days, Duration, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};

pub type CategoryId = i64;

/// The one category created at first start.
pub const GAMES: CategoryId = 1;
/// Apps a person has marked as not worth tracking: hidden from the dashboard and never budgeted.
/// Their time is still recorded, so moving one back restores its history.
pub const IGNORED: CategoryId = 2;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    Blackout {
        until: NaiveDateTime,
        note: String,
    },
    /// A timer the parent started has run out (lock mode).
    TimerEnded {
        at: NaiveDateTime,
    },
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

/// What stops when a timer runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerMode {
    /// The computer locks, as outside allowed hours.
    Lock,
    /// Games close, as with a used-up games budget.
    Games,
}

/// A stop the parent set for "N minutes from now". It only ever ends things sooner than the rules would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timer {
    pub ends: NaiveDateTime,
    pub mode: TimerMode,
}

impl Timer {
    /// The stop lasts until the midnight after it ended (unless the parent lifts it first).
    pub fn stop_until(&self) -> NaiveDateTime {
        (self.ends.date() + Days::new(1)).and_time(NaiveTime::MIN)
    }
}

pub fn decide(
    day: &DayRule,
    blackouts: &[BlackoutSpan],
    used_secs: &BTreeMap<CategoryId, i64>,
    now: NaiveDateTime,
) -> Decision {
    decide_with_timer(day, blackouts, used_secs, None, now)
}

/// `decide`, with the parent's timer (if any) able to stop things sooner.
pub fn decide_with_timer(
    day: &DayRule,
    blackouts: &[BlackoutSpan],
    used_secs: &BTreeMap<CategoryId, i64>,
    timer: Option<&Timer>,
    now: NaiveDateTime,
) -> Decision {
    // A timer whose stop has passed no longer matters
    let timer = timer.filter(|t| now < t.stop_until());
    let ended = timer.is_some_and(|t| now >= t.ends);
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
    let computer = match (active, timer) {
        (Some(b), _) => Computer::Blackout {
            until: b.end,
            note: b.note.clone(),
        },
        (None, Some(t)) if ended && t.mode == TimerMode::Lock => {
            Computer::TimerEnded { at: t.ends }
        }
        _ if !in_stretch => Computer::OutsideSchedule,
        _ => Computer::Allowed,
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
    if let Some(t) = timer {
        consider(t.ends);
        consider(t.stop_until());
    }

    let mut categories: Vec<CategoryStatus> = day
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
    // A games timer counts down the Games category too, budget or not; the sooner end wins
    if let Some(t) = timer.filter(|t| t.mode == TimerMode::Games) {
        let timer_left = (t.ends - now).num_seconds().max(0);
        match categories.iter_mut().find(|c| c.category == GAMES) {
            Some(c) => c.left_secs = c.left_secs.min(timer_left),
            None => categories.push(CategoryStatus {
                category: GAMES,
                used_secs: used_secs.get(&GAMES).copied().unwrap_or(0),
                left_secs: timer_left,
                used_up: false,
            }),
        }
        for c in categories.iter_mut().filter(|c| c.category == GAMES) {
            c.used_up = c.left_secs == 0;
        }
    }

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

    fn lock_timer(ends: NaiveDateTime) -> Timer {
        Timer {
            ends,
            mode: TimerMode::Lock,
        }
    }

    fn games_timer(ends: NaiveDateTime) -> Timer {
        Timer {
            ends,
            mode: TimerMode::Games,
        }
    }

    #[test]
    fn a_lock_timer_counts_down_then_locks_until_midnight() {
        let t = lock_timer(at(19, 30));
        let before = decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), at(19, 0));
        assert_eq!(before.computer, Computer::Allowed);
        assert_eq!(before.next_change, at(19, 30));
        let after = decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), at(19, 30));
        assert_eq!(after.computer, Computer::TimerEnded { at: at(19, 30) });
        assert_eq!(after.next_change, at(0, 0) + Days::new(1));
        let tomorrow = at(0, 0) + Days::new(1);
        assert_eq!(
            decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), tomorrow).computer,
            Computer::Allowed
        );
    }

    #[test]
    fn a_blackout_wins_over_an_ended_timer_which_wins_over_the_schedule() {
        let t = lock_timer(at(18, 0));
        let span = BlackoutSpan {
            start: at(18, 30),
            end: at(19, 0),
            note: "dinner".into(),
        };
        let d = decide_with_timer(
            &DayRule::default(),
            std::slice::from_ref(&span),
            &none(),
            Some(&t),
            at(18, 45),
        );
        assert_eq!(
            d.computer,
            Computer::Blackout {
                until: at(19, 0),
                note: "dinner".into()
            }
        );
        let outside = day(&[(375, 1080)]); // allowed until 6pm
        let d = decide_with_timer(&outside, &[], &none(), Some(&t), at(18, 10));
        assert_eq!(d.computer, Computer::TimerEnded { at: at(18, 0) });
    }

    #[test]
    fn an_earlier_bedtime_comes_first() {
        let t = lock_timer(at(21, 30));
        let d = decide_with_timer(&day(&[(375, 1260)]), &[], &none(), Some(&t), at(20, 50));
        assert_eq!(d.next_change, at(21, 0));
        assert_eq!(
            decide_with_timer(&day(&[(375, 1260)]), &[], &none(), Some(&t), at(21, 10)).computer,
            Computer::OutsideSchedule
        );
    }

    #[test]
    fn a_games_timer_counts_down_without_a_budget_and_then_uses_games_up() {
        let t = games_timer(at(16, 30));
        let d = decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), at(16, 0));
        assert_eq!(
            d.categories,
            vec![CategoryStatus {
                category: GAMES,
                used_secs: 0,
                left_secs: 1800,
                used_up: false
            }]
        );
        assert_eq!(d.computer, Computer::Allowed);
        let after = decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), at(16, 30));
        assert!(after.categories[0].used_up);
        assert_eq!(after.computer, Computer::Allowed);
    }

    #[test]
    fn a_games_timer_and_a_budget_count_to_whichever_is_sooner() {
        let mut rule = DayRule::default();
        rule.budgets.insert(GAMES, 60);
        let used = BTreeMap::from([(GAMES, 600)]); // 50 minutes of budget left
        let t = games_timer(at(16, 20));
        let d = decide_with_timer(&rule, &[], &used, Some(&t), at(16, 0));
        assert_eq!(d.categories[0].left_secs, 1200);
        let late = games_timer(at(18, 0));
        assert_eq!(
            decide_with_timer(&rule, &[], &used, Some(&late), at(16, 0)).categories[0].left_secs,
            3000
        );
    }

    #[test]
    fn a_timer_crossing_midnight_stops_until_the_following_midnight() {
        let t = lock_timer(at(0, 20) + Days::new(1)); // started 11:50pm for 30 minutes
        assert_eq!(t.stop_until(), at(0, 0) + Days::new(2));
        let next = |h, m| at(h, m) + Days::new(1);
        assert_eq!(
            decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), next(0, 10)).computer,
            Computer::Allowed
        );
        assert!(matches!(
            decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), next(0, 20)).computer,
            Computer::TimerEnded { .. }
        ));
        assert!(matches!(
            decide_with_timer(&DayRule::default(), &[], &none(), Some(&t), next(23, 59)).computer,
            Computer::TimerEnded { .. }
        ));
        assert_eq!(
            decide_with_timer(
                &DayRule::default(),
                &[],
                &none(),
                Some(&t),
                at(0, 0) + Days::new(2)
            )
            .computer,
            Computer::Allowed
        );
    }

    #[test]
    fn timer_json_shape() {
        let d = decide_with_timer(
            &DayRule::default(),
            &[],
            &none(),
            Some(&lock_timer(at(19, 30))),
            at(20, 0),
        );
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["computer"]["state"], "timer_ended");
        assert_eq!(json["computer"]["at"], "2026-10-05T19:30:00");
        let t: Timer =
            serde_json::from_str(r#"{"ends":"2026-10-05T19:30:00","mode":"games"}"#).unwrap();
        assert_eq!(t.mode, TimerMode::Games);
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
