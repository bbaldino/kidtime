# Rules and Management UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a parent set a weekly schedule, one-off blackouts and a Games budget per kid, see each kid's
current decision and time left, and read a log of what kidtime would have done, all behind a verified login.

**Architecture:** Everything is in `crates/server`. A pure module (`rules.rs`) holds the rule types and the
`decide` function. `db.rs` gains the app catalogue, per-app time stretches, rule storage and the event log.
`auth.rs` verifies the reverse proxy's signed token as axum middleware. `api.rs` holds the new JSON handlers.
The dashboard stays vanilla JS with no build step and gains three tabs. Nothing changes in the agent or the
wire protocol.

**Tech Stack:** Rust 2024, axum 0.8, rusqlite (bundled SQLite), chrono, `jsonwebtoken`, `reqwest` (rustls),
vanilla JS/CSS.

**Spec:** `docs/superpowers/specs/2026-10-02-rules-and-management-design.md`. Read it before starting any task.

## Global Constraints

- Add dependencies with `cargo add`, never by editing `Cargo.toml` by hand.
- Format with `cargo +nightly fmt`. The gate is: `cargo +nightly fmt --check`,
  `cargo +stable clippy --all-targets --all-features -- -D warnings`, `cargo +stable test --locked`.
  Run all three before every commit.
- Only the first letter of an acronym is capitalised in identifiers: `Jwt`, `Jwk`, `Api`, never `JWT`.
- No household details in committed files: accounts are `kid1`/`kid2`, hosts `host-a`/`host-b`, issuer
  `https://team.example.com`, audience `test-aud`.
- Days and weekdays use the server's local time zone. Weekday index: 0 = Monday … 6 = Sunday
  (`chrono::Datelike::weekday().num_days_from_monday()`).
- Stretch bounds are minutes after local midnight: `start < end`, `end ≤ 1440`, no overlap within a day.
- A restricted day with no stretches means the computer is not allowed that day.
- A blackout takes precedence over the schedule when both apply.
- An account with no rules is unrestricted. "All restricted accounts" = accounts with at least one restricted
  day or one budget.
- Only `POST /api/report` and `GET /healthz` answer without a valid login when the login check is on.
  A request without a valid token gets 401 with an empty body, static files included.
- With the login check off (either setting unset), log a warning at start-up. Exactly one of the two
  settings set is a start-up error. Key fetch failure means 503, never open access.
- Invalid input gets 422 with JSON `{"error": "<message>", "field": "<name>"}`.
- `app_activity` and `event` rows older than 30 days are deleted once a day.
- Dashboard: vanilla JS, no build step, phone-first, status never shown by colour alone. Prettier style for
  new JS: single quotes are NOT used in the existing files (they use double quotes and semicolons); match
  the existing files rather than reformatting them.
- Work on the branch `rules-and-management`. Commit after each task with the message given. Do not push,
  and do not merge with a merge commit: the release bot only reads first-parent history.
- Every `feat:`/`fix:` commit that reaches `main` is released. The user decides when the branch lands.

## Review Focus

Inputs and conditions the spec implies but that are easy to leave untested. Each has a test in the task named.

1. **Session expiry seen from the browser.** Through the proxy an expired login is a cross-origin redirect,
   not a 401, so `fetch` would fail or follow it. Expected: the page reloads into the login. (Task 7: the
   `api()` helper uses `redirect: "manual"` and treats `opaqueredirect` like 401; checked by hand.)
2. **App ids with reserved characters.** Stream windows are identified by title (`window:A/B: Game?`).
   Expected: the catalogue endpoint can set the category of any id. (Task 6: API test with such an id.)
3. **A sample that crosses midnight.** Expected: its seconds are split between the two days' category time.
   (Task 2: clipping test.)
4. **Budget of 0 minutes.** Expected: the category is used up from the start of the day, not "unlimited".
   (Task 1: `decide` test.)
5. **A token signed with an unknown key id during key rotation.** Expected: one refetch, then accept; no
   refetch storm when the id is simply wrong. (Task 5: refetch-throttle test.)

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/server/src/rules.rs` (new) | Rule types, validation, `decide`. No I/O. |
| `crates/server/src/db.rs` (modify) | Schema; recording (catalogue, stretches); category time; rule, blackout, event storage; pruning. |
| `crates/server/src/auth.rs` (new) | Token verification, key cache, login middleware. |
| `crates/server/src/api.rs` (new) | Handlers for rules, blackouts, apps, categories, events. |
| `crates/server/src/main.rs` (modify) | Config, `router()`, existing handlers, status additions, daily prune task. |
| `crates/server/static/index.html`, `app.js`, `style.css` (modify) | Tabs, Today additions, shared helpers. |
| `crates/server/static/manage.js` (new) | Rules, blackouts and Apps tabs. |
| `deploy/server.toml.example`, `compose.yaml`, `README.md`, `CLAUDE.md` (modify) | The two new settings, documentation. |

---

### Task 0: Branch

- [ ] **Step 1: Create the branch**

```bash
git switch -c rules-and-management
```

---

### Task 1: Rule types and `decide`

**Files:**
- Create: `crates/server/src/rules.rs`
- Modify: `crates/server/src/main.rs` (add `mod rules;` under `mod db;`)

**Interfaces:**
- Consumes: nothing.
- Produces (all `pub`, in `rules`):
  - `type CategoryId = i64;`
  - `struct Stretch { start_min: u16, end_min: u16 }`
  - `struct DayRule { restricted: bool, stretches: Vec<Stretch>, budgets: BTreeMap<CategoryId, u32> }` (budget in minutes; `Default` = unrestricted, no budgets)
  - `struct BlackoutSpan { start: NaiveDateTime, end: NaiveDateTime, note: String }`
  - `enum Computer { Allowed, OutsideSchedule, Blackout { until: NaiveDateTime, note: String } }`
  - `struct CategoryStatus { category: CategoryId, used_secs: i64, left_secs: i64, used_up: bool }`
  - `struct Decision { computer: Computer, categories: Vec<CategoryStatus>, next_change: NaiveDateTime }`
  - `fn validate_day(rule: &DayRule) -> Result<(), Invalid>`
  - `fn validate_blackout(start: NaiveDateTime, end: NaiveDateTime) -> Result<(), Invalid>`
  - `struct Invalid { field: &'static str, message: String }`
  - `fn decide(day: &DayRule, blackouts: &[BlackoutSpan], used_secs: &BTreeMap<CategoryId, i64>, now: NaiveDateTime) -> Decision`

`decide` works entirely in local wall-clock time (`NaiveDateTime`); callers convert. `day` is the rule for
`now`'s weekday. `blackouts` are the ones that apply to this account. Only budgeted categories appear in
`categories`.

- [ ] **Step 1: Write the failing tests**

Create `crates/server/src/rules.rs` with only the tests and the imports they need:

```rust
//! Rule types and the decision function. No I/O: callers supply the rules, usage and time.

use std::collections::BTreeMap;

use chrono::{Days, Duration, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn day(stretches: &[(u16, u16)]) -> DayRule {
        DayRule {
            restricted: true,
            stretches: stretches.iter().map(|&(start_min, end_min)| Stretch { start_min, end_min }).collect(),
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
        assert_eq!(decide(&rule, &[], &none(), at(6, 14)).computer, Computer::OutsideSchedule);
        assert_eq!(decide(&rule, &[], &none(), at(6, 15)).computer, Computer::Allowed);
        assert_eq!(decide(&rule, &[], &none(), at(19, 59)).computer, Computer::Allowed);
        assert_eq!(decide(&rule, &[], &none(), at(20, 0)).computer, Computer::OutsideSchedule);
    }

    #[test]
    fn next_change_is_the_nearest_stretch_edge() {
        let rule = day(&[(375, 450), (960, 1200)]); // 6:15–7:30, 16:00–20:00
        assert_eq!(decide(&rule, &[], &none(), at(5, 0)).next_change, at(6, 15));
        assert_eq!(decide(&rule, &[], &none(), at(7, 0)).next_change, at(7, 30));
        assert_eq!(decide(&rule, &[], &none(), at(12, 0)).next_change, at(16, 0));
        assert_eq!(decide(&rule, &[], &none(), at(21, 0)).next_change, at(0, 0) + Days::new(1));
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
        let span = BlackoutSpan { start: at(17, 0), end: at(19, 0), note: "dinner".into() };
        let rule = day(&[(375, 1200)]);
        let d = decide(&rule, std::slice::from_ref(&span), &none(), at(18, 0));
        assert_eq!(d.computer, Computer::Blackout { until: at(19, 0), note: "dinner".into() });
        assert_eq!(d.next_change, at(19, 0));
        // Before it starts, its start is the next change
        assert_eq!(decide(&rule, std::slice::from_ref(&span), &none(), at(16, 0)).next_change, at(17, 0));
        // Its end is exclusive
        assert_eq!(decide(&rule, &[span], &none(), at(19, 0)).computer, Computer::Allowed);
    }

    #[test]
    fn blackout_spanning_days_holds_outside_the_schedule_too() {
        let span = BlackoutSpan { start: at(0, 0) - Days::new(1), end: at(0, 0) + Days::new(2), note: String::new() };
        let d = decide(&day(&[(375, 1200)]), &[span.clone()], &none(), at(22, 0));
        assert_eq!(d.computer, Computer::Blackout { until: span.end, note: String::new() });
    }

    #[test]
    fn overlapping_blackouts_report_the_later_end() {
        let a = BlackoutSpan { start: at(10, 0), end: at(12, 0), note: "a".into() };
        let b = BlackoutSpan { start: at(11, 0), end: at(14, 0), note: "b".into() };
        let d = decide(&DayRule::default(), &[a, b], &none(), at(11, 30));
        assert_eq!(d.computer, Computer::Blackout { until: at(14, 0), note: "b".into() });
    }

    #[test]
    fn budget_left_and_used_up() {
        let mut rule = DayRule::default();
        rule.budgets.insert(1, 60);
        let used = |secs| BTreeMap::from([(1, secs)]);

        let d = decide(&rule, &[], &used(600), at(12, 0));
        assert_eq!(d.categories, vec![CategoryStatus { category: 1, used_secs: 600, left_secs: 3000, used_up: false }]);
        let d = decide(&rule, &[], &used(3600), at(12, 0));
        assert_eq!(d.categories[0], CategoryStatus { category: 1, used_secs: 3600, left_secs: 0, used_up: true });
        // Over budget never goes negative
        assert_eq!(decide(&rule, &[], &used(5000), at(12, 0)).categories[0].left_secs, 0);
        // No usage recorded yet
        assert_eq!(decide(&rule, &[], &none(), at(12, 0)).categories[0].left_secs, 3600);
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
        let d = decide(&DayRule::default(), &[], &BTreeMap::from([(1, 500)]), at(12, 0));
        assert!(d.categories.is_empty());
    }

    #[test]
    fn validation() {
        assert!(validate_day(&day(&[(375, 450), (960, 1200)])).is_ok());
        assert!(validate_day(&day(&[(0, 1440)])).is_ok());
        assert_eq!(validate_day(&day(&[(600, 600)])).unwrap_err().field, "stretches");
        assert_eq!(validate_day(&day(&[(700, 600)])).unwrap_err().field, "stretches");
        assert_eq!(validate_day(&day(&[(0, 1441)])).unwrap_err().field, "stretches");
        assert_eq!(validate_day(&day(&[(300, 600), (599, 700)])).unwrap_err().field, "stretches");
        // Touching stretches don't overlap
        assert!(validate_day(&day(&[(300, 600), (600, 700)])).is_ok());
        let mut over = DayRule::default();
        over.budgets.insert(1, 1441);
        assert_eq!(validate_day(&over).unwrap_err().field, "budgets");
        assert!(validate_blackout(at(10, 0), at(11, 0)).is_ok());
        assert_eq!(validate_blackout(at(11, 0), at(11, 0)).unwrap_err().field, "end");
    }

    #[test]
    fn decision_json_shape() {
        let d = decide(&day(&[]), &[], &none(), at(12, 0));
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["computer"]["state"], "outside_schedule");
        assert_eq!(json["next_change"], "2026-10-06T00:00:00");
    }
}
```

Add `mod rules;` to `crates/server/src/main.rs` below `mod db;`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p server rules`
Expected: compile errors, "cannot find type `DayRule`" and similar.

- [ ] **Step 3: Write the implementation**

Insert above the `tests` module:

```rust
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
    Invalid { field, message: message.into() }
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

    let active = blackouts.iter().filter(|b| b.start <= now && now < b.end).max_by_key(|b| b.end);
    let in_stretch =
        !day.restricted || day.stretches.iter().any(|s| s.start_min <= minute && minute < s.end_min);
    let computer = match active {
        Some(b) => Computer::Blackout { until: b.end, note: b.note.clone() },
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
            CategoryStatus { category, used_secs: used, left_secs, used_up: left_secs == 0 }
        })
        .collect();

    Decision { computer, categories, next_change }
}
```

chrono needs its `serde` feature for `NaiveDateTime` in JSON:

```bash
cargo add -p server chrono --features serde
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p server rules`
Expected: 13 tests pass. Until Task 6 uses these items, clippy reports them as dead code; add
`#![allow(dead_code)]` at the top of `rules.rs` for now. Task 6 removes it.

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt
git add crates/server Cargo.lock
git commit -m "feat(server): add rule types and the decision function"
```

---

### Task 2: App catalogue, per-app stretches and category time

**Files:**
- Modify: `crates/server/src/db.rs`

**Interfaces:**
- Consumes: `rules::CategoryId`.
- Produces (methods on `db::Db`, plus items in `db`):
  - `pub const GAMES: CategoryId = 1;`
  - `pub struct AppEntry { app_id: String, name: String, first_seen: i64, last_seen: i64, category_id: Option<CategoryId>, set_by_person: bool, reviewed: bool }` (derives `Serialize`)
  - `pub fn apps(&self) -> Result<Vec<AppEntry>>` (unreviewed first, then by `last_seen` descending)
  - `pub fn set_app_category(&mut self, app_id: &str, category_id: Option<CategoryId>) -> Result<bool>` (false if the app is unknown; sets `set_by_person` and `reviewed`)
  - `pub fn categories(&self) -> Result<Vec<(CategoryId, String)>>`
  - `pub fn category_secs(&self, user: &str, day: NaiveDate) -> Result<BTreeMap<CategoryId, i64>>`
  - `pub fn is_account(&self, user: &str) -> Result<bool>` (has ever appeared in a report)
  - `record` keeps its signature and additionally maintains `account`, `app` and `app_activity`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `db.rs`. First a helper that replaces the copy-pasted temp-file setup for the
new tests (leave the two existing tests as they are):

```rust
    fn temp_db(name: &str) -> (Db, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("kidtime-test-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (Db::open(&path).unwrap(), path)
    }

    /// One 15-second sample ending at `at`, for kid1, with the given apps.
    fn sample_report(host: &str, seq: u64, at: i64, state: UserState, apps: &[(&str, &str)]) -> Report {
        Report {
            host: host.into(),
            agent_id: format!("agent-{host}"),
            interval_secs: 15,
            samples: vec![Sample {
                seq,
                at,
                elapsed_secs: 15,
                users: vec![UserSample {
                    user: "kid1".into(),
                    state,
                    apps: apps.iter().map(|&(id, name)| App { id: id.into(), name: name.into() }).collect(),
                }],
            }],
        }
    }

    #[test]
    fn apps_are_catalogued_and_auto_categorised() {
        let (mut db, path) = temp_db("catalogue");
        let t0 = start_of_test();
        db.record(&sample_report("host-a", 1, t0 + 15, UserState::Active, &[("steam:1", "Minecraft"), ("org.example.Editor", "Editor")])).unwrap();
        db.record(&sample_report("host-a", 2, t0 + 30, UserState::Streaming, &[("window:Some Game", "Some Game"), ("steam", "Steam")])).unwrap();

        let apps = db.apps().unwrap();
        let cat = |id: &str| apps.iter().find(|a| a.app_id == id).unwrap().category_id;
        assert_eq!(cat("steam:1"), Some(GAMES));
        assert_eq!(cat("org.example.Editor"), None);
        assert_eq!(cat("window:Some Game"), Some(GAMES));
        assert_eq!(cat("steam"), None);
        assert!(apps.iter().all(|a| !a.reviewed && !a.set_by_person));
        assert!(db.is_account("kid1").unwrap());
        assert!(!db.is_account("kid9").unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_persons_category_survives_later_reports() {
        let (mut db, path) = temp_db("person");
        let t0 = start_of_test();
        db.record(&sample_report("host-a", 1, t0 + 15, UserState::Active, &[("steam:1", "Minecraft")])).unwrap();
        assert!(db.set_app_category("steam:1", None).unwrap());
        db.record(&sample_report("host-a", 2, t0 + 30, UserState::Streaming, &[("steam:1", "Minecraft")])).unwrap();
        let app = db.apps().unwrap().remove(0);
        assert_eq!(app.category_id, None);
        assert!(app.set_by_person && app.reviewed);
        assert!(!db.set_app_category("no-such-app", Some(GAMES)).unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_uncategorised_app_becomes_a_game_when_seen_in_a_stream() {
        let (mut db, path) = temp_db("promote");
        let t0 = start_of_test();
        db.record(&sample_report("host-a", 1, t0 + 15, UserState::Active, &[("window:Game", "Game")])).unwrap();
        assert_eq!(db.apps().unwrap()[0].category_id, None);
        db.record(&sample_report("host-a", 2, t0 + 30, UserState::Streaming, &[("window:Game", "Game")])).unwrap();
        assert_eq!(db.apps().unwrap()[0].category_id, Some(GAMES));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn back_to_back_samples_merge_into_one_stretch() {
        let (mut db, path) = temp_db("merge");
        let t0 = start_of_test();
        for seq in 1..=4 {
            db.record(&sample_report("host-a", seq, t0 + 15 * seq as i64, UserState::Active, &[("steam:1", "Minecraft")])).unwrap();
        }
        // A gap, then one more sample
        db.record(&sample_report("host-a", 5, t0 + 600, UserState::Active, &[("steam:1", "Minecraft")])).unwrap();
        let rows: i64 = db.conn.query_row("SELECT COUNT(*) FROM app_activity", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2);
        let today = Local::now().date_naive();
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 75);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn category_time_is_the_union_across_hosts_and_apps() {
        let (mut db, path) = temp_db("union");
        let t0 = start_of_test();
        // The same 15 seconds reported by two hosts, and two games at once on one of them
        db.record(&sample_report("host-a", 1, t0 + 15, UserState::Streaming, &[("steam:1", "Minecraft"), ("steam:2", "Portal")])).unwrap();
        db.record(&sample_report("host-b", 1, t0 + 15, UserState::Active, &[("steam:1", "Minecraft"), ("org.example.Viewer", "Viewer")])).unwrap();
        let today = Local::now().date_naive();
        let secs = db.category_secs("kid1", today).unwrap();
        assert_eq!(secs.get(&GAMES), Some(&15));
        assert_eq!(secs.len(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn recategorising_changes_the_whole_day() {
        let (mut db, path) = temp_db("recat");
        let t0 = start_of_test();
        db.record(&sample_report("host-a", 1, t0 + 15, UserState::Active, &[("org.example.Launcher", "Launcher")])).unwrap();
        let today = Local::now().date_naive();
        assert!(db.category_secs("kid1", today).unwrap().is_empty());
        db.set_app_category("org.example.Launcher", Some(GAMES)).unwrap();
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 15);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_sample_crossing_midnight_is_split_between_the_days() {
        let (mut db, path) = temp_db("midnight");
        let today = Local::now().date_naive();
        let yesterday = today - Days::new(1);
        // Ends 5 seconds into today, so 10 seconds belong to yesterday
        db.record(&sample_report("host-a", 1, midnight(today) + 5, UserState::Active, &[("steam:1", "Minecraft")])).unwrap();
        assert_eq!(db.category_secs("kid1", yesterday).unwrap()[&GAMES], 10);
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 5);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn idle_samples_add_no_app_time() {
        let (mut db, path) = temp_db("idle");
        db.record(&sample_report("host-a", 1, start_of_test() + 15, UserState::Idle, &[("steam:1", "Minecraft")])).unwrap();
        assert!(db.category_secs("kid1", Local::now().date_naive()).unwrap().is_empty());
        assert!(db.apps().unwrap().is_empty());
        // The account is still known: it reported
        assert!(db.is_account("kid1").unwrap());
        std::fs::remove_file(&path).unwrap();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p server db::`
Expected: compile errors for `GAMES`, `apps`, `set_app_category`, `category_secs`, `is_account`.

- [ ] **Step 3: Write the implementation**

Imports at the top of `db.rs`:

```rust
use std::collections::BTreeMap;

use serde::Serialize;

use crate::rules::CategoryId;
```

Below the imports:

```rust
/// The one category created at first start.
pub const GAMES: CategoryId = 1;

#[derive(Debug, Serialize)]
pub struct AppEntry {
    pub app_id: String,
    pub name: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub category_id: Option<CategoryId>,
    pub set_by_person: bool,
    pub reviewed: bool,
}
```

Append to the `execute_batch` string in `Db::open`, before the closing quote:

```sql
             CREATE TABLE IF NOT EXISTS account (
                 user      TEXT PRIMARY KEY,
                 last_seen INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS category (
                 id   INTEGER PRIMARY KEY,
                 name TEXT NOT NULL
             );
             INSERT OR IGNORE INTO category (id, name) VALUES (1, 'Games');
             CREATE TABLE IF NOT EXISTS app (
                 app_id        TEXT PRIMARY KEY,
                 name          TEXT NOT NULL,
                 first_seen    INTEGER NOT NULL,
                 last_seen     INTEGER NOT NULL,
                 category_id   INTEGER REFERENCES category (id),
                 set_by_person INTEGER NOT NULL DEFAULT 0,
                 reviewed      INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS app_activity (
                 user   TEXT NOT NULL,
                 host   TEXT NOT NULL,
                 app_id TEXT NOT NULL,
                 start  INTEGER NOT NULL,
                 end    INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS app_activity_user_end ON app_activity (user, end);
```

In `record`, prepare three more statements next to `add` and `active`:

```rust
            let mut seen = tx.prepare(
                "INSERT INTO account (user, last_seen) VALUES (?1, ?2)
                 ON CONFLICT (user) DO UPDATE SET last_seen = MAX(last_seen, excluded.last_seen)",
            )?;
            // The automatic category only fills an empty one, and never after a person has chosen
            let mut catalogue = tx.prepare(
                "INSERT INTO app (app_id, name, first_seen, last_seen, category_id) VALUES (?1, ?2, ?3, ?3, ?4)
                 ON CONFLICT (app_id) DO UPDATE SET
                     name = excluded.name,
                     last_seen = MAX(last_seen, excluded.last_seen),
                     category_id = CASE WHEN set_by_person = 0 AND category_id IS NULL
                                        THEN excluded.category_id ELSE category_id END",
            )?;
            let mut extend = tx.prepare(
                "UPDATE app_activity SET end = ?5
                 WHERE user = ?1 AND host = ?2 AND app_id = ?3 AND end BETWEEN ?4 - 1 AND ?4 + 1",
            )?;
            let mut stretch = tx.prepare(
                "INSERT INTO app_activity (user, host, app_id, start, end) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
```

Inside the sample loop, before the existing `for user in sample.users.iter().filter(|u| u.state.counts())`:

```rust
                for user in &sample.users {
                    seen.execute(params![user.user, sample.at])?;
                }
```

Inside the existing counted-user loop, in the existing `for app in &user.apps` body, after the `add.execute`:

```rust
                        let start = sample.at - i64::from(sample.elapsed_secs);
                        let auto = automatic_category(&app.id, user.state);
                        catalogue.execute(params![app.id, app.name, sample.at, auto])?;
                        if extend.execute(params![user.user, report.host, app.id, start, sample.at])? == 0 {
                            stretch.execute(params![user.user, report.host, app.id, start, sample.at])?;
                        }
```

New free function, next to `midnight`:

```rust
/// The category an app gets when nobody has chosen one: Steam-launched apps, and anything in a stream
/// except the Steam client, are games.
fn automatic_category(app_id: &str, state: UserState) -> Option<CategoryId> {
    let streamed_game = state == UserState::Streaming && app_id != "steam";
    (app_id.starts_with("steam:") || streamed_game).then_some(GAMES)
}
```

(`use protocol::UserState;` at the top of `db.rs`; the tests module already imports it, so drop it from
the tests' `use` line if the compiler reports it unused.)

New methods on `Db`:

```rust
    /// The catalogue: apps nobody has looked at first, then the most recently seen.
    pub fn apps(&self) -> Result<Vec<AppEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT app_id, name, first_seen, last_seen, category_id, set_by_person, reviewed
             FROM app ORDER BY reviewed, last_seen DESC, app_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AppEntry {
                    app_id: r.get(0)?,
                    name: r.get(1)?,
                    first_seen: r.get(2)?,
                    last_seen: r.get(3)?,
                    category_id: r.get(4)?,
                    set_by_person: r.get(5)?,
                    reviewed: r.get(6)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// A person's choice of category (or none). Returns false if the app is unknown.
    pub fn set_app_category(&mut self, app_id: &str, category_id: Option<CategoryId>) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE app SET category_id = ?2, set_by_person = 1, reviewed = 1 WHERE app_id = ?1",
            params![app_id, category_id],
        )?;
        Ok(changed > 0)
    }

    pub fn categories(&self) -> Result<Vec<(CategoryId, String)>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM category ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn is_account(&self, user: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS (SELECT 1 FROM account WHERE user = ?1)", [user], |r| r.get(0))?)
    }

    /// Seconds on `day` during which the user had an app of each category in use, on any host.
    /// Categories with no time are left out.
    pub fn category_secs(&self, user: &str, day: NaiveDate) -> Result<BTreeMap<CategoryId, i64>> {
        let (day_start, day_end) = (midnight(day), midnight(day + Days::new(1)));
        let mut stmt = self.conn.prepare(
            "SELECT app.category_id, a.start, a.end FROM app_activity a JOIN app ON app.app_id = a.app_id
             WHERE a.user = ?1 AND a.end > ?2 AND a.start < ?3 AND app.category_id IS NOT NULL
             ORDER BY app.category_id, a.start",
        )?;
        let mut by_category: BTreeMap<CategoryId, Vec<(i64, i64)>> = BTreeMap::new();
        let rows = stmt.query_map(params![user, day_start, day_end], |r| {
            Ok((r.get::<_, CategoryId>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (category, start, end) = row?;
            by_category.entry(category).or_default().push((start, end));
        }
        Ok(by_category
            .into_iter()
            .map(|(category, intervals)| {
                let secs = merge(intervals).iter().map(|&(s, e)| (e.min(day_end) - s.max(day_start)).max(0)).sum();
                (category, secs)
            })
            .collect())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p server db::`
Expected: the 2 existing and 8 new tests pass.

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt
git add crates/server
git commit -m "feat(server): catalogue apps and count time per category"
```

---

### Task 3: Rule and blackout storage, and the decision for an account

**Files:**
- Modify: `crates/server/src/db.rs`

**Interfaces:**
- Consumes: `rules::{DayRule, Stretch, BlackoutSpan, Decision, decide, CategoryId}`, `Db::category_secs`.
- Produces (methods on `db::Db`, plus items in `db`):
  - `pub struct Blackout { id: i64, user: Option<String>, start: NaiveDateTime, end: NaiveDateTime, note: String }` (derives `Serialize`; `user: None` = all restricted accounts)
  - `pub fn day_rule(&self, user: &str, weekday: u8) -> Result<DayRule>`
  - `pub fn week_rules(&self, user: &str) -> Result<Vec<DayRule>>` (7 entries, Monday first)
  - `pub fn set_day_rule(&mut self, user: &str, weekday: u8, rule: &DayRule) -> Result<()>` (replaces; the caller has validated)
  - `pub fn is_restricted(&self, user: &str) -> Result<bool>`
  - `pub fn add_blackout(&mut self, user: Option<&str>, start: NaiveDateTime, end: NaiveDateTime, note: &str) -> Result<i64>`
  - `pub fn delete_blackout(&mut self, id: i64) -> Result<bool>`
  - `pub fn blackouts(&self, now: NaiveDateTime) -> Result<Vec<Blackout>>` (not yet ended, by start)
  - `pub fn decision(&self, user: &str, now: NaiveDateTime) -> Result<Decision>`

Blackout times are stored as text in `%Y-%m-%dT%H:%M:%S` local time, which sorts correctly as text.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `db.rs`:

```rust
    use crate::rules::{Computer, DayRule, Stretch};

    fn noon_today() -> chrono::NaiveDateTime {
        Local::now().date_naive().and_hms_opt(12, 0, 0).unwrap()
    }

    fn weekday_today() -> u8 {
        use chrono::Datelike;
        Local::now().date_naive().weekday().num_days_from_monday() as u8
    }

    #[test]
    fn day_rules_round_trip_and_replace() {
        let (mut db, path) = temp_db("rules");
        assert_eq!(db.day_rule("kid1", 0).unwrap(), DayRule::default());
        assert!(!db.is_restricted("kid1").unwrap());

        let rule = DayRule {
            restricted: true,
            stretches: vec![Stretch { start_min: 960, end_min: 1200 }, Stretch { start_min: 375, end_min: 450 }],
            budgets: BTreeMap::from([(GAMES, 60)]),
        };
        db.set_day_rule("kid1", 0, &rule).unwrap();
        let stored = db.day_rule("kid1", 0).unwrap();
        // Stretches come back sorted by start
        assert_eq!(stored.stretches[0].start_min, 375);
        assert_eq!(stored.budgets, rule.budgets);
        assert!(stored.restricted);
        assert!(db.is_restricted("kid1").unwrap());
        assert_eq!(db.week_rules("kid1").unwrap().len(), 7);
        assert_eq!(db.week_rules("kid1").unwrap()[1], DayRule::default());

        db.set_day_rule("kid1", 0, &DayRule::default()).unwrap();
        assert_eq!(db.day_rule("kid1", 0).unwrap(), DayRule::default());
        assert!(!db.is_restricted("kid1").unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_budget_alone_makes_an_account_restricted() {
        let (mut db, path) = temp_db("budget-only");
        let rule = DayRule { restricted: false, stretches: vec![], budgets: BTreeMap::from([(GAMES, 30)]) };
        db.set_day_rule("kid1", 3, &rule).unwrap();
        assert!(db.is_restricted("kid1").unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn blackouts_list_only_those_not_ended() {
        let (mut db, path) = temp_db("blackouts");
        let now = noon_today();
        let hour = chrono::Duration::hours(1);
        db.add_blackout(Some("kid1"), now - hour * 3, now - hour, "over").unwrap();
        let current = db.add_blackout(None, now - hour, now + hour, "now").unwrap();
        db.add_blackout(Some("kid2"), now + hour * 5, now + hour * 6, "later").unwrap();
        let listed = db.blackouts(now).unwrap();
        assert_eq!(listed.iter().map(|b| b.note.as_str()).collect::<Vec<_>>(), ["now", "later"]);
        assert_eq!(listed[0].user, None);
        assert!(db.delete_blackout(current).unwrap());
        assert!(!db.delete_blackout(current).unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn decision_applies_own_and_everyone_blackouts_only_to_restricted_accounts() {
        let (mut db, path) = temp_db("decision");
        let now = noon_today();
        let hour = chrono::Duration::hours(1);
        db.add_blackout(None, now - hour, now + hour, "everyone").unwrap();

        // No rules: an "everyone" blackout doesn't touch this account
        assert_eq!(db.decision("parent", now).unwrap().computer, Computer::Allowed);

        let rule = DayRule { restricted: false, stretches: vec![], budgets: BTreeMap::from([(GAMES, 1)]) };
        db.set_day_rule("kid1", weekday_today(), &rule).unwrap();
        let d = db.decision("kid1", now).unwrap();
        assert_eq!(d.computer, Computer::Blackout { until: now + hour, note: "everyone".into() });

        // A blackout naming another account doesn't apply
        db.set_day_rule("kid2", weekday_today(), &rule).unwrap();
        db.add_blackout(Some("kid1"), now - hour, now + hour * 4, "kid1 only").unwrap();
        let d = db.decision("kid2", now).unwrap();
        assert_eq!(d.computer, Computer::Blackout { until: now + hour, note: "everyone".into() });
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn decision_uses_todays_rule_and_todays_category_time() {
        let (mut db, path) = temp_db("decision-budget");
        let rule = DayRule { restricted: false, stretches: vec![], budgets: BTreeMap::from([(GAMES, 1)]) };
        db.set_day_rule("kid1", weekday_today(), &rule).unwrap();
        let t0 = start_of_test();
        for seq in 1..=4 {
            db.record(&sample_report("host-a", seq, t0 + 15 * seq as i64, UserState::Active, &[("steam:1", "Minecraft")])).unwrap();
        }
        let d = db.decision("kid1", noon_today()).unwrap();
        assert_eq!(d.categories[0].used_secs, 60);
        assert!(d.categories[0].used_up);
        std::fs::remove_file(&path).unwrap();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p server db::`
Expected: compile errors for `day_rule`, `set_day_rule`, `is_restricted`, `add_blackout`, `blackouts`, `decision`.

- [ ] **Step 3: Write the implementation**

Imports: extend the chrono `use` with `Datelike, NaiveDateTime`, and add
`use crate::rules::{self, BlackoutSpan, DayRule, Decision, Stretch};`.

Append to the schema string in `Db::open`:

```sql
             CREATE TABLE IF NOT EXISTS day_rule (
                 user       TEXT NOT NULL,
                 weekday    INTEGER NOT NULL,
                 restricted INTEGER NOT NULL,
                 PRIMARY KEY (user, weekday)
             );
             CREATE TABLE IF NOT EXISTS stretch (
                 user      TEXT NOT NULL,
                 weekday   INTEGER NOT NULL,
                 start_min INTEGER NOT NULL,
                 end_min   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS stretch_user_weekday ON stretch (user, weekday);
             CREATE TABLE IF NOT EXISTS budget (
                 user        TEXT NOT NULL,
                 weekday     INTEGER NOT NULL,
                 category_id INTEGER NOT NULL REFERENCES category (id),
                 minutes     INTEGER NOT NULL,
                 PRIMARY KEY (user, weekday, category_id)
             );
             CREATE TABLE IF NOT EXISTS blackout (
                 id    INTEGER PRIMARY KEY,
                 user  TEXT,
                 start TEXT NOT NULL,
                 end   TEXT NOT NULL,
                 note  TEXT NOT NULL
             );
```

Below `AppEntry`:

```rust
/// A one-off blackout. `user: None` applies to every restricted account.
#[derive(Debug, Serialize)]
pub struct Blackout {
    pub id: i64,
    pub user: Option<String>,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub note: String,
}

/// How local date-times are stored; this form sorts correctly as text.
const LOCAL_TIME: &str = "%Y-%m-%dT%H:%M:%S";

fn local_text(t: NaiveDateTime) -> String {
    t.format(LOCAL_TIME).to_string()
}

fn parse_local(text: String) -> rusqlite::Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(&text, LOCAL_TIME)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}
```

Methods on `Db`:

```rust
    pub fn day_rule(&self, user: &str, weekday: u8) -> Result<DayRule> {
        let restricted = self
            .conn
            .query_row("SELECT restricted FROM day_rule WHERE user = ?1 AND weekday = ?2", params![user, weekday], |r| r.get(0))
            .optional()?
            .unwrap_or(false);
        let mut stmt = self
            .conn
            .prepare("SELECT start_min, end_min FROM stretch WHERE user = ?1 AND weekday = ?2 ORDER BY start_min")?;
        let stretches = stmt
            .query_map(params![user, weekday], |r| Ok(Stretch { start_min: r.get(0)?, end_min: r.get(1)? }))?
            .collect::<Result<_, _>>()?;
        let mut stmt = self.conn.prepare("SELECT category_id, minutes FROM budget WHERE user = ?1 AND weekday = ?2")?;
        let budgets = stmt.query_map(params![user, weekday], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        Ok(DayRule { restricted, stretches, budgets })
    }

    /// Monday first.
    pub fn week_rules(&self, user: &str) -> Result<Vec<DayRule>> {
        (0..7).map(|weekday| self.day_rule(user, weekday)).collect()
    }

    /// Replaces the weekday's rule. The caller validates it first.
    pub fn set_day_rule(&mut self, user: &str, weekday: u8, rule: &DayRule) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM day_rule WHERE user = ?1 AND weekday = ?2", params![user, weekday])?;
        tx.execute("DELETE FROM stretch WHERE user = ?1 AND weekday = ?2", params![user, weekday])?;
        tx.execute("DELETE FROM budget WHERE user = ?1 AND weekday = ?2", params![user, weekday])?;
        if rule.restricted {
            tx.execute("INSERT INTO day_rule (user, weekday, restricted) VALUES (?1, ?2, 1)", params![user, weekday])?;
            for s in &rule.stretches {
                tx.execute(
                    "INSERT INTO stretch (user, weekday, start_min, end_min) VALUES (?1, ?2, ?3, ?4)",
                    params![user, weekday, s.start_min, s.end_min],
                )?;
            }
        }
        for (category, minutes) in &rule.budgets {
            tx.execute(
                "INSERT INTO budget (user, weekday, category_id, minutes) VALUES (?1, ?2, ?3, ?4)",
                params![user, weekday, category, minutes],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether the account has any rule: a restricted day or a budget.
    pub fn is_restricted(&self, user: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM day_rule WHERE user = ?1 AND restricted = 1)
                 OR EXISTS (SELECT 1 FROM budget WHERE user = ?1)",
            [user],
            |r| r.get(0),
        )?)
    }

    pub fn add_blackout(&mut self, user: Option<&str>, start: NaiveDateTime, end: NaiveDateTime, note: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO blackout (user, start, end, note) VALUES (?1, ?2, ?3, ?4)",
            params![user, local_text(start), local_text(end), note],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_blackout(&mut self, id: i64) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM blackout WHERE id = ?1", [id])? > 0)
    }

    /// Blackouts that haven't ended at `now`, earliest start first.
    pub fn blackouts(&self, now: NaiveDateTime) -> Result<Vec<Blackout>> {
        let mut stmt = self.conn.prepare("SELECT id, user, start, end, note FROM blackout WHERE end > ?1 ORDER BY start, id")?;
        let rows = stmt
            .query_map([local_text(now)], |r| {
                Ok(Blackout {
                    id: r.get(0)?,
                    user: r.get(1)?,
                    start: parse_local(r.get(2)?)?,
                    end: parse_local(r.get(3)?)?,
                    note: r.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// What the rules say for the account at `now`.
    pub fn decision(&self, user: &str, now: NaiveDateTime) -> Result<Decision> {
        let weekday = now.date().weekday().num_days_from_monday() as u8;
        let rule = self.day_rule(user, weekday)?;
        let restricted = self.is_restricted(user)?;
        let spans: Vec<BlackoutSpan> = self
            .blackouts(now)?
            .into_iter()
            .filter(|b| match &b.user {
                Some(name) => name == user,
                None => restricted,
            })
            .map(|b| BlackoutSpan { start: b.start, end: b.end, note: b.note })
            .collect();
        let used = self.category_secs(user, now.date())?;
        Ok(rules::decide(&rule, &spans, &used, now))
    }
```

Remove `#![allow(dead_code)]` items only in Task 6; `decision` is unused until then, so add
`#[allow(dead_code)]` on the methods clippy flags and remove those in Task 6.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p server db::`
Expected: 15 tests pass.

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt
git add crates/server
git commit -m "feat(server): store schedules, budgets and blackouts"
```

---

### Task 4: The would-have log and pruning

**Files:**
- Modify: `crates/server/src/db.rs`

**Interfaces:**
- Consumes: `rules::{Decision, Computer}`, `Db::decision`, `Db::categories`.
- Produces:
  - `pub struct Event { id: i64, user: String, at: i64, kind: String, detail: String }` (derives `Serialize`). `kind` is `"locked"`, `"closed"` or `"allowed"`.
  - `pub fn log_decision(&mut self, user: &str, at: i64, decision: &Decision) -> Result<bool>` (true if an event was added)
  - `pub fn events(&self, user: Option<&str>, limit: u32) -> Result<Vec<Event>>` (newest first)
  - `pub fn prune(&mut self, now: i64) -> Result<()>` (deletes `app_activity` and `event` rows older than 30 days)

An event is added only when the decision's key differs from the account's latest event. The key is the
computer state plus the sorted ids of used-up categories. An account with no events whose first decision is
"allowed, nothing used up" gets no event.

`detail` wording: for `locked`, `"outside schedule"` or `"blackout"` followed by `": <note>"` when the
note isn't empty; for `closed`, the names of the newly used-up categories joined with `", "`; for
`allowed`, empty.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `db.rs`:

```rust
    use crate::rules::{CategoryStatus, Decision};

    fn decision_of(computer: Computer, used_up: &[CategoryId]) -> Decision {
        Decision {
            computer,
            categories: used_up
                .iter()
                .map(|&category| CategoryStatus { category, used_secs: 60, left_secs: 0, used_up: true })
                .collect(),
            next_change: noon_today(),
        }
    }

    #[test]
    fn events_are_logged_once_per_change() {
        let (mut db, path) = temp_db("events");
        let allowed = decision_of(Computer::Allowed, &[]);
        let outside = decision_of(Computer::OutsideSchedule, &[]);
        let blackout = decision_of(Computer::Blackout { until: noon_today(), note: "dinner".into() }, &[]);
        let games_gone = decision_of(Computer::Allowed, &[GAMES]);

        // Nothing to say about an account that starts out allowed
        assert!(!db.log_decision("kid1", 100, &allowed).unwrap());
        assert!(db.log_decision("kid1", 110, &outside).unwrap());
        assert!(!db.log_decision("kid1", 120, &outside).unwrap());
        assert!(db.log_decision("kid1", 130, &blackout).unwrap());
        assert!(db.log_decision("kid1", 140, &allowed).unwrap());
        assert!(db.log_decision("kid1", 150, &games_gone).unwrap());
        assert!(db.log_decision("kid2", 160, &outside).unwrap());

        let all = db.events(None, 50).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].user, "kid2");
        let kid1 = db.events(Some("kid1"), 50).unwrap();
        let seen: Vec<(&str, &str)> = kid1.iter().rev().map(|e| (e.kind.as_str(), e.detail.as_str())).collect();
        assert_eq!(seen, [("locked", "outside schedule"), ("locked", "blackout: dinner"), ("allowed", ""), ("closed", "Games")]);
        assert_eq!(db.events(Some("kid1"), 2).unwrap().len(), 2);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn prune_removes_rows_older_than_thirty_days() {
        let (mut db, path) = temp_db("prune");
        let now = start_of_test();
        let old = now - 31 * 86400;
        db.record(&sample_report("host-a", 1, old, UserState::Active, &[("steam:1", "Minecraft")])).unwrap();
        db.record(&sample_report("host-a", 2, now, UserState::Active, &[("steam:1", "Minecraft")])).unwrap();
        db.log_decision("kid1", old, &decision_of(Computer::OutsideSchedule, &[])).unwrap();
        db.log_decision("kid1", now, &decision_of(Computer::Allowed, &[])).unwrap();

        db.prune(now).unwrap();
        let count = |table: &str| -> i64 { db.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap() };
        assert_eq!(count("app_activity"), 1);
        assert_eq!(count("event"), 1);
        // Totals don't depend on the pruned tables
        // Two days, each with a host total and one app
        assert_eq!(count("usage"), 4);
        std::fs::remove_file(&path).unwrap();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p server db::`
Expected: compile errors for `log_decision`, `events`, `prune`.

- [ ] **Step 3: Write the implementation**

Append to the schema string:

```sql
             CREATE TABLE IF NOT EXISTS event (
                 id     INTEGER PRIMARY KEY,
                 user   TEXT NOT NULL,
                 at     INTEGER NOT NULL,
                 key    TEXT NOT NULL,
                 kind   TEXT NOT NULL,
                 detail TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS event_user_id ON event (user, id);
```

Below `Blackout`:

```rust
/// A change in what the rules say for an account. Nothing is enforced yet, so these record what
/// would have happened.
#[derive(Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub user: String,
    pub at: i64,
    /// "locked", "closed" or "allowed".
    pub kind: String,
    pub detail: String,
}

const KEEP_SECS: i64 = 30 * 86400;
const ALLOWED_KEY: &str = "allowed|";

fn decision_key(decision: &Decision) -> String {
    let computer = match &decision.computer {
        Computer::Allowed => "allowed",
        Computer::OutsideSchedule => "outside_schedule",
        Computer::Blackout { .. } => "blackout",
    };
    let used_up: Vec<String> =
        decision.categories.iter().filter(|c| c.used_up).map(|c| c.category.to_string()).collect();
    format!("{computer}|{}", used_up.join(","))
}
```

(`use crate::rules::Computer;` joins the existing `rules` import.)

Methods on `Db`:

```rust
    /// Adds an event if the decision differs from the account's last logged one.
    pub fn log_decision(&mut self, user: &str, at: i64, decision: &Decision) -> Result<bool> {
        let key = decision_key(decision);
        let last: String = self
            .conn
            .query_row("SELECT key FROM event WHERE user = ?1 ORDER BY id DESC LIMIT 1", [user], |r| r.get(0))
            .optional()?
            .unwrap_or_else(|| ALLOWED_KEY.to_string());
        if key == last {
            return Ok(false);
        }
        let (last_computer, last_used_up) = last.split_once('|').unwrap_or((&last, ""));
        let (computer, _) = key.split_once('|').unwrap_or((&key, ""));

        let (kind, detail) = if computer != "allowed" && computer != last_computer {
            let detail = match &decision.computer {
                Computer::Blackout { note, .. } if !note.is_empty() => format!("blackout: {note}"),
                Computer::Blackout { .. } => "blackout".to_string(),
                _ => "outside schedule".to_string(),
            };
            ("locked", detail)
        } else {
            let before: Vec<&str> = last_used_up.split(',').collect();
            let names = self.categories()?;
            let newly: Vec<String> = decision
                .categories
                .iter()
                .filter(|c| c.used_up && !before.contains(&c.category.to_string().as_str()))
                .map(|c| names.iter().find(|(id, _)| *id == c.category).map_or_else(|| c.category.to_string(), |(_, n)| n.clone()))
                .collect();
            if newly.is_empty() { ("allowed", String::new()) } else { ("closed", newly.join(", ")) }
        };
        self.conn.execute(
            "INSERT INTO event (user, at, key, kind, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user, at, key, kind, detail],
        )?;
        Ok(true)
    }

    /// Newest first.
    pub fn events(&self, user: Option<&str>, limit: u32) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user, at, kind, detail FROM event WHERE ?1 IS NULL OR user = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![user, limit], |r| {
                Ok(Event { id: r.get(0)?, user: r.get(1)?, at: r.get(2)?, kind: r.get(3)?, detail: r.get(4)? })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Drops per-app stretches and events older than 30 days. Daily totals are kept.
    pub fn prune(&mut self, now: i64) -> Result<()> {
        self.conn.execute("DELETE FROM app_activity WHERE end < ?1", [now - KEEP_SECS])?;
        self.conn.execute("DELETE FROM event WHERE at < ?1", [now - KEEP_SECS])?;
        Ok(())
    }
```

Note on the "allowed" case in the test: going from `blackout` to `allowed|` takes the `else` branch with
nothing newly used up, giving `("allowed", "")`. Going from a blocked state to "allowed with Games used up"
logs `closed`, which is the more useful line.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p server db::`
Expected: 17 tests pass.

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt
git add crates/server
git commit -m "feat(server): log what the rules would have done"
```

---

### Task 5: Login check

**Files:**
- Create: `crates/server/src/auth.rs`
- Modify: `crates/server/src/main.rs` (`mod auth;`, config fields; the middleware is attached in Task 6)
- Modify: `crates/server/Cargo.toml` (via `cargo add`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces (in `auth`):
  - `pub struct AccessConfig { pub team: String, pub aud: String }` (`team` has no trailing slash)
  - `pub fn access_config(team: Option<String>, aud: Option<String>) -> anyhow::Result<Option<AccessConfig>>` (both → `Some`, neither → `None`, one → error)
  - `pub struct Auth` with
    `pub fn new(config: AccessConfig) -> Auth` (fetches keys over HTTPS on demand),
    `pub fn with_keys(config: AccessConfig, keys: JwkSet) -> Auth` (never fetches; for tests),
    `pub async fn check(&self, token: Option<&str>) -> Result<(), StatusCode>` (`UNAUTHORIZED` or `SERVICE_UNAVAILABLE`)
  - `pub async fn require_login(State(auth): State<Option<Arc<Auth>>>, request: Request, next: Next) -> Response` (axum middleware; passes everything through when `auth` is `None`)
  - `pub const TOKEN_HEADER: &str = "cf-access-jwt-assertion";`

- [ ] **Step 1: Add the dependencies**

```bash
cargo add -p server jsonwebtoken --features rust_crypto
cargo add -p server reqwest --no-default-features --features json,rustls
cargo add -p server --dev rsa rand base64
```

The code below is written for jsonwebtoken 10 (`rust_crypto` backend feature, `jwk::JwkSet`,
`DecodingKey::from_jwk`). If `cargo add` resolves a different major version, check its changelog for those
three names before continuing, and for the `rsa`/`rand` pairing use the `rand` version `rsa` re-exports
(`rsa::rand_core`) if the two disagree.

- [ ] **Step 2: Write the failing tests**

Create `crates/server/src/auth.rs` with only this module at the bottom, and add `mod auth;` to `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use serde_json::json;

    const TEAM: &str = "https://team.example.com";
    const AUD: &str = "test-aud";

    /// One key pair for the whole test binary: generating RSA keys is slow in debug builds.
    fn key() -> &'static (EncodingKey, JwkSet) {
        static KEY: OnceLock<(EncodingKey, JwkSet)> = OnceLock::new();
        KEY.get_or_init(|| {
            let private = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
            let pem = private.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();
            let jwks = json!({ "keys": [{
                "kty": "RSA", "alg": "RS256", "use": "sig", "kid": "key-1",
                "n": URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(private.e().to_bytes_be()),
            }]});
            (EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(), serde_json::from_value(jwks).unwrap())
        })
    }

    fn token(kid: &str, iss: &str, aud: &str, expires_in: i64) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        let exp = chrono::Utc::now().timestamp() + expires_in;
        let claims = json!({ "iss": iss, "aud": [aud], "exp": exp, "email": "parent@example.com" });
        jsonwebtoken::encode(&header, &claims, &key().0).unwrap()
    }

    fn auth() -> Auth {
        Auth::with_keys(AccessConfig { team: TEAM.into(), aud: AUD.into() }, key().1.clone())
    }

    #[tokio::test]
    async fn a_valid_token_passes() {
        assert_eq!(auth().check(Some(&token("key-1", TEAM, AUD, 600))).await, Ok(()));
    }

    #[tokio::test]
    async fn bad_tokens_are_unauthorized() {
        let a = auth();
        let unauthorized = Err(StatusCode::UNAUTHORIZED);
        assert_eq!(a.check(None).await, unauthorized);
        assert_eq!(a.check(Some("not-a-token")).await, unauthorized);
        assert_eq!(a.check(Some(&token("key-1", TEAM, AUD, -600))).await, unauthorized, "expired");
        assert_eq!(a.check(Some(&token("key-1", TEAM, "other-aud", 600))).await, unauthorized, "audience");
        assert_eq!(a.check(Some(&token("key-1", "https://evil.example.com", AUD, 600))).await, unauthorized, "issuer");
        assert_eq!(a.check(Some(&token("key-9", TEAM, AUD, 600))).await, unauthorized, "unknown key id");

        // A token whose payload was changed after signing
        let good = token("key-1", TEAM, AUD, 600);
        let mut parts: Vec<&str> = good.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(json!({ "iss": TEAM, "aud": [AUD], "exp": 9_999_999_999i64 }).to_string());
        parts[1] = &forged;
        assert_eq!(a.check(Some(&parts.join("."))).await, unauthorized, "signature");
    }

    #[tokio::test]
    async fn an_unsigned_token_is_rejected() {
        let payload = URL_SAFE_NO_PAD.encode(json!({ "iss": TEAM, "aud": [AUD], "exp": 9_999_999_999i64 }).to_string());
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","kid":"key-1"}"#);
        assert_eq!(auth().check(Some(&format!("{header}.{payload}."))).await, Err(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn refetch_is_throttled() {
        let mut gate = RefetchGate::default();
        let t0 = std::time::Instant::now();
        assert!(gate.allow(t0));
        assert!(!gate.allow(t0 + std::time::Duration::from_secs(30)));
        assert!(gate.allow(t0 + std::time::Duration::from_secs(61)));
    }

    #[test]
    fn config_needs_both_settings_or_neither() {
        assert!(access_config(None, None).unwrap().is_none());
        let both = access_config(Some("https://team.example.com/".into()), Some(AUD.into())).unwrap().unwrap();
        assert_eq!(both.team, TEAM, "trailing slash removed");
        assert!(access_config(Some(TEAM.into()), None).is_err());
        assert!(access_config(None, Some(AUD.into())).is_err());
        // An empty string counts as unset, so an empty environment variable doesn't half-configure it
        assert!(access_config(Some(String::new()), Some(String::new())).unwrap().is_none());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p server auth::`
Expected: compile errors, `Auth`, `AccessConfig`, `RefetchGate`, `access_config` not found.

- [ ] **Step 4: Write the implementation**

Above the tests in `auth.rs`:

```rust
//! Login check: verifies the signed token the reverse proxy adds to requests that passed its login.
//!
//! Only the signature is trusted. Plain identity headers are ignored, because the server can also be
//! reached without going through the proxy.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use tokio::sync::Mutex;

pub const TOKEN_HEADER: &str = "cf-access-jwt-assertion";
const REFETCH_EVERY: Duration = Duration::from_secs(60);

pub struct AccessConfig {
    /// The identity provider's base URL, which is also the token's issuer. No trailing slash.
    pub team: String,
    /// The application's audience tag.
    pub aud: String,
}

/// Both settings turn the login check on, neither leaves it off, and one alone is a mistake.
pub fn access_config(team: Option<String>, aud: Option<String>) -> Result<Option<AccessConfig>> {
    let team = team.filter(|s| !s.is_empty());
    let aud = aud.filter(|s| !s.is_empty());
    match (team, aud) {
        (Some(team), Some(aud)) => Ok(Some(AccessConfig { team: team.trim_end_matches('/').to_string(), aud })),
        (None, None) => Ok(None),
        _ => bail!("the login check needs both access_team and access_aud (or KIDTIME_ACCESS_TEAM and KIDTIME_ACCESS_AUD)"),
    }
}

/// Allows a key refetch at most once a minute, so tokens with made-up key ids can't cause a fetch each.
#[derive(Default)]
struct RefetchGate {
    last: Option<Instant>,
}

impl RefetchGate {
    fn allow(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|last| now.duration_since(last) < REFETCH_EVERY) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

struct Keys {
    set: Option<JwkSet>,
    gate: RefetchGate,
}

pub struct Auth {
    config: AccessConfig,
    keys: Mutex<Keys>,
    /// None in tests: the keys given up front are all there is.
    client: Option<reqwest::Client>,
}

#[derive(serde::Deserialize)]
struct Claims {}

enum Rejected {
    /// Signed with a key id we don't have: worth one refetch.
    UnknownKey,
    Invalid,
}

impl Auth {
    pub fn new(config: AccessConfig) -> Self {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("HTTP client");
        Self { config, keys: Mutex::new(Keys { set: None, gate: RefetchGate::default() }), client: Some(client) }
    }

    pub fn with_keys(config: AccessConfig, keys: JwkSet) -> Self {
        Self { config, keys: Mutex::new(Keys { set: Some(keys), gate: RefetchGate::default() }), client: None }
    }

    pub async fn check(&self, token: Option<&str>) -> Result<(), StatusCode> {
        let token = token.ok_or(StatusCode::UNAUTHORIZED)?;
        let mut keys = self.keys.lock().await;
        if keys.set.is_none() {
            keys.set = Some(self.fetch().await.ok_or(StatusCode::SERVICE_UNAVAILABLE)?);
        }
        match self.verify(token, keys.set.as_ref().expect("keys were just loaded")) {
            Ok(()) => Ok(()),
            Err(Rejected::Invalid) => Err(StatusCode::UNAUTHORIZED),
            Err(Rejected::UnknownKey) => {
                // The provider rotates its keys; try once with fresh ones
                if !keys.gate.allow(Instant::now()) {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                let Some(fresh) = self.fetch().await else {
                    return Err(StatusCode::UNAUTHORIZED);
                };
                keys.set = Some(fresh);
                self.verify(token, keys.set.as_ref().expect("keys were just loaded")).map_err(|_| StatusCode::UNAUTHORIZED)
            }
        }
    }

    fn verify(&self, token: &str, keys: &JwkSet) -> Result<(), Rejected> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| Rejected::Invalid)?;
        let kid = header.kid.ok_or(Rejected::Invalid)?;
        let jwk = keys.find(&kid).ok_or(Rejected::UnknownKey)?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| Rejected::Invalid)?;
        // RS256 only: the algorithm comes from here, never from the token's own header
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.config.aud]);
        validation.set_issuer(&[&self.config.team]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        jsonwebtoken::decode::<Claims>(token, &key, &validation).map(|_| ()).map_err(|_| Rejected::Invalid)
    }

    async fn fetch(&self) -> Option<JwkSet> {
        let client = self.client.as_ref()?;
        let url = format!("{}/cdn-cgi/access/certs", self.config.team);
        let result = async { client.get(&url).send().await?.error_for_status()?.json::<JwkSet>().await }.await;
        match result {
            Ok(keys) => Some(keys),
            Err(e) => {
                tracing::error!("fetching login keys from {url}: {e}");
                None
            }
        }
    }
}

/// Lets a request through only with a valid login token. Does nothing when the check is off.
pub async fn require_login(State(auth): State<Option<Arc<Auth>>>, request: Request, next: Next) -> Response {
    if let Some(auth) = auth {
        let token = request.headers().get(TOKEN_HEADER).and_then(|v| v.to_str().ok());
        if let Err(status) = auth.check(token).await {
            return status.into_response();
        }
    }
    next.run(request).await
}
```

In `main.rs`, add to `Config`:

```rust
    /// Login check: the identity provider's base URL. Set together with `access_aud`.
    #[serde(default)]
    access_team: Option<String>,
    /// Login check: the application's audience tag.
    #[serde(default)]
    access_aud: Option<String>,
```

and in `load_config`, after the `KIDTIME_AGENT_TOKEN` override:

```rust
    if let Ok(v) = std::env::var("KIDTIME_ACCESS_TEAM") {
        config.access_team = Some(v);
    }
    if let Ok(v) = std::env::var("KIDTIME_ACCESS_AUD") {
        config.access_aud = Some(v);
    }
```

In `main`, after `load_config`:

```rust
    let access = auth::access_config(config.access_team.clone(), config.access_aud.clone())?;
    if access.is_none() {
        tracing::warn!("login check is OFF: anyone who can reach this server can read and change everything");
    }
    let auth = access.map(|c| Arc::new(auth::Auth::new(c)));
```

Add `auth: Option<Arc<auth::Auth>>` to `AppState` and set it. It is unused until Task 6; allow dead code on
the field for now.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p server auth::`
Expected: 5 tests pass (the first run takes a few seconds for key generation).

Then check the start-up behaviour by hand:

```bash
cargo build -p server
KIDTIME_AGENT_TOKEN=dev KIDTIME_DB=/tmp/claude-kt.db KIDTIME_LISTEN=127.0.0.1:18470 KIDTIME_ACCESS_TEAM=https://team.example.com target/debug/kidtime-server
```
Expected: exits with "the login check needs both access_team and access_aud".

- [ ] **Step 6: Commit**

```bash
cargo +nightly fmt
git add crates/server Cargo.lock
git commit -m "feat(server): verify the reverse proxy's login token"
```

---

### Task 6: API, router and status

**Files:**
- Create: `crates/server/src/api.rs`
- Modify: `crates/server/src/main.rs`
- Modify: `crates/server/src/rules.rs`, `crates/server/src/db.rs` (remove the temporary `allow(dead_code)`)

**Interfaces:**
- Consumes: everything produced by Tasks 1–5.
- Produces:
  - `pub fn router(state: Arc<AppState>) -> Router` in `main.rs`, used by `main` and by tests.
  - `AppState` fields are `pub(crate)`: `db: Mutex<db::Db>`, `agent_token: String`, `live: Mutex<HashMap<String, LiveHost>>`, `auth: Option<Arc<auth::Auth>>`.
  - `GET /api/status`: each user gains `"restricted": bool` and `"decision": Decision | null`.
  - The endpoints in the spec's API table, with these bodies:
    - `GET /api/rules/{user}` → `{"user": "kid1", "days": [DayRule × 7]}`
    - `PUT /api/rules/{user}/{weekday}` body `DayRule` → 204
    - `POST /api/rules/{user}/copy` body `{"from": 0, "to": [1,2,3,4]}` → 204
    - `GET /api/blackouts` → `[Blackout]`
    - `POST /api/blackouts` body `{"user": "kid1" | null, "start": "2026-10-05T17:00", "end": "2026-10-05T19:00", "note": ""}` → 201 `{"id": 1}`
    - `DELETE /api/blackouts/{id}` → 204, or 404
    - `GET /api/apps` → `[AppEntry]`; `PUT /api/apps/{id}` body `{"category_id": 1 | null}` → 204, or 404
    - `GET /api/categories` → `[{"id": 1, "name": "Games"}]`
    - `GET /api/events?user=kid1` → `[Event]` (at most 50)
  - Errors: 422 `{"error", "field"}`; 404 for an unknown account, app or blackout.

Date-times in request bodies accept `YYYY-MM-DDTHH:MM` (what `<input type="datetime-local">` sends) and
`YYYY-MM-DDTHH:MM:SS`.

- [ ] **Step 1: Add test dependencies**

```bash
cargo add -p server --dev tower --features util
cargo add -p server --dev http-body-util
```

- [ ] **Step 2: Write the failing tests**

Create `crates/server/src/api.rs` with this test module (and `mod api;` in `main.rs`):

```rust
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::{AppState, db, router};

    struct TestApp {
        state: Arc<AppState>,
        path: std::path::PathBuf,
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn app(name: &str) -> TestApp {
        let path = std::env::temp_dir().join(format!("kidtime-api-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let state = Arc::new(AppState {
            db: Mutex::new(db::Db::open(&path).unwrap()),
            agent_token: "token".into(),
            live: Mutex::new(HashMap::new()),
            auth: None,
        });
        TestApp { state, path }
    }

    async fn call(app: &TestApp, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = router(app.state.clone()).oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Makes `kid1` a known account with one app in the catalogue.
    async fn report(app: &TestApp, app_id: &str) {
        let at = chrono::Local::now().timestamp();
        let body = json!({ "host": "host-a", "agent_id": "a", "interval_secs": 15, "samples": [{
            "seq": 1, "at": at, "elapsed_secs": 15,
            "users": [{ "user": "kid1", "state": "active", "apps": [{ "id": app_id, "name": "Some App" }] }],
        }]});
        let request = Request::builder()
            .method("POST")
            .uri("/api/report")
            .header("content-type", "application/json")
            .header("authorization", "Bearer token")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(app.state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn rules_round_trip_and_copy() {
        let app = app("rules");
        report(&app, "steam:1").await;
        let rule = json!({ "restricted": true, "stretches": [{ "start_min": 375, "end_min": 1200 }], "budgets": { "1": 60 } });
        assert_eq!(call(&app, "PUT", "/api/rules/kid1/0", Some(rule.clone())).await.0, StatusCode::NO_CONTENT);
        let copy = json!({ "from": 0, "to": [1, 4] });
        assert_eq!(call(&app, "POST", "/api/rules/kid1/copy", Some(copy)).await.0, StatusCode::NO_CONTENT);

        let (status, body) = call(&app, "GET", "/api/rules/kid1", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["days"].as_array().unwrap().len(), 7);
        assert_eq!(body["days"][0], rule);
        assert_eq!(body["days"][4], rule);
        assert_eq!(body["days"][2]["restricted"], false);
    }

    #[tokio::test]
    async fn rules_reject_bad_input() {
        let app = app("rules-bad");
        report(&app, "steam:1").await;
        let backwards = json!({ "restricted": true, "stretches": [{ "start_min": 700, "end_min": 600 }] });
        let (status, body) = call(&app, "PUT", "/api/rules/kid1/0", Some(backwards)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["field"], "stretches");
        assert!(body["error"].as_str().unwrap().contains("end after"));

        let ok = json!({ "restricted": false });
        assert_eq!(call(&app, "PUT", "/api/rules/kid1/7", Some(ok.clone())).await.1["field"], "weekday");
        assert_eq!(call(&app, "PUT", "/api/rules/nobody/0", Some(ok.clone())).await.0, StatusCode::NOT_FOUND);
        let unknown_category = json!({ "restricted": false, "budgets": { "99": 10 } });
        assert_eq!(call(&app, "PUT", "/api/rules/kid1/0", Some(unknown_category)).await.1["field"], "budgets");
        let bad_copy = json!({ "from": 0, "to": [9] });
        assert_eq!(call(&app, "POST", "/api/rules/kid1/copy", Some(bad_copy)).await.1["field"], "to");
    }

    #[tokio::test]
    async fn blackouts_add_list_delete() {
        let app = app("blackouts");
        report(&app, "steam:1").await;
        let body = json!({ "user": "kid1", "start": "2099-01-01T17:00", "end": "2099-01-01T19:00", "note": "dinner" });
        let (status, created) = call(&app, "POST", "/api/blackouts", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = created["id"].as_i64().unwrap();

        let (_, list) = call(&app, "GET", "/api/blackouts", None).await;
        assert_eq!(list[0]["note"], "dinner");
        assert_eq!(list[0]["start"], "2099-01-01T17:00:00");

        let backwards = json!({ "user": null, "start": "2099-01-01T19:00", "end": "2099-01-01T17:00", "note": "" });
        assert_eq!(call(&app, "POST", "/api/blackouts", Some(backwards)).await.1["field"], "end");
        let unknown = json!({ "user": "nobody", "start": "2099-01-01T17:00", "end": "2099-01-01T19:00", "note": "" });
        assert_eq!(call(&app, "POST", "/api/blackouts", Some(unknown)).await.1["field"], "user");
        let garbled = json!({ "user": null, "start": "tomorrow", "end": "2099-01-01T19:00", "note": "" });
        assert_eq!(call(&app, "POST", "/api/blackouts", Some(garbled)).await.1["field"], "start");

        assert_eq!(call(&app, "DELETE", &format!("/api/blackouts/{id}"), None).await.0, StatusCode::NO_CONTENT);
        assert_eq!(call(&app, "DELETE", &format!("/api/blackouts/{id}"), None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn apps_can_be_categorised_whatever_their_id() {
        let app = app("apps");
        let id = "window:A/B: Game? 100%";
        report(&app, id).await;
        let (_, list) = call(&app, "GET", "/api/apps", None).await;
        assert_eq!(list[0]["app_id"], id);
        assert_eq!(list[0]["reviewed"], false);

        // Percent-encoded the way encodeURIComponent does it
        let uri = "/api/apps/window%3AA%2FB%3A%20Game%3F%20100%25";
        assert_eq!(call(&app, "PUT", uri, Some(json!({ "category_id": 1 }))).await.0, StatusCode::NO_CONTENT);
        let (_, list) = call(&app, "GET", "/api/apps", None).await;
        assert_eq!(list[0]["category_id"], 1);
        assert_eq!(list[0]["reviewed"], true);

        assert_eq!(call(&app, "PUT", "/api/apps/nope", Some(json!({ "category_id": null }))).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&app, "PUT", uri, Some(json!({ "category_id": 99 }))).await.1["field"], "category_id");
        let (_, categories) = call(&app, "GET", "/api/categories", None).await;
        assert_eq!(categories, json!([{ "id": 1, "name": "Games" }]));
    }

    #[tokio::test]
    async fn status_carries_the_decision_and_reports_log_events() {
        let app = app("status");
        report(&app, "steam:1").await;
        let (_, status) = call(&app, "GET", "/api/status", None).await;
        assert_eq!(status["users"][0]["restricted"], false);
        assert_eq!(status["users"][0]["decision"]["computer"]["state"], "allowed");

        // No stretches on any day: never allowed
        for weekday in 0..7 {
            let rule = json!({ "restricted": true, "stretches": [] });
            call(&app, "PUT", &format!("/api/rules/kid1/{weekday}"), Some(rule)).await;
        }
        let (_, status) = call(&app, "GET", "/api/status", None).await;
        assert_eq!(status["users"][0]["restricted"], true);
        assert_eq!(status["users"][0]["decision"]["computer"]["state"], "outside_schedule");

        // The next report notices the change
        report(&app, "steam:1").await;
        let (_, events) = call(&app, "GET", "/api/events?user=kid1", None).await;
        assert_eq!(events[0]["kind"], "locked");
        assert_eq!(events[0]["detail"], "outside schedule");
        assert_eq!(call(&app, "GET", "/api/events", None).await.1.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn with_the_login_check_on_only_report_and_health_are_open() {
        use crate::auth::{AccessConfig, Auth};
        let mut app = app("login");
        let keys = serde_json::from_value(json!({ "keys": [] })).unwrap();
        let auth = Auth::with_keys(AccessConfig { team: "https://team.example.com".into(), aud: "test-aud".into() }, keys);
        Arc::get_mut(&mut app.state).unwrap().auth = Some(Arc::new(auth));

        for (method, uri) in [
            ("GET", "/"), ("GET", "/app.js"), ("GET", "/manage.js"), ("GET", "/manifest.webmanifest"),
            ("GET", "/api/status"), ("GET", "/api/rules/kid1"), ("PUT", "/api/rules/kid1/0"),
            ("POST", "/api/rules/kid1/copy"), ("GET", "/api/blackouts"), ("POST", "/api/blackouts"),
            ("DELETE", "/api/blackouts/1"), ("GET", "/api/apps"), ("PUT", "/api/apps/x"),
            ("GET", "/api/categories"), ("GET", "/api/events"),
        ] {
            let (status, body) = call(&app, method, uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
            assert_eq!(body, Value::Null, "{method} {uri} must have an empty body");
        }
        assert_eq!(call(&app, "GET", "/healthz", None).await.0, StatusCode::OK);
        report(&app, "steam:1").await;
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p server api::`
Expected: compile errors: `router` not found, `AppState` fields private, no `auth` field.

- [ ] **Step 4: Write the handlers**

Above the tests in `api.rs`:

```rust
//! JSON handlers for rules, blackouts, the app catalogue and the would-have log.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{Local, NaiveDateTime};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::rules::{self, CategoryId, DayRule, Invalid};

pub enum ApiError {
    NotFound,
    Invalid(Invalid),
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => StatusCode::NOT_FOUND.into_response(),
            ApiError::Invalid(i) => {
                (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": i.message, "field": i.field }))).into_response()
            }
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("api: {e:#}");
        ApiError::Internal
    }
}

impl From<Invalid> for ApiError {
    fn from(i: Invalid) -> Self {
        ApiError::Invalid(i)
    }
}

fn invalid(field: &'static str, message: &str) -> ApiError {
    ApiError::Invalid(Invalid { field, message: message.into() })
}

type Api<T> = Result<T, ApiError>;

fn check_weekday(weekday: u8) -> Api<()> {
    if weekday > 6 { Err(invalid("weekday", "weekday must be 0 (Monday) to 6 (Sunday)")) } else { Ok(()) }
}

pub async fn get_rules(State(state): State<Arc<AppState>>, Path(user): Path<String>) -> Api<Json<serde_json::Value>> {
    let db = state.db.lock().unwrap();
    if !db.is_account(&user)? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(json!({ "user": user, "days": db.week_rules(&user)? })))
}

pub async fn put_rule(
    State(state): State<Arc<AppState>>,
    Path((user, weekday)): Path<(String, u8)>,
    Json(rule): Json<DayRule>,
) -> Api<StatusCode> {
    check_weekday(weekday)?;
    rules::validate_day(&rule)?;
    let mut db = state.db.lock().unwrap();
    if !db.is_account(&user)? {
        return Err(ApiError::NotFound);
    }
    let known: Vec<CategoryId> = db.categories()?.into_iter().map(|(id, _)| id).collect();
    if rule.budgets.keys().any(|id| !known.contains(id)) {
        return Err(invalid("budgets", "unknown category"));
    }
    db.set_day_rule(&user, weekday, &rule)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct CopyRule {
    from: u8,
    to: Vec<u8>,
}

pub async fn copy_rule(
    State(state): State<Arc<AppState>>,
    Path(user): Path<String>,
    Json(copy): Json<CopyRule>,
) -> Api<StatusCode> {
    if copy.from > 6 {
        return Err(invalid("from", "weekday must be 0 (Monday) to 6 (Sunday)"));
    }
    if copy.to.iter().any(|&d| d > 6) {
        return Err(invalid("to", "weekday must be 0 (Monday) to 6 (Sunday)"));
    }
    let mut db = state.db.lock().unwrap();
    if !db.is_account(&user)? {
        return Err(ApiError::NotFound);
    }
    let rule = db.day_rule(&user, copy.from)?;
    for weekday in copy.to {
        db.set_day_rule(&user, weekday, &rule)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_blackouts(State(state): State<Arc<AppState>>) -> Api<Json<Vec<crate::db::Blackout>>> {
    Ok(Json(state.db.lock().unwrap().blackouts(Local::now().naive_local())?))
}

#[derive(Deserialize)]
pub struct NewBlackout {
    user: Option<String>,
    start: String,
    end: String,
    #[serde(default)]
    note: String,
}

/// What `<input type="datetime-local">` sends, with or without seconds.
fn parse_local(field: &'static str, text: &str) -> Api<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .map_err(|_| invalid(field, "not a date and time"))
}

pub async fn post_blackout(State(state): State<Arc<AppState>>, Json(new): Json<NewBlackout>) -> Api<Response> {
    let start = parse_local("start", &new.start)?;
    let end = parse_local("end", &new.end)?;
    rules::validate_blackout(start, end)?;
    let mut db = state.db.lock().unwrap();
    if let Some(user) = &new.user
        && !db.is_account(user)?
    {
        return Err(invalid("user", "unknown account"));
    }
    let id = db.add_blackout(new.user.as_deref(), start, end, new.note.trim())?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

pub async fn delete_blackout(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Api<StatusCode> {
    if state.db.lock().unwrap().delete_blackout(id)? { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::NotFound) }
}

pub async fn get_apps(State(state): State<Arc<AppState>>) -> Api<Json<Vec<crate::db::AppEntry>>> {
    Ok(Json(state.db.lock().unwrap().apps()?))
}

#[derive(Deserialize)]
pub struct SetCategory {
    category_id: Option<CategoryId>,
}

pub async fn put_app(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(set): Json<SetCategory>,
) -> Api<StatusCode> {
    let mut db = state.db.lock().unwrap();
    if let Some(id) = set.category_id
        && !db.categories()?.iter().any(|(known, _)| *known == id)
    {
        return Err(invalid("category_id", "unknown category"));
    }
    if db.set_app_category(&app_id, set.category_id)? { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::NotFound) }
}

pub async fn get_categories(State(state): State<Arc<AppState>>) -> Api<Json<serde_json::Value>> {
    let categories = state.db.lock().unwrap().categories()?;
    Ok(Json(categories.into_iter().map(|(id, name)| json!({ "id": id, "name": name })).collect()))
}

#[derive(Deserialize)]
pub struct EventQuery {
    user: Option<String>,
}

pub async fn get_events(State(state): State<Arc<AppState>>, Query(query): Query<EventQuery>) -> Api<Json<Vec<crate::db::Event>>> {
    Ok(Json(state.db.lock().unwrap().events(query.user.as_deref(), 50)?))
}
```

- [ ] **Step 5: Restructure `main.rs`**

Make `AppState` and its fields `pub(crate)` (and `LiveHost` `pub(crate)`). Replace the inline `Router::new()`
in `main` with `let app = router(state);` and add:

```rust
/// Everything except the agents' endpoint and the health check sits behind the login check.
pub(crate) fn router(state: Arc<AppState>) -> Router {
    use axum::routing::{delete, put};

    let behind_login = Router::new()
        .route("/api/status", get(status))
        .route("/api/rules/{user}", get(api::get_rules))
        .route("/api/rules/{user}/copy", post(api::copy_rule))
        .route("/api/rules/{user}/{weekday}", put(api::put_rule))
        .route("/api/blackouts", get(api::get_blackouts).post(api::post_blackout))
        .route("/api/blackouts/{id}", delete(api::delete_blackout))
        .route("/api/apps", get(api::get_apps))
        .route("/api/apps/{id}", put(api::put_app))
        .route("/api/categories", get(api::get_categories))
        .route("/api/events", get(api::get_events))
        .route("/", get(|| asset("index.html")))
        .route("/{file}", get(|axum::extract::Path(file): axum::extract::Path<String>| asset_owned(file)))
        // The login check runs before any handler's extractors, so a bad body can't answer before it
        .layer(axum::middleware::from_fn_with_state(state.auth.clone(), auth::require_login));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/report", post(report))
        .merge(behind_login)
        .with_state(state)
}
```

If the `PUT`/`POST` routes return 422 or 415 before 401 in the login test, the layer is in the wrong place:
the middleware must run before the handler's extractors, which `.layer` on the sub-router guarantees.

In `report`, after the `record` call succeeds, log decisions for every account in the report:

```rust
    {
        let mut db = state.db.lock().unwrap();
        let now_local = Local::now().naive_local();
        let mut users: Vec<&str> =
            report.samples.iter().flat_map(|s| s.users.iter().map(|u| u.user.as_str())).collect();
        users.sort_unstable();
        users.dedup();
        for user in users {
            // A failure here must not make the agent resend a report that was already recorded
            let logged = db.decision(user, now_local).and_then(|d| db.log_decision(user, now(), &d));
            if let Err(e) = logged {
                tracing::error!("logging the decision for {user}: {e:#}");
            }
        }
    }
```

Fold the `record` call into the same lock scope so the database is locked once.

In `UserStatus` add:

```rust
    /// Whether the account has any rule.
    restricted: bool,
    /// None if it couldn't be worked out; the other accounts are still returned.
    decision: Option<rules::Decision>,
```

and in `status`, when building each user:

```rust
        let now_local = Local::now().naive_local();
        let decision = db
            .decision(&name, now_local)
            .inspect_err(|e| tracing::error!("decision for {name}: {e:#}"))
            .ok();
        let restricted = db.is_restricted(&name).map_err(internal)?;
```

Add `"manage.js"` to `asset()`:

```rust
        "manage.js" => (include_bytes!("../static/manage.js"), "text/javascript; charset=utf-8"),
```

and create an empty `crates/server/static/manage.js` containing only `"use strict";` so the build passes;
Task 7 fills it.

Daily pruning, in `main` before `axum::serve`:

```rust
    let pruner = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(86400));
        loop {
            tick.tick().await;
            if let Err(e) = pruner.db.lock().unwrap().prune(now()) {
                tracing::error!("pruning old rows: {e:#}");
            }
        }
    });
```

(`state` must be cloned before it is moved into `router`. tokio needs its `time` feature:
`cargo add -p server tokio --features time`.)

Remove every temporary `allow(dead_code)` added in Tasks 1, 3 and 5.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p server`
Expected: all pass: 13 rules, 17 db, 5 auth, 6 api.

Run: `cargo +stable clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Check against a running server**

```bash
cargo build -p server
rm -f /tmp/claude-kt.db*
KIDTIME_AGENT_TOKEN=dev KIDTIME_DB=/tmp/claude-kt.db KIDTIME_LISTEN=127.0.0.1:18470 target/debug/kidtime-server &
sleep 1
curl -s -X POST localhost:18470/api/report -H 'Authorization: Bearer dev' -H 'Content-Type: application/json' \
  -d "{\"host\":\"host-a\",\"agent_id\":\"a\",\"interval_secs\":15,\"samples\":[{\"seq\":1,\"at\":$(date +%s),\"elapsed_secs\":15,\"users\":[{\"user\":\"kid1\",\"state\":\"active\",\"apps\":[{\"id\":\"steam:1\",\"name\":\"Minecraft\"}]}]}]}"
curl -s -X PUT localhost:18470/api/rules/kid1/$(( ($(date +%u) + 6) % 7 )) -H 'Content-Type: application/json' -d '{"restricted":false,"budgets":{"1":0}}'
curl -s localhost:18470/api/status | python3 -m json.tool | grep -A12 '"decision"'
pkill -x kidtime-server
```

Expected: the start-up log has the "login check is OFF" warning, and the decision shows `"state": "allowed"`
with category 1 `"used_up": true`, `"used_secs": 15`.

- [ ] **Step 8: Commit**

```bash
cargo +nightly fmt
git add crates/server Cargo.lock
git commit -m "feat(server): add the rules API and put the dashboard behind the login check"
```

---

### Task 7: Screens

**Files:**
- Modify: `crates/server/static/index.html`, `crates/server/static/app.js`, `crates/server/static/style.css`,
  `crates/server/static/sw.js`
- Create (replace the stub): `crates/server/static/manage.js`

**Interfaces:**
- Consumes: the JSON API from Task 6.
- Produces: three tabs (Today, Rules, Apps). No server changes.

There is no JS test runner in this repo and the spec says the screens are checked by hand. The check list in
Step 7 is the test; do every item.

- [ ] **Step 1: `index.html`**

Replace the `<body>` contents with:

```html
  <header class="top">
    <h1>Kidtime</h1>
    <p class="updated" id="updated" aria-live="polite">Loading…</p>
  </header>
  <nav class="tabs" aria-label="Sections">
    <button type="button" class="tab" data-tab="today" aria-current="page">Today</button>
    <button type="button" class="tab" data-tab="rules">Rules</button>
    <button type="button" class="tab" data-tab="apps">Apps <span id="apps-new" class="badge" hidden></span></button>
  </nav>
  <main>
    <section id="tab-today">
      <div id="users" class="users"></div>
      <section class="card log">
        <h2>What the rules would have done</h2>
        <p class="footnote">Nothing is enforced yet. This shows when kidtime would have acted.</p>
        <ul id="events" class="events"></ul>
      </section>
    </section>
    <section id="tab-rules" hidden></section>
    <section id="tab-apps" hidden></section>
  </main>
  <div id="tooltip" class="tooltip" role="tooltip" hidden></div>
  <script src="/app.js"></script>
  <script src="/manage.js"></script>
```

- [ ] **Step 2: Shared helpers and the Today additions in `app.js`**

Add after the `esc` helper:

```js
// Every API call goes through here. Behind the login proxy an expired session is a redirect to another
// origin, which fetch can't follow usefully; treat it like a 401 and reload into the login.
async function api(path, { method = "GET", body } = {}) {
  const res = await fetch(path, {
    method,
    cache: "no-store",
    redirect: "manual",
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401 || res.type === "opaqueredirect") {
    location.reload();
    throw new Error("signed out");
  }
  if (res.status === 422) {
    const problem = await res.json();
    throw Object.assign(new Error(problem.error), { field: problem.field });
  }
  if (!res.ok) throw new Error(res.statusText || `HTTP ${res.status}`);
  return res.status === 204 ? null : res.json();
}

let categories = [];
const categoryName = (id) => categories.find((c) => c.id === id)?.name ?? "Uncategorised";

function clock(text) {
  // "2026-10-05T20:00:00" is the server's local time; show it as written
  const [date, time] = text.split("T");
  const [h, m] = time.split(":").map(Number);
  const label = `${h % 12 || 12}:${String(m).padStart(2, "0")}${h < 12 ? "am" : "pm"}`;
  const today = new Date();
  const todayText = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, "0")}-${String(today.getDate()).padStart(2, "0")}`;
  if (date === todayText) return label;
  return `${parseDay(date).toLocaleDateString(undefined, { weekday: "short" })} ${label}`;
}

function decisionHtml(u) {
  if (!u.restricted || !u.decision) return "";
  const d = u.decision;
  let line;
  if (d.computer.state === "allowed") line = `Allowed until ${clock(d.next_change)}`;
  else if (d.computer.state === "blackout") line = `Would be locked: blackout until ${clock(d.computer.until)}${d.computer.note ? ` (${esc(d.computer.note)})` : ""}`;
  else line = `Would be locked: outside allowed hours until ${clock(d.next_change)}`;
  const budgets = d.categories.map((c) => c.used_up
    ? `<li><strong>${esc(categoryName(c.category))}</strong> budget used up</li>`
    : `<li><strong>${esc(categoryName(c.category))}</strong> ${duration(c.left_secs)} left</li>`).join("");
  return `<div class="decision"><p>${line}</p>${budgets ? `<ul>${budgets}</ul>` : ""}</div>`;
}

const EVENT_TEXT = {
  locked: (e) => `Would have locked: ${e.detail}`,
  closed: (e) => `Would have closed ${e.detail.toLowerCase()}: ${e.detail} budget used up`,
  allowed: () => "Allowed again",
};

function eventsHtml(events) {
  if (!events.length) return '<li class="empty-note">Nothing yet.</li>';
  return events.map((e) => {
    const when = new Date(e.at * 1000).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" });
    return `<li><span class="event-when">${esc(when)}</span> <span class="event-who">${esc(e.user)}</span> ${esc((EVENT_TEXT[e.kind] ?? (() => e.kind))(e))}</li>`;
  }).join("");
}
```

In `cardHtml`, insert `${decisionHtml(u)}` on the line after the closing `</div>` of `.hero`.

Replace `refresh` with:

```js
async function refresh() {
  try {
    const [status, events] = await Promise.all([api("/api/status"), api("/api/events")]);
    if (!categories.length) categories = await api("/api/categories");
    hideTip();
    usersEl.innerHTML = status.users.length
      ? status.users.map(cardHtml).join("")
      : '<article class="card"><p class="empty-note">No activity reported yet. Is an agent running?</p></article>';
    document.getElementById("events").innerHTML = eventsHtml(events);
    window.kidtimeUsers = status.users.map((u) => u.user);
    updatedEl.classList.remove("error");
    updatedEl.textContent = `Updated ${new Date(status.generated_at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`;
  } catch {
    updatedEl.classList.add("error");
    updatedEl.textContent = "Can't reach server";
  }
}
```

Add tab switching before the `refresh();` call at the bottom:

```js
const tabs = [...document.querySelectorAll(".tab")];
function showTab(name) {
  for (const t of tabs) {
    const on = t.dataset.tab === name;
    if (on) t.setAttribute("aria-current", "page"); else t.removeAttribute("aria-current");
    document.getElementById(`tab-${t.dataset.tab}`).hidden = !on;
  }
  window.dispatchEvent(new CustomEvent("kidtime:tab", { detail: name }));
}
for (const t of tabs) t.addEventListener("click", () => showTab(t.dataset.tab));
```

- [ ] **Step 3: `manage.js`**

Replace the stub with:

```js
"use strict";

// Rules, blackouts and the app catalogue. Shares api(), esc(), duration() and categories with app.js.

const DAYS = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const GAMES = 1;
const rulesEl = document.getElementById("tab-rules");
const appsEl = document.getElementById("tab-apps");
let rulesUser = null;
let week = [];
let editing = null; // weekday being edited, or null

const toClock = (min) => `${String(Math.floor(min / 60)).padStart(2, "0")}:${String(min % 60).padStart(2, "0")}`;
// <input type="time"> can't express 24:00; the editor shows end-of-day as 23:59 and saves it as 1440
const fromClock = (text, isEnd) => {
  const [h, m] = text.split(":").map(Number);
  const min = h * 60 + m;
  return isEnd && min === 1439 ? 1440 : min;
};
const clockLabel = (min) => {
  if (min === 1440) return "midnight";
  const h = Math.floor(min / 60), m = min % 60;
  return `${h % 12 || 12}${m ? `:${String(m).padStart(2, "0")}` : ""}${h < 12 ? "am" : "pm"}`;
};

function daySummary(rule) {
  const hours = !rule.restricted ? "Any time"
    : rule.stretches.length ? rule.stretches.map((s) => `${clockLabel(s.start_min)}–${clockLabel(s.end_min)}`).join(", ")
    : "Not allowed";
  const games = rule.budgets[GAMES];
  const budget = games === undefined ? "no games limit" : games === 0 ? "no games" : `games ${duration(games * 60)}`;
  return `${hours} · ${budget}`;
}

function editorHtml(weekday) {
  const rule = week[weekday];
  const rows = rule.stretches.map((s, i) => `
    <li class="stretch-row">
      <label>From <input type="time" name="start" value="${toClock(s.start_min)}" required></label>
      <label>to <input type="time" name="end" value="${toClock(Math.min(s.end_min, 1439))}" required></label>
      <button type="button" data-remove="${i}" aria-label="Remove this stretch">Remove</button>
    </li>`).join("");
  const games = rule.budgets[GAMES];
  return `
    <form class="editor" data-weekday="${weekday}">
      <label class="switch"><input type="checkbox" name="restricted" ${rule.restricted ? "checked" : ""}> Limit the hours on ${DAYS[weekday]}</label>
      <ul class="stretches" ${rule.restricted ? "" : "hidden"}>${rows}</ul>
      <button type="button" data-add ${rule.restricted ? "" : "hidden"}>Add allowed hours</button>
      <label class="switch"><input type="checkbox" name="limited" ${games === undefined ? "" : "checked"}> Limit games</label>
      <label ${games === undefined ? "hidden" : ""}>Minutes of games <input type="number" name="minutes" min="0" max="1440" step="5" value="${games ?? 60}"></label>
      <p class="form-error" role="alert" hidden></p>
      <div class="editor-actions">
        <button type="submit">Save</button>
        <button type="button" data-cancel>Cancel</button>
      </div>
      <div class="editor-actions">
        <span>Save and copy to:</span>
        <button type="button" data-copy="0,1,2,3,4">Weekdays</button>
        <button type="button" data-copy="5,6">Weekend</button>
        <button type="button" data-copy="0,1,2,3,4,5,6">All days</button>
      </div>
    </form>`;
}

function readEditor(form) {
  const restricted = form.elements.restricted.checked;
  const stretches = restricted ? [...form.querySelectorAll(".stretch-row")].map((row) => ({
    start_min: fromClock(row.querySelector('[name="start"]').value, false),
    end_min: fromClock(row.querySelector('[name="end"]').value, true),
  })) : [];
  const budgets = form.elements.limited.checked ? { [GAMES]: Number(form.elements.minutes.value) } : {};
  return { restricted, stretches, budgets };
}

function blackoutsHtml(blackouts) {
  const rows = blackouts.map((b) => `
    <li>
      <span>${esc(b.user ?? "All kids")}: ${esc(clock(b.start))} to ${esc(clock(b.end))}${b.note ? ` (${esc(b.note)})` : ""}</span>
      <button type="button" data-delete-blackout="${b.id}">Delete</button>
    </li>`).join("");
  const now = new Date(Date.now() - new Date().getTimezoneOffset() * 60000).toISOString().slice(0, 16);
  const who = (window.kidtimeUsers ?? []).map((u) => `<option value="${esc(u)}" ${u === rulesUser ? "selected" : ""}>${esc(u)}</option>`).join("");
  return `
    <h2>Blackouts</h2>
    <ul class="blackouts">${rows || '<li class="empty-note">None.</li>'}</ul>
    <form class="editor" id="blackout-form">
      <label>Who <select name="user"><option value="">All kids</option>${who}</select></label>
      <label>From <input type="datetime-local" name="start" value="${now}" required></label>
      <label>To <input type="datetime-local" name="end" required></label>
      <label>Note <input type="text" name="note" maxlength="80"></label>
      <p class="form-error" role="alert" hidden></p>
      <button type="submit">Add blackout</button>
    </form>`;
}

async function renderRules() {
  const users = window.kidtimeUsers ?? [];
  if (!users.length) { rulesEl.innerHTML = '<article class="card"><p class="empty-note">No accounts have reported yet.</p></article>'; return; }
  rulesUser ??= users[0];
  const [rules, blackouts] = await Promise.all([api(`/api/rules/${encodeURIComponent(rulesUser)}`), api("/api/blackouts")]);
  week = rules.days;
  const picker = users.map((u) => `<button type="button" class="chip" data-user="${esc(u)}" ${u === rulesUser ? 'aria-pressed="true"' : 'aria-pressed="false"'}>${esc(u)}</button>`).join("");
  const days = week.map((rule, i) => editing === i ? `<li>${editorHtml(i)}</li>` : `
    <li><button type="button" class="day-row" data-edit="${i}"><span class="day-name">${DAYS[i]}</span><span class="day-summary">${esc(daySummary(rule))}</span></button></li>`).join("");
  rulesEl.innerHTML = `
    <article class="card">
      <div class="chips" role="group" aria-label="Account">${picker}</div>
      <ul class="week-rules">${days}</ul>
    </article>
    <article class="card">${blackoutsHtml(blackouts)}</article>`;
}

function showError(form, error) {
  const el = form.querySelector(".form-error");
  el.textContent = error.message;
  el.hidden = false;
}

rulesEl.addEventListener("click", async (e) => {
  const t = e.target.closest("button");
  if (!t) return;
  const form = t.closest("form");
  if (t.dataset.user) { rulesUser = t.dataset.user; editing = null; return renderRules(); }
  if (t.dataset.edit !== undefined) { editing = Number(t.dataset.edit); return renderRules(); }
  if (t.dataset.cancel !== undefined) { editing = null; return renderRules(); }
  if (t.dataset.add !== undefined) {
    // Keep what was typed: update the model from the form, then add a row
    week[editing] = readEditor(form);
    week[editing].stretches.push({ start_min: 960, end_min: 1200 });
    return renderRulesKeepingEdits();
  }
  if (t.dataset.remove !== undefined) {
    week[editing] = readEditor(form);
    week[editing].stretches.splice(Number(t.dataset.remove), 1);
    return renderRulesKeepingEdits();
  }
  if (t.dataset.copy) {
    if (!form.reportValidity()) return;
    return saveDay(form, t.dataset.copy.split(",").map(Number));
  }
  if (t.dataset.deleteBlackout) {
    await api(`/api/blackouts/${t.dataset.deleteBlackout}`, { method: "DELETE" });
    return renderRules();
  }
});

// Re-render the open editor from `week` without refetching, so unsaved edits survive
function renderRulesKeepingEdits() {
  const li = rulesEl.querySelector("form[data-weekday]").parentElement;
  li.innerHTML = editorHtml(editing);
}

rulesEl.addEventListener("change", (e) => {
  const form = e.target.closest("form[data-weekday]");
  if (!form || (e.target.name !== "restricted" && e.target.name !== "limited")) return;
  week[editing] = readEditor(form);
  if (e.target.name === "limited" && !e.target.checked) week[editing].budgets = {};
  if (e.target.name === "limited" && e.target.checked) week[editing].budgets = { [GAMES]: 60 };
  renderRulesKeepingEdits();
});

async function saveDay(form, copyTo) {
  const weekday = Number(form.dataset.weekday);
  try {
    await api(`/api/rules/${encodeURIComponent(rulesUser)}/${weekday}`, { method: "PUT", body: readEditor(form) });
    if (copyTo) await api(`/api/rules/${encodeURIComponent(rulesUser)}/copy`, { method: "POST", body: { from: weekday, to: copyTo } });
    editing = null;
    await renderRules();
  } catch (error) {
    // The editor stays open with what was typed
    showError(form, error);
  }
}

rulesEl.addEventListener("submit", async (e) => {
  e.preventDefault();
  const form = e.target;
  if (form.dataset.weekday !== undefined) return saveDay(form, null);
  if (form.id === "blackout-form") {
    const f = form.elements;
    try {
      await api("/api/blackouts", { method: "POST", body: { user: f.user.value || null, start: f.start.value, end: f.end.value, note: f.note.value } });
      await renderRules();
    } catch (error) {
      showError(form, error);
    }
  }
});

async function renderApps() {
  const apps = await api("/api/apps");
  const options = (current) => [`<option value="" ${current === null ? "selected" : ""}>Uncategorised</option>`]
    .concat(categories.map((c) => `<option value="${c.id}" ${current === c.id ? "selected" : ""}>${esc(c.name)}</option>`)).join("");
  const rows = apps.map((a) => `
    <li class="app-row">
      <span class="app-name">${a.reviewed ? "" : '<span class="badge">New</span> '}${esc(a.name)}</span>
      <span class="app-seen">last used ${esc(new Date(a.last_seen * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" }))}</span>
      <select data-app="${esc(a.app_id)}" aria-label="Category for ${esc(a.name)}">${options(a.category_id)}</select>
    </li>`).join("");
  appsEl.innerHTML = `
    <article class="card">
      <p class="footnote">Only apps in a category with a budget use that budget. Steam games and anything played in a stream are set to Games automatically; change any of them here.</p>
      <p class="footnote">At the desktop an app counts while it is open and the kid is active, even in the background. Games launched outside Steam at the desktop may not be detected.</p>
      <ul class="apps">${rows || '<li class="empty-note">No apps seen yet.</li>'}</ul>
    </article>`;
  updateNewBadge(apps);
}

function updateNewBadge(apps) {
  const badge = document.getElementById("apps-new");
  const count = apps.filter((a) => !a.reviewed).length;
  badge.textContent = count;
  badge.hidden = count === 0;
  badge.setAttribute("aria-label", `${count} new`);
}

appsEl.addEventListener("change", async (e) => {
  const select = e.target.closest("select[data-app]");
  if (!select) return;
  await api(`/api/apps/${encodeURIComponent(select.dataset.app)}`, { method: "PUT", body: { category_id: select.value === "" ? null : Number(select.value) } });
  await renderApps();
});

window.addEventListener("kidtime:tab", (e) => {
  if (e.detail === "rules") renderRules().catch(() => {});
  if (e.detail === "apps") renderApps().catch(() => {});
});

// Keep the "new apps" badge current without opening the tab
api("/api/apps").then(updateNewBadge).catch(() => {});
```

- [ ] **Step 4: `style.css`**

Append (uses the existing custom properties; no new colours):

```css
.tabs { display: flex; gap: 4px; padding: 0 16px 12px; }
.tab {
  flex: 1; padding: 8px 10px; border: 1px solid var(--track); border-radius: 8px;
  background: transparent; color: var(--text-secondary); font: inherit;
}
.tab[aria-current="page"] { background: var(--track); color: var(--text-primary); font-weight: 600; }
.badge { display: inline-block; padding: 0 6px; border-radius: 9px; font-size: 0.7rem; font-weight: 600; border: 1px solid currentColor; }
.decision { margin: 12px 0 0; font-size: 0.9rem; color: var(--text-primary); }
.decision p, .decision ul { margin: 0; }
.decision ul { list-style: none; padding: 0; color: var(--text-secondary); }
.log { margin: 12px 16px; }
.log h2 { font-size: 1rem; margin: 0; }
.events, .week-rules, .blackouts, .apps, .stretches { list-style: none; margin: 8px 0 0; padding: 0; display: grid; gap: 8px; }
.events li { font-size: 0.875rem; color: var(--text-secondary); }
.event-when { font-variant-numeric: tabular-nums; }
.event-who { color: var(--text-primary); font-weight: 600; text-transform: capitalize; }
#tab-rules, #tab-apps { padding: 0 16px 16px; display: grid; gap: 12px; }
.chips { display: flex; gap: 6px; flex-wrap: wrap; }
.chip { padding: 6px 12px; border: 1px solid var(--track); border-radius: 16px; background: transparent; color: var(--text-secondary); font: inherit; text-transform: capitalize; }
.chip[aria-pressed="true"] { background: var(--track); color: var(--text-primary); font-weight: 600; }
.day-row { display: flex; justify-content: space-between; gap: 12px; width: 100%; padding: 10px 0; border: 0; border-bottom: 1px solid var(--track); background: transparent; color: inherit; font: inherit; text-align: left; }
.day-name { font-weight: 600; }
.day-summary { color: var(--text-secondary); text-align: right; }
.editor { display: grid; gap: 10px; padding: 10px 0; }
.editor label { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.editor input, .editor select, .editor button, .app-row select { font: inherit; padding: 6px 8px; min-height: 36px; }
.editor-actions { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.stretch-row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.form-error { margin: 0; color: var(--text-primary); font-weight: 600; }
.form-error::before { content: "⚠ "; }
.blackouts li, .app-row { display: flex; justify-content: space-between; align-items: center; gap: 8px; flex-wrap: wrap; }
.app-seen { color: var(--text-muted); font-size: 0.8rem; }
```

- [ ] **Step 5: `sw.js`**

Bump the cache name and add the new script to the shell so an installed app picks up the new files:

```js
const CACHE = "kidtime-v2";
const SHELL = ["/", "/app.js", "/manage.js", "/style.css", "/manifest.webmanifest", "/icon.svg", "/icon-192.png"];
```

- [ ] **Step 6: Build**

Run: `cargo build -p server && cargo test -p server`
Expected: builds; all tests still pass (the static files are compiled in).

- [ ] **Step 7: Check every screen by hand**

Start a local server with the login check off, feed it one report as in Task 6 Step 7, and open
`http://127.0.0.1:18470` in a browser (use the Playwright browser tools if no person is at the keyboard),
at a 390px-wide viewport and again at 1200px, in light and dark colour schemes.

- [ ] Today: cards render as before; a restricted kid shows the decision line and "Games … left".
- [ ] Today: an unrestricted account shows no decision line.
- [ ] Rules: pick the account; seven rows; tapping one opens the editor.
- [ ] Rules: turn on "Limit the hours", add two stretches, save; the row summary shows both.
- [ ] Rules: enter a stretch that ends before it starts and save; the server's message appears, and the
      editor still holds what was typed.
- [ ] Rules: "Weekdays" copies Monday–Friday and leaves Saturday and Sunday alone.
- [ ] Rules: set games to 0; the summary says "no games"; Today shows "Games budget used up".
- [ ] Rules: a stretch ending 23:59 saves as "midnight" and reopens as 23:59.
- [ ] Blackouts: add one covering now for the account; Today shows "Would be locked: blackout until …".
      After the next report, the log gains "Would have locked: blackout: <note>". Delete it; after the next
      report the log gains "Allowed again".
- [ ] Blackouts: an end before the start shows the server's message.
- [ ] Apps: the app is listed with "New"; choosing a category removes "New" and the tab badge count drops.
- [ ] Apps: report an app with id `window:A/B: Game?` and recategorise it.
- [ ] Keyboard: every control is reachable with Tab and operable with Enter or Space.
- [ ] No horizontal scrolling at 390px.
- [ ] Session expiry: with the server stopped, the header shows "Can't reach server" and nothing throws in
      the console. (The redirect case can only be seen behind the real proxy; check it after deployment by
      opening the app with an expired session.)

Fix what fails before committing.

- [ ] **Step 8: Commit**

```bash
git add crates/server/static
git commit -m "feat(dashboard): add rules, blackouts and app categories"
```

---

### Task 8: Configuration examples and documentation

**Files:**
- Modify: `deploy/server.toml.example`, `compose.yaml`, `README.md`, `CLAUDE.md`

**Interfaces:**
- Consumes: the settings from Task 5.
- Produces: documentation only.

- [ ] **Step 1: `deploy/server.toml.example`**

Append:

```toml

# Login check. Set both to make the server verify the signed token an authenticating reverse proxy
# adds to each request (header Cf-Access-Jwt-Assertion). Then only the agents' endpoint and /healthz
# answer without a login. Leave both unset only for local development.
# access_team = "https://TEAM.cloudflareaccess.com"
# access_aud = "APPLICATION-AUDIENCE-TAG"
```

- [ ] **Step 2: `compose.yaml`**

In `environment:`, after `TZ`:

```yaml
      # Login check (see README). Both empty or unset turns it off; set both in production.
      KIDTIME_ACCESS_TEAM: ${KIDTIME_ACCESS_TEAM:-}
      KIDTIME_ACCESS_AUD: ${KIDTIME_ACCESS_AUD:-}
```

`access_config` treats empty strings as unset, so the empty defaults leave the check off.

- [ ] **Step 3: `README.md`**

Add a section after "Dashboard on a phone":

```markdown
## Rules

The dashboard has three tabs.

- **Today** shows each kid's usage, what the rules say right now, and a log of what kidtime would have done.
  Nothing is enforced on the computers yet.
- **Rules** sets, per kid and per weekday, the allowed hours and a games budget, plus one-off blackouts.
- **Apps** lists every app seen and its category. Only apps in the Games category use the games budget.

## Who can see and change things

Without the login check, anyone who can reach the server can read and change everything. To turn it on,
put the server behind a reverse proxy that authenticates people and adds a signed token to each request
(Cloudflare Access does), and set `KIDTIME_ACCESS_TEAM` and `KIDTIME_ACCESS_AUD`. The server then verifies
that token itself, so reaching its port directly doesn't get around the login. The agents' endpoint and
`/healthz` stay open; the agents use their own token.
```

- [ ] **Step 4: `CLAUDE.md`**

- In "Architecture", list the new server files and one line each (copy the File Structure table's wording).
- In "Data flow", add the new tables: `app`, `app_activity`, `day_rule`, `stretch`, `budget`, `blackout`,
  `event`, `category`, `account`, each with one line.
- In "Server, dashboard and container", add the two settings and the rule "only `/api/report` and
  `/healthz` answer without a login".
- In "Tests", add: the decision function, category time, token verification, the API.
- In "Next steps", replace the "dashboard auth" bullet with "done" and leave enforcement as the next item,
  noting the planned behaviour: budget used up closes that category's apps; schedule or blackout locks the
  session.
- In "Design decisions to keep", add: "Budgets count one category of apps; category time is worked out
  from per-app stretches when asked, so recategorising applies to the whole day."

- [ ] **Step 5: Run the whole gate**

```bash
cargo +nightly fmt --check
cargo +stable clippy --all-targets --all-features -- -D warnings
cargo +stable test --locked
docker build -t kidtime-server:dev .
```

Expected: all clean, and the image builds (the new dependencies compile in the container too).

- [ ] **Step 6: Commit**

```bash
git add deploy compose.yaml README.md CLAUDE.md
git commit -m "docs: describe rules, categories and the login check"
```

---

## After the last task

Do not push. Report to the user: the branch name, the commits, the gate results, and the hand-check list
with its results. Landing it is their decision, and takes these steps in this order:

1. Rebase `rules-and-management` onto `main` and fast-forward `main` (no merge commit), then push. The
   release bot cuts the next version and publishes the image.
2. In the deployment's environment, set `KIDTIME_ACCESS_TEAM` and `KIDTIME_ACCESS_AUD`, bump the pinned
   image tag, and redeploy. Until both are set the new version runs with the login check off.
3. Verify on the live system: the direct port returns 401 for `/`, `/healthz` returns `ok`, the agents keep
   reporting, and the dashboard works through the login.
