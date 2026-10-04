# Enforcement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the kidtime agent enforce the server's rules on each PC: warn, lock and disable login while blocked,
cut streams, close games when the games budget is used up, keep doing it while the server is unreachable, and
let a parent switch it on per kid.

**Architecture:** The rule types and `decide` move from the server into the shared `protocol` crate. The server's
report response carries a snapshot per account (rules, blackouts, today's category time, app lists, the Enforce
switch). The agent keeps the latest snapshots on disk and runs a pure `Enforcer` every 5 seconds that decides
locally and drives an `Actions` trait; the real `Actions` implementation runs `loginctl`, `usermod`, `gdbus`,
`swaymsg`, `nft`, signals and `gsettings`. Everything the `Enforcer` does is tested with a fake `Actions`.

**Tech Stack:** Rust 2024 (workspace: `protocol`, `agent`, `server`), tokio, zbus, axum, rusqlite, chrono,
vanilla JS dashboard, systemd, nftables, GNOME/GDM, Sway.

**Spec:** `docs/superpowers/specs/2026-10-04-enforcement-design.md`. Read it, and its predecessor
`docs/superpowers/specs/2026-10-02-rules-and-management-design.md`, before starting any task.

## Global Constraints

- Add dependencies with `cargo add`, never by editing `Cargo.toml` by hand.
- Format with `cargo +nightly fmt`. The gate before every commit: `cargo +nightly fmt --check`,
  `cargo +stable clippy --all-targets --all-features -- -D warnings`, `cargo +stable test --locked`.
- Identifiers capitalise only the first letter of acronyms (`Gdm`, `Nft`, `Dbus`).
- No household details in committed files: accounts `kid1`/`kid2`, hosts `host-a`/`host-b`.
- Warnings at exactly 10, 5 and 1 minute (600, 300, 60 seconds) before a lock or before the games budget runs out.
- Enforcement loop every 5 seconds. Clock skew threshold 5 minutes (300 seconds).
- Only apps in the Games category (`GAMES = 1`) are ever closed. Apps in Ignored (`IGNORED = 2`) never count.
  Every other app counts toward the games budget.
- Games keep running behind a lock. Closing: SIGTERM, then SIGKILL after 10 seconds.
- Login is disabled with `usermod -L` and restored with `usermod -U`, and only for accounts the agent itself
  disabled. An account that was already password-locked is never unlocked by the agent.
- Stream cut: nftables table `inet kidtime`, input hook, priority -10, dropping TCP and UDP to ports
  `base-5`…`base+21` from any interface except `lo`; base from `~kid/.config/sunshine/sunshine.conf` (`port = N`),
  default 47989.
- Login-screen banner: one line per blocked, enforced kid, plain text; off when nobody is blocked.
- An account with Enforce off, or with no snapshot, is never acted on, and anything the agent did to it is undone.
- Compatibility: old agents ignore the response body; a new agent treats an empty 204 as "no snapshot".
- Work on branch `enforcement`. Commit after each task with the given message. Never push or merge; the user lands it.

## Review Focus

1. **A kid who is blocked when the agent restarts or the PC reboots.** Expected: still blocked after start (from
   the state file), and unblocked as soon as rules allow, without the server. (Task 5: restart test with a
   persisted state.)
2. **An account already password-locked by the parent before kidtime blocks it.** Expected: kidtime never unlocks
   it, even after the block ends. (Task 4: test.)
3. **Parent extends time after a warning has fired** (budget raised or a stretch moved later). Expected: warnings
   fire again for the new deadline, not suppressed by the old ones. (Task 4: test.)
4. **Midnight while offline.** Expected: the games count restarts at zero; yesterday's weekday rule is not applied
   to today. (Task 4: test.)
5. **A banner or notification text containing quotes or newlines** (blackout notes are typed by the parent).
   Expected: shown as text, never breaks the command. (Task 6: escaping tests.)

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/protocol/src/rules.rs` (moved from server) | Rule types, `GAMES`/`IGNORED`, validation, `decide`. |
| `crates/protocol/src/lib.rs` (modify) | `ReportResponse`, `AccountSnapshot`; `UserSample.overrun`/`errors`. |
| `crates/server/src/db.rs` (modify) | Enforce setting, snapshot building, event `enforced`/`host`. |
| `crates/server/src/api.rs`, `main.rs` (modify) | Enforce endpoint, report response, status additions. |
| `crates/agent/src/enforce.rs` (new) | `Enforcer`: pure decision-to-action logic, warnings, banner lines, persisted state. |
| `crates/agent/src/actions.rs` (new) | `SystemActions`: the real `Actions` (commands and signals). |
| `crates/agent/src/apps.rs`, `sway.rs`, `logind.rs` (modify) | Running apps with pids/windows; sessions with lock state. |
| `crates/agent/src/main.rs` (modify) | Response parsing, 5-second loop, state file, `--release-all`. |
| `crates/server/static/*.js` (modify) | Enforce switch, wording, overrun/errors lines. |
| `deploy/install-agent.sh`, `deploy/kidtime-agent.service`, docs (modify) | Install options, unit, recovery. |

---

### Task 0: Branch

- [ ] **Step 1:** `git switch -c enforcement`

---

### Task 1: Move the rules into `protocol`

A pure move. No behaviour changes.

**Files:**
- Move: `crates/server/src/rules.rs` → `crates/protocol/src/rules.rs`
- Modify: `crates/protocol/src/lib.rs`, `crates/protocol/Cargo.toml`, `crates/server/src/{main.rs,db.rs,api.rs}`

**Interfaces:**
- Produces: `protocol::rules::{CategoryId, Stretch, DayRule, BlackoutSpan, Computer, CategoryStatus, Decision,
  Invalid, validate_day, validate_blackout, decide, GAMES, IGNORED}`. `GAMES = 1`, `IGNORED = 2` move from
  `db.rs` into `rules.rs`; `db.rs` keeps `pub use protocol::rules::{GAMES, IGNORED};` so existing paths compile.

- [ ] **Step 1: Move the file and wire it up**

```bash
git mv crates/server/src/rules.rs crates/protocol/src/rules.rs
cargo add -p protocol chrono --no-default-features --features serde,std,clock
```

In `crates/protocol/src/lib.rs`, add at the top: `pub mod rules;`.
In `crates/server/src/main.rs`, remove `mod rules;` and replace every `crate::rules` / `rules::` path with
`protocol::rules` (in `main.rs`, `db.rs`, `api.rs`: `use protocol::rules::{self, …};`).
Move the two constants with their doc comments from `db.rs` into `rules.rs`, and in `db.rs` write
`pub use protocol::rules::{GAMES, IGNORED};`.

`rules.rs` tests use `serde_json`: `cargo add -p protocol --dev serde_json`.

- [ ] **Step 2: Gate**

Run: `cargo +stable test --locked` — expected: same test count as before (server 64 + rules tests now under
`protocol`), all passing. `cargo +stable clippy --all-targets --all-features -- -D warnings` clean.

- [ ] **Step 3: Commit**

```bash
git add -A crates/protocol crates/server Cargo.lock
git commit -m "refactor: move the rules and decision function into the protocol crate"
```

---

### Task 2: Protocol types for the snapshot and the report extras

**Files:**
- Modify: `crates/protocol/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct ReportResponse { pub server_time: NaiveDateTime, pub accounts: Vec<AccountSnapshot> }
  pub struct AccountSnapshot {
      pub user: String, pub enforce: bool, pub for_day: NaiveDate,
      pub day: rules::DayRule, pub blackouts: Vec<rules::BlackoutSpan>,
      pub used_secs: BTreeMap<rules::CategoryId, i64>,
      pub games: Vec<String>, pub ignored: Vec<String>,
  }
  // UserSample gains (serde default, so old agents' reports still parse):
  pub overrun: Vec<String>,   // names of uncategorised apps running after the games budget ran out
  pub errors: Vec<String>,    // enforcement actions that failed since the last report
  ```
  `BlackoutSpan` gains `Serialize, Deserialize`. All new types derive `Debug, Clone, PartialEq, Serialize, Deserialize`.

- [ ] **Step 1: Failing test** — append to `crates/protocol/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn snapshot_round_trips_and_old_samples_still_parse() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let response = ReportResponse {
            server_time: day.and_hms_opt(12, 0, 0).unwrap(),
            accounts: vec![AccountSnapshot {
                user: "kid1".into(),
                enforce: true,
                for_day: day,
                day: rules::DayRule::default(),
                blackouts: vec![rules::BlackoutSpan {
                    start: day.and_hms_opt(17, 0, 0).unwrap(),
                    end: day.and_hms_opt(19, 0, 0).unwrap(),
                    note: "dinner".into(),
                }],
                used_secs: BTreeMap::from([(rules::GAMES, 900)]),
                games: vec!["steam:1".into()],
                ignored: vec!["kitty".into()],
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<ReportResponse>(&json).unwrap(), response);

        // A sample from an agent that predates overrun/errors
        let old = r#"{"user":"kid1","state":"active","apps":[]}"#;
        let sample: UserSample = serde_json::from_str(old).unwrap();
        assert!(sample.overrun.is_empty() && sample.errors.is_empty());
    }
}
```

Run: `cargo test -p protocol` — expected: compile error, `ReportResponse` not found.

- [ ] **Step 2: Implement**

```rust
use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};

/// What the server answers a report with: everything an agent needs to enforce the rules for the accounts
/// in that report, including while the server is unreachable afterwards.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportResponse {
    /// The server's local wall-clock time, so an agent can tell if its own clock is off.
    pub server_time: NaiveDateTime,
    pub accounts: Vec<AccountSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub user: String,
    /// Off: the agent leaves the account alone and undoes anything it did.
    pub enforce: bool,
    /// The day `day` and `used_secs` belong to.
    pub for_day: NaiveDate,
    pub day: rules::DayRule,
    /// Blackouts that apply to this account and haven't ended.
    pub blackouts: Vec<rules::BlackoutSpan>,
    /// Today's category time across all PCs, merged.
    pub used_secs: BTreeMap<rules::CategoryId, i64>,
    /// App ids in the Games category: the only apps an agent closes.
    pub games: Vec<String>,
    /// App ids in the Ignored category: never counted.
    pub ignored: Vec<String>,
}
```

Add to `UserSample`:

```rust
    /// Uncategorised apps still running after the games budget ran out (they count, but aren't closed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrun: Vec<String>,
    /// Enforcement actions that failed since the last report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
```

Every place that builds a `UserSample` (agent `take_sample`, server and agent tests) gains
`overrun: Vec::new(), errors: Vec::new()`. In `rules.rs`, `BlackoutSpan` derives `Serialize, Deserialize`.

- [ ] **Step 3:** `cargo +stable test --locked` — all pass. Gate.

- [ ] **Step 4: Commit**

```bash
git add crates Cargo.lock
git commit -m "feat(protocol): add the report response snapshot and report extras"
```

---

### Task 3: Server — Enforce switch, report response, status, event fields

**Files:**
- Modify: `crates/server/src/db.rs`, `crates/server/src/api.rs`, `crates/server/src/main.rs`

**Interfaces:**
- Consumes: Task 2 types.
- Produces:
  - `Db::enforce(&self, user) -> Result<bool>`, `Db::set_enforce(&mut self, user, bool) -> Result<()>`
    (table `account_settings(user TEXT PRIMARY KEY, enforce INTEGER NOT NULL)`).
  - `Db::snapshot(&self, user, now: NaiveDateTime) -> Result<AccountSnapshot>`.
  - `Db::blackout_spans(&self, user, now) -> Result<Vec<BlackoutSpan>>` (factored out of `decision`, which uses it).
  - `Db::app_ids_in(&self, category) -> Result<Vec<String>>`.
  - `Db::log_decision(&mut self, user, at, decision, enforced: bool, host: &str)`; `event` gains columns
    `enforced INTEGER NOT NULL DEFAULT 0`, `host TEXT NOT NULL DEFAULT ''`, added to an existing table with
    `ALTER TABLE` when missing; `Event` gains `enforced: bool, host: String`.
  - `PUT /api/accounts/{user}/enforce` body `{"enforce": bool}` → 204 (404 unknown account), behind the login.
  - `GET /api/rules/{user}` gains `"enforce": bool`.
  - `POST /api/report` → 200 with a `ReportResponse` (accounts in the report's last sample, in that order).
  - `GET /api/status` users gain `enforce: bool`, `overrun: Vec<String>`, `errors: Vec<String>` (from the
    latest live samples across hosts; deduplicated).

- [ ] **Step 1: Failing tests**

`db.rs` tests:

```rust
    #[test]
    fn enforce_defaults_off_and_round_trips() {
        let (mut db, path) = temp_db("enforce");
        assert!(!db.enforce("kid1").unwrap());
        db.set_enforce("kid1", true).unwrap();
        assert!(db.enforce("kid1").unwrap());
        db.set_enforce("kid1", false).unwrap();
        assert!(!db.enforce("kid1").unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn snapshot_carries_todays_rule_usage_blackouts_and_app_lists() {
        let (mut db, path) = temp_db("snapshot");
        let t0 = start_of_test();
        db.record(&sample_report("host-a", 1, t0 + 15, UserState::Active, &[("steam:1", "Minecraft"), ("kitty", "kitty")]))
            .unwrap();
        db.set_app_category("kitty", Some(IGNORED)).unwrap();
        let rule = DayRule { restricted: true, stretches: vec![Stretch { start_min: 375, end_min: 1260 }], budgets: BTreeMap::from([(GAMES, 60)]) };
        db.set_day_rule("kid1", weekday_today(), &rule).unwrap();
        let now = noon_today();
        let hour = chrono::Duration::hours(1);
        db.add_blackout(Some("kid1"), now + hour, now + hour * 2, "dinner").unwrap();
        db.add_blackout(Some("kid2"), now + hour, now + hour * 2, "not mine").unwrap();
        db.set_enforce("kid1", true).unwrap();

        let snap = db.snapshot("kid1", now).unwrap();
        assert!(snap.enforce);
        assert_eq!(snap.for_day, now.date());
        assert_eq!(snap.day, rule);
        assert_eq!(snap.used_secs[&GAMES], 15);
        assert_eq!(snap.blackouts.iter().map(|b| b.note.as_str()).collect::<Vec<_>>(), ["dinner"]);
        assert_eq!(snap.games, ["steam:1"]);
        assert_eq!(snap.ignored, ["kitty"]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn events_record_enforcement_and_host() {
        let (mut db, path) = temp_db("event-fields");
        db.log_decision("kid1", 100, &decision_of(Computer::OutsideSchedule, &[]), true, "host-a").unwrap();
        let e = &db.events(Some("kid1"), 10).unwrap()[0];
        assert!(e.enforced);
        assert_eq!(e.host, "host-a");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn event_columns_are_added_to_an_existing_database() {
        let path = std::env::temp_dir().join(format!("kidtime-test-oldevents-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE event (id INTEGER PRIMARY KEY, user TEXT NOT NULL, at INTEGER NOT NULL,
                 key TEXT NOT NULL, kind TEXT NOT NULL, detail TEXT NOT NULL);
                 INSERT INTO event (user, at, key, kind, detail) VALUES ('kid1', 1, 'k', 'locked', 'x');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let e = &db.events(Some("kid1"), 10).unwrap()[0];
        assert!(!e.enforced);
        assert_eq!(e.host, "");
        std::fs::remove_file(&path).unwrap();
    }
```

Update every existing `log_decision(...)` call in tests to pass `false, "host-a"` as the last two arguments.

`api.rs` tests:

```rust
    #[tokio::test]
    async fn report_answers_with_a_snapshot_per_account() {
        let app = app("snapshot-response");
        report_for(&app, "kid1").await;
        call(&app, "PUT", "/api/accounts/kid1/enforce", Some(json!({ "enforce": true }))).await;
        let at = chrono::Local::now().timestamp();
        let body = json!({ "host": "host-a", "agent_id": "kid1", "interval_secs": 15, "samples": [{
            "seq": 2, "at": at, "elapsed_secs": 15,
            "users": [{ "user": "kid1", "state": "active", "apps": [] }, { "user": "kid2", "state": "offline", "apps": [] }],
        }]});
        let request = Request::builder()
            .method("POST").uri("/api/report")
            .header("content-type", "application/json").header("authorization", "Bearer token")
            .body(Body::from(body.to_string())).unwrap();
        let response = router(app.state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let parsed: protocol::ReportResponse = serde_json::from_slice(&bytes).unwrap();
        let users: Vec<&str> = parsed.accounts.iter().map(|a| a.user.as_str()).collect();
        assert_eq!(users, ["kid1", "kid2"]);
        assert!(parsed.accounts[0].enforce);
        assert!(!parsed.accounts[1].enforce);
    }

    #[tokio::test]
    async fn enforce_switch_round_trips_through_rules() {
        let app = app("enforce-api");
        report_for(&app, "kid1").await;
        assert_eq!(call(&app, "GET", "/api/rules/kid1", None).await.1["enforce"], false);
        let (status, _) = call(&app, "PUT", "/api/accounts/kid1/enforce", Some(json!({ "enforce": true }))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(call(&app, "GET", "/api/rules/kid1", None).await.1["enforce"], true);
        assert_eq!(call(&app, "PUT", "/api/accounts/nobody/enforce", Some(json!({ "enforce": true }))).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn status_carries_enforce_overrun_and_errors() {
        let app = app("status-extras");
        let at = chrono::Local::now().timestamp();
        let body = json!({ "host": "host-a", "agent_id": "a", "interval_secs": 15, "samples": [{
            "seq": 1, "at": at, "elapsed_secs": 15,
            "users": [{ "user": "kid1", "state": "active", "apps": [], "overrun": ["Terminal"], "errors": ["lock failed: x"] }],
        }]});
        let request = Request::builder()
            .method("POST").uri("/api/report")
            .header("content-type", "application/json").header("authorization", "Bearer token")
            .body(Body::from(body.to_string())).unwrap();
        router(app.state.clone()).oneshot(request).await.unwrap();
        let user = &call(&app, "GET", "/api/status", None).await.1["users"][0];
        assert_eq!(user["enforce"], false);
        assert_eq!(user["overrun"], json!(["Terminal"]));
        assert_eq!(user["errors"], json!(["lock failed: x"]));
    }
```

Existing API tests that assert `POST /api/report` returns `NO_CONTENT` change to `OK` (the helpers
`report`, `report_as`, `report_for`). Add `("PUT", "/api/accounts/kid1/enforce")` to the login-check route list.

Run: `cargo test -p server` — expected: compile errors for the new methods.

- [ ] **Step 2: Implement `db.rs`**

Schema additions in `Db::open` (after the existing batch):

```sql
CREATE TABLE IF NOT EXISTS account_settings (
    user    TEXT PRIMARY KEY,
    enforce INTEGER NOT NULL
);
```

and after the batch, for the event table:

```rust
        add_column_if_missing(&conn, "event", "enforced", "INTEGER NOT NULL DEFAULT 0")?;
        add_column_if_missing(&conn, "event", "host", "TEXT NOT NULL DEFAULT ''")?;
```

```rust
/// SQLite has no `ADD COLUMN IF NOT EXISTS`; databases from older versions get the column here.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let exists: bool = conn.query_row(
        &format!("SELECT EXISTS (SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
        [column],
        |r| r.get(0),
    )?;
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
    }
    Ok(())
}
```

Methods:

```rust
    pub fn enforce(&self, user: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT enforce FROM account_settings WHERE user = ?1", [user], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    pub fn set_enforce(&mut self, user: &str, enforce: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO account_settings (user, enforce) VALUES (?1, ?2)
             ON CONFLICT (user) DO UPDATE SET enforce = excluded.enforce",
            params![user, enforce],
        )?;
        Ok(())
    }

    pub fn app_ids_in(&self, category: CategoryId) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT app_id FROM app WHERE category_id = ?1 ORDER BY app_id")?;
        let ids = stmt.query_map([category], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Blackouts that apply to the account at `now`: its own, and the "all kids" ones if it has any rule.
    pub fn blackout_spans(&self, user: &str, now: NaiveDateTime) -> Result<Vec<BlackoutSpan>> {
        let restricted = self.is_restricted(user)?;
        Ok(self
            .blackouts(now)?
            .into_iter()
            .filter(|b| match &b.user {
                Some(name) => name == user,
                None => restricted,
            })
            .map(|b| BlackoutSpan { start: b.start, end: b.end, note: b.note })
            .collect())
    }

    pub fn snapshot(&self, user: &str, now: NaiveDateTime) -> Result<protocol::AccountSnapshot> {
        let weekday = now.date().weekday().num_days_from_monday() as u8;
        Ok(protocol::AccountSnapshot {
            user: user.to_string(),
            enforce: self.enforce(user)?,
            for_day: now.date(),
            day: self.day_rule(user, weekday)?,
            blackouts: self.blackout_spans(user, now)?,
            used_secs: self.category_secs(user, now.date())?,
            games: self.app_ids_in(GAMES)?,
            ignored: self.app_ids_in(IGNORED)?,
        })
    }
```

`decision` now uses `blackout_spans` instead of its inline filter. `log_decision` gains `enforced: bool, host: &str`
and inserts them; `events()` selects them into `Event { …, enforced: r.get(5)?, host: r.get(6)? }`.

- [ ] **Step 3: Implement the server handlers**

`api.rs`:

```rust
#[derive(Deserialize)]
pub struct SetEnforce {
    enforce: bool,
}

pub async fn put_enforce(
    State(state): State<Arc<AppState>>,
    Path(user): Path<String>,
    Json(set): Json<SetEnforce>,
) -> Api<StatusCode> {
    let mut db = state.db.lock().unwrap();
    if !db.is_account(&user)? {
        return Err(ApiError::NotFound);
    }
    db.set_enforce(&user, set.enforce)?;
    Ok(StatusCode::NO_CONTENT)
}
```

`get_rules` returns `json!({ "user": user, "enforce": db.enforce(&user)?, "days": db.week_rules(&user)? })`.
Route in `router()`: `.route("/api/accounts/{user}/enforce", put(api::put_enforce))` inside `behind_login`.

`main.rs` `report`: after recording and logging decisions (pass `enforced = db.enforce(user)?`-equivalent with
errors logged, and `host = &report.host`), build the response for the users in the last sample, in order:

```rust
    let now_local = Local::now().naive_local();
    let mut accounts = Vec::new();
    for user in last_sample_users {
        match db.snapshot(user, now_local) {
            Ok(s) => accounts.push(s),
            // The report is already recorded; answer with what can be built rather than fail it
            Err(e) => tracing::error!("snapshot for {user}: {e:#}"),
        }
    }
    (StatusCode::OK, Json(protocol::ReportResponse { server_time: now_local, accounts })).into_response()
```

`UserStatus` gains `enforce: bool`, `overrun: Vec<String>`, `errors: Vec<String>`. In `status`, collect each user's
`overrun` and `errors` from the non-stale live hosts' `UserSample`s (sorted, deduplicated) and `enforce` from
`db.enforce(&name)` (error → 500 like the other queries).

- [ ] **Step 4:** `cargo +stable test --locked` — all pass. Gate.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "feat(server): answer reports with per-account snapshots and add the Enforce switch"
```

---

### Task 4: The `Enforcer`

The core of the piece: pure logic, no I/O, everything through `Actions`.

**Files:**
- Create: `crates/agent/src/enforce.rs`
- Modify: `crates/agent/src/main.rs` (`mod enforce;` only)

**Interfaces:**
- Consumes: `protocol::{ReportResponse, AccountSnapshot, App, UserState}`, `protocol::rules::{decide, Computer,
  DayRule, GAMES}`.
- Produces (all `pub`):
  ```rust
  pub const WARN_AT_SECS: [i64; 3] = [600, 300, 60];
  pub const MAX_CLOCK_SKEW_SECS: i64 = 300;
  pub struct SessionInfo { pub id: String, pub locked: bool }
  pub struct WindowRef { pub socket: PathBuf, pub con_id: i64 }
  pub struct RunningApp { pub id: String, pub name: String, pub pids: Vec<u32>, pub window: Option<WindowRef> }
  pub struct Observation { pub user: String, pub sessions: Vec<SessionInfo>, pub running: Vec<RunningApp>, pub streaming_host: bool }
  pub trait Actions {
      fn lock_session(&mut self, session_id: &str) -> anyhow::Result<()>;
      fn login_disabled(&mut self, user: &str) -> anyhow::Result<bool>;
      fn disable_login(&mut self, user: &str) -> anyhow::Result<()>;
      fn enable_login(&mut self, user: &str) -> anyhow::Result<()>;
      fn notify(&mut self, user: &str, text: &str);
      fn close(&mut self, user: &str, app: &RunningApp) -> anyhow::Result<()>;
      fn block_stream(&mut self, user: &str) -> anyhow::Result<()>;
      fn unblock_stream(&mut self, user: &str) -> anyhow::Result<()>;
      fn set_banner(&mut self, lines: &[String]) -> anyhow::Result<()>;
  }
  #[derive(Default, Serialize, Deserialize, …)]
  pub struct Persisted { pub snapshots: BTreeMap<String, AccountSnapshot>, pub login_disabled: BTreeSet<String>,
                         pub streams_blocked: BTreeSet<String>, pub clock_offset_secs: i64 }
  pub struct Enforcer { pub persisted: Persisted, /* private live state */ }
  impl Enforcer {
      pub fn new(persisted: Persisted) -> Self;
      pub fn apply_response(&mut self, response: &ReportResponse, local_now: NaiveDateTime);
      pub fn count(&mut self, user: &str, state: UserState, apps: &[App], secs: i64, today: NaiveDate);
      pub fn tick(&mut self, local_now: NaiveDateTime, observations: &[Observation], act: &mut dyn Actions);
      pub fn release_all(&mut self, act: &mut dyn Actions);
      pub fn overrun(&self, user: &str) -> Vec<String>;
      pub fn take_errors(&mut self, user: &str) -> Vec<String>;
      pub fn take_dirty(&mut self) -> bool;  // true once after `persisted` changed
  }
  ```
  The spec names four action traits; one `Actions` trait with the same operations is equivalent and simpler to
  fake. Notification text goes to both the desktop and, on a streaming host, the stream; that split lives in the
  real implementation (Task 6).

- [ ] **Step 1: Failing tests**

Create `crates/agent/src/enforce.rs` containing only the tests module and the imports it needs, and add
`mod enforce;` to `main.rs`. Write a fake that records calls:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use protocol::rules::{BlackoutSpan, DayRule, Stretch, GAMES};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        already_locked: BTreeSet<String>,
        fail_lock: bool,
        banner: Vec<String>,
    }

    impl Actions for Fake {
        fn lock_session(&mut self, id: &str) -> anyhow::Result<()> {
            self.calls.push(format!("lock {id}"));
            if self.fail_lock { anyhow::bail!("logind said no") } else { Ok(()) }
        }
        fn login_disabled(&mut self, user: &str) -> anyhow::Result<bool> { Ok(self.already_locked.contains(user)) }
        fn disable_login(&mut self, user: &str) -> anyhow::Result<()> { self.calls.push(format!("disable {user}")); Ok(()) }
        fn enable_login(&mut self, user: &str) -> anyhow::Result<()> { self.calls.push(format!("enable {user}")); Ok(()) }
        fn notify(&mut self, user: &str, text: &str) { self.calls.push(format!("notify {user}: {text}")); }
        fn close(&mut self, user: &str, app: &RunningApp) -> anyhow::Result<()> { self.calls.push(format!("close {user} {}", app.id)); Ok(()) }
        fn block_stream(&mut self, user: &str) -> anyhow::Result<()> { self.calls.push(format!("block {user}")); Ok(()) }
        fn unblock_stream(&mut self, user: &str) -> anyhow::Result<()> { self.calls.push(format!("unblock {user}")); Ok(()) }
        fn set_banner(&mut self, lines: &[String]) -> anyhow::Result<()> {
            self.calls.push(format!("banner {}", lines.join(" | ")));
            self.banner = lines.to_vec();
            Ok(())
        }
    }

    impl Fake {
        fn take(&mut self) -> Vec<String> { std::mem::take(&mut self.calls) }
    }

    fn day() -> NaiveDate { NaiveDate::from_ymd_opt(2026, 10, 5).unwrap() } // a Monday
    fn at(h: u32, m: u32) -> NaiveDateTime { day().and_hms_opt(h, m, 0).unwrap() }

    fn snapshot(user: &str, rule: DayRule) -> AccountSnapshot {
        AccountSnapshot {
            user: user.into(),
            enforce: true,
            for_day: day(),
            day: rule,
            blackouts: vec![],
            used_secs: BTreeMap::new(),
            games: vec!["steam:1".into()],
            ignored: vec!["kitty".into()],
        }
    }

    fn evenings() -> DayRule { // allowed 6:15am–9pm
        DayRule { restricted: true, stretches: vec![Stretch { start_min: 375, end_min: 1260 }], budgets: BTreeMap::new() }
    }

    fn games_budget(minutes: u32) -> DayRule {
        DayRule { restricted: false, stretches: vec![], budgets: BTreeMap::from([(GAMES, minutes)]) }
    }

    fn enforcer(snaps: Vec<AccountSnapshot>, server_time: NaiveDateTime) -> Enforcer {
        let mut e = Enforcer::new(Persisted::default());
        e.apply_response(&ReportResponse { server_time, accounts: snaps }, server_time);
        e
    }

    fn app(id: &str) -> RunningApp { RunningApp { id: id.into(), name: id.into(), pids: vec![42], window: None } }

    fn obs(user: &str, locked: bool, running: Vec<RunningApp>) -> Observation {
        Observation { user: user.into(), sessions: vec![SessionInfo { id: "7".into(), locked }], running, streaming_host: false }
    }

    #[test]
    fn warns_once_at_each_threshold_before_a_lock() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(20, 50), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["notify kid1: 10 minutes left today: the computer locks at 9:00pm"]);
        e.tick(at(20, 51), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.take().is_empty(), "the 10-minute warning fires once");
        e.tick(at(20, 55), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["notify kid1: 5 minutes left today: the computer locks at 9:00pm"]);
        e.tick(at(20, 59), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["notify kid1: 1 minute left: save your game. The computer locks at 9:00pm"]);
    }

    #[test]
    fn a_late_start_gets_only_the_nearest_warning() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(20, 57), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["notify kid1: 5 minutes left today: the computer locks at 9:00pm"]);
    }

    #[test]
    fn no_warnings_for_a_kid_who_isnt_on_this_pc() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        let away = Observation { user: "kid1".into(), sessions: vec![], running: vec![], streaming_host: false };
        e.tick(at(20, 55), &[away], &mut fake);
        assert!(fake.take().is_empty());
    }

    #[test]
    fn blocking_disables_login_locks_and_relocks_then_restores() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), [
            "disable kid1",
            "notify kid1: Computer time is over until 6:15am",
            "lock 7",
            "banner kid1: computer time is over until 6:15am",
        ]);
        assert!(e.persisted.login_disabled.contains("kid1"));
        assert!(e.take_dirty());
        // Still locked: nothing to do
        e.tick(at(21, 0), &[obs("kid1", true, vec![])], &mut fake);
        assert!(fake.take().is_empty());
        // They unlocked somehow: lock again
        e.tick(at(21, 1), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["lock 7"]);
        // Next morning
        let next = day().succ_opt().unwrap().and_hms_opt(6, 15, 0).unwrap();
        let mut snap = snapshot("kid1", evenings());
        snap.for_day = next.date();
        e.apply_response(&ReportResponse { server_time: next, accounts: vec![snap] }, next);
        e.tick(next, &[obs("kid1", true, vec![])], &mut fake);
        assert_eq!(fake.take(), ["enable kid1", "banner "]);
        assert!(e.persisted.login_disabled.is_empty());
    }

    #[test]
    fn an_account_already_locked_by_someone_else_is_never_unlocked() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake { already_locked: BTreeSet::from(["kid1".into()]), ..Default::default() };
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert!(!fake.calls.iter().any(|c| c.starts_with("disable")));
        assert!(e.persisted.login_disabled.is_empty());
        fake.take();
        let mut snap = snapshot("kid1", DayRule::default());
        snap.for_day = day();
        e.apply_response(&ReportResponse { server_time: at(21, 5), accounts: vec![snap] }, at(21, 5));
        e.tick(at(21, 5), &[obs("kid1", true, vec![])], &mut fake);
        assert!(!fake.calls.iter().any(|c| c.starts_with("enable")));
    }

    #[test]
    fn blackout_text_and_banner_name_the_blackout() {
        let mut snap = snapshot("kid1", DayRule::default());
        snap.blackouts = vec![BlackoutSpan { start: at(17, 0), end: at(19, 0), note: "dinner".into() }];
        let mut e = enforcer(vec![snap], at(17, 0));
        let mut fake = Fake::default();
        e.tick(at(17, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.calls.contains(&"notify kid1: Computer time is over until 7:00pm (dinner)".to_string()));
        assert_eq!(fake.banner, ["kid1: computer time is over until 7:00pm (dinner)"]);
    }

    #[test]
    fn streams_are_cut_on_a_streaming_host_and_restored() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        let mut o = obs("kid1", true, vec![]);
        o.streaming_host = true;
        e.tick(at(21, 0), std::slice::from_ref(&o), &mut fake);
        assert!(fake.calls.contains(&"block kid1".to_string()));
        assert!(e.persisted.streams_blocked.contains("kid1"));
        fake.take();
        e.tick(at(21, 1), std::slice::from_ref(&o), &mut fake);
        assert!(!fake.calls.contains(&"block kid1".to_string()), "blocked once, not every tick");
        e.persisted.snapshots.get_mut("kid1").unwrap().enforce = false;
        e.tick(at(21, 2), &[o], &mut fake);
        assert!(fake.calls.contains(&"unblock kid1".to_string()));
        assert!(e.persisted.streams_blocked.is_empty());
    }

    #[test]
    fn games_budget_warns_then_closes_games_only_and_reports_overrun() {
        let mut snap = snapshot("kid1", games_budget(30));
        snap.used_secs = BTreeMap::from([(GAMES, 29 * 60)]);
        let mut e = enforcer(vec![snap], at(15, 0));
        let mut fake = Fake::default();
        let running = vec![app("steam:1"), app("org.example.New"), app("kitty")];
        e.tick(at(15, 0), &[obs("kid1", false, running.clone())], &mut fake);
        assert_eq!(fake.take(), ["notify kid1: 1 minute of games left today: save your game"]);
        // A minute of local play later
        e.count("kid1", UserState::Active, &[protocol::App { id: "steam:1".into(), name: "x".into() }], 60, day());
        e.tick(at(15, 1), &[obs("kid1", false, running.clone())], &mut fake);
        assert_eq!(fake.take(), ["notify kid1: Games time is used up for today", "close kid1 steam:1"]);
        assert_eq!(e.overrun("kid1"), ["org.example.New"]);
        // Restarted game is closed again; the notice isn't repeated
        e.tick(at(15, 2), &[obs("kid1", false, running)], &mut fake);
        assert_eq!(fake.take(), ["close kid1 steam:1"]);
    }

    #[test]
    fn warnings_fire_again_when_the_parent_gives_more_time() {
        let mut snap = snapshot("kid1", evenings());
        let mut e = enforcer(vec![snap.clone()], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(20, 55), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take().len(), 1);
        snap.day.stretches = vec![Stretch { start_min: 375, end_min: 1320 }]; // until 10pm now
        e.apply_response(&ReportResponse { server_time: at(20, 56), accounts: vec![snap] }, at(20, 56));
        e.tick(at(21, 55), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["notify kid1: 5 minutes left today: the computer locks at 10:00pm"]);
    }

    #[test]
    fn offline_counting_adds_to_the_snapshot_and_restarts_at_midnight() {
        let mut snap = snapshot("kid1", games_budget(30));
        snap.used_secs = BTreeMap::from([(GAMES, 20 * 60)]);
        let mut e = enforcer(vec![snap], at(23, 0));
        let games = [protocol::App { id: "steam:1".into(), name: "x".into() }];
        e.count("kid1", UserState::Active, &games, 600, day());
        let mut fake = Fake::default();
        e.tick(at(23, 30), &[obs("kid1", false, vec![app("steam:1")])], &mut fake);
        assert!(fake.calls.contains(&"close kid1 steam:1".to_string()), "20 + 10 minutes used up the 30");
        fake.take();
        // After midnight, without a new snapshot: yesterday's Monday rule doesn't apply to Tuesday
        let tuesday = day().succ_opt().unwrap();
        e.count("kid1", UserState::Active, &games, 15, tuesday);
        e.tick(tuesday.and_hms_opt(0, 5, 0).unwrap(), &[obs("kid1", false, vec![app("steam:1")])], &mut fake);
        assert!(fake.take().is_empty());
    }

    #[test]
    fn ignored_and_idle_time_is_not_counted() {
        let mut snap = snapshot("kid1", games_budget(1));
        snap.used_secs = BTreeMap::new();
        let mut e = enforcer(vec![snap], at(15, 0));
        e.count("kid1", UserState::Active, &[protocol::App { id: "kitty".into(), name: "kitty".into() }], 120, day());
        e.count("kid1", UserState::Idle, &[protocol::App { id: "steam:1".into(), name: "x".into() }], 120, day());
        let mut fake = Fake::default();
        e.tick(at(15, 0), &[obs("kid1", false, vec![app("steam:1")])], &mut fake);
        assert!(!fake.calls.iter().any(|c| c.starts_with("close")));
    }

    #[test]
    fn a_new_snapshot_resets_the_local_count() {
        let mut snap = snapshot("kid1", games_budget(30));
        let mut e = enforcer(vec![snap.clone()], at(15, 0));
        let games = [protocol::App { id: "steam:1".into(), name: "x".into() }];
        e.count("kid1", UserState::Active, &games, 25 * 60, day());
        snap.used_secs = BTreeMap::from([(GAMES, 25 * 60)]); // the server has those samples now
        e.apply_response(&ReportResponse { server_time: at(15, 25), accounts: vec![snap] }, at(15, 25));
        let mut fake = Fake::default();
        e.tick(at(15, 25), &[obs("kid1", false, vec![app("steam:1")])], &mut fake);
        assert!(!fake.calls.iter().any(|c| c.starts_with("close")), "25 minutes, not 50");
    }

    #[test]
    fn enforce_off_or_no_snapshot_means_hands_off() {
        let mut snap = snapshot("kid1", evenings());
        snap.enforce = false;
        let mut e = enforcer(vec![snap], at(22, 0));
        let mut fake = Fake::default();
        e.tick(at(22, 0), &[obs("kid1", false, vec![app("steam:1")]), obs("kid2", false, vec![app("steam:1")])], &mut fake);
        assert!(fake.take().is_empty());
    }

    #[test]
    fn the_server_clock_wins_when_ours_is_far_off() {
        // Our clock says 8:00pm, the server's says 9:00pm
        let mut e = Enforcer::new(Persisted::default());
        e.apply_response(&ReportResponse { server_time: at(21, 0), accounts: vec![snapshot("kid1", evenings())] }, at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(20, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.calls.contains(&"disable kid1".to_string()));
        // A small difference is ignored
        let mut e = Enforcer::new(Persisted::default());
        e.apply_response(&ReportResponse { server_time: at(20, 2), accounts: vec![snapshot("kid1", evenings())] }, at(20, 0));
        assert_eq!(e.persisted.clock_offset_secs, 0);
    }

    #[test]
    fn failures_are_reported_and_retried() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake { fail_lock: true, ..Default::default() };
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(e.take_errors("kid1"), ["lock failed: logind said no"]);
        assert!(e.take_errors("kid1").is_empty());
        fake.take();
        e.tick(at(21, 1), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["lock 7"]);
    }

    #[test]
    fn release_all_undoes_everything() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        let mut o = obs("kid1", true, vec![]);
        o.streaming_host = true;
        e.tick(at(21, 0), &[o], &mut fake);
        fake.take();
        e.release_all(&mut fake);
        assert_eq!(fake.take(), ["enable kid1", "unblock kid1", "banner "]);
        assert!(e.persisted.login_disabled.is_empty() && e.persisted.streams_blocked.is_empty());
    }

    #[test]
    fn persisted_state_round_trips_and_a_restart_resumes_the_block() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        let saved = serde_json::to_string(&e.persisted).unwrap();
        // A new process, offline: still blocked, login stays disabled (not disabled twice)
        let mut e = Enforcer::new(serde_json::from_str(&saved).unwrap());
        let mut fake = Fake::default();
        e.tick(at(21, 30), &[obs("kid1", false, vec![])], &mut fake);
        assert!(!fake.calls.contains(&"disable kid1".to_string()));
        assert!(fake.calls.contains(&"lock 7".to_string()));
        // And it lets go in the morning without the server
        let morning = day().succ_opt().unwrap().and_hms_opt(6, 15, 0).unwrap();
        fake.take();
        e.tick(morning, &[obs("kid1", true, vec![])], &mut fake);
        assert!(fake.calls.contains(&"enable kid1".to_string()), "same weekday rule? no: unrestricted until the server answers");
    }

    #[test]
    fn banner_lists_every_blocked_kid_and_is_only_set_on_change() {
        let mut e = enforcer(vec![snapshot("kid1", evenings()), snapshot("kid2", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", true, vec![]), obs("kid2", true, vec![])], &mut fake);
        assert_eq!(fake.banner, ["kid1: computer time is over until 6:15am", "kid2: computer time is over until 6:15am"]);
        fake.take();
        e.tick(at(21, 1), &[obs("kid1", true, vec![]), obs("kid2", true, vec![])], &mut fake);
        assert!(!fake.calls.iter().any(|c| c.starts_with("banner")));
    }
}
```

Note on `persisted_state_round_trips…`: the next day is a Tuesday and the snapshot is Monday's, so the rule
does not carry over and the kid is unrestricted until the server answers (spec, Loop step 1).

Run: `cargo test -p agent enforce` — expected: compile errors (types missing).

- [ ] **Step 2: Implement**

Above the tests in `enforce.rs`:

```rust
//! Turns the server's rules into actions on this PC. Pure logic: every effect goes through `Actions`, so the
//! whole thing is tested with a fake.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use protocol::rules::{self, Computer, Decision, DayRule, GAMES};
use protocol::{AccountSnapshot, App, ReportResponse, UserState};
use serde::{Deserialize, Serialize};

pub const WARN_AT_SECS: [i64; 3] = [600, 300, 60];
pub const MAX_CLOCK_SKEW_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRef {
    pub socket: PathBuf,
    pub con_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningApp {
    pub id: String,
    pub name: String,
    pub pids: Vec<u32>,
    /// The Sway window, for apps in a streaming session.
    pub window: Option<WindowRef>,
}

/// What the agent sees of one tracked account on this PC right now.
#[derive(Debug, Clone)]
pub struct Observation {
    pub user: String,
    /// Graphical sessions on this PC.
    pub sessions: Vec<SessionInfo>,
    pub running: Vec<RunningApp>,
    /// This PC hosts streaming sessions, so the account's stream can be cut here.
    pub streaming_host: bool,
}

pub trait Actions {
    fn lock_session(&mut self, session_id: &str) -> anyhow::Result<()>;
    /// Whether the account's password login is disabled right now.
    fn login_disabled(&mut self, user: &str) -> anyhow::Result<bool>;
    fn disable_login(&mut self, user: &str) -> anyhow::Result<()>;
    fn enable_login(&mut self, user: &str) -> anyhow::Result<()>;
    /// On the desktop and, on a streaming host, in the stream. Failures are logged by the implementation.
    fn notify(&mut self, user: &str, text: &str);
    fn close(&mut self, user: &str, app: &RunningApp) -> anyhow::Result<()>;
    fn block_stream(&mut self, user: &str) -> anyhow::Result<()>;
    fn unblock_stream(&mut self, user: &str) -> anyhow::Result<()>;
    /// Empty turns the banner off.
    fn set_banner(&mut self, lines: &[String]) -> anyhow::Result<()>;
}

/// Survives restarts, in the state file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Persisted {
    pub snapshots: BTreeMap<String, AccountSnapshot>,
    /// Accounts whose login this agent disabled (and so may re-enable).
    pub login_disabled: BTreeSet<String>,
    /// Accounts whose Sunshine ports this agent blocked.
    pub streams_blocked: BTreeSet<String>,
    /// Server time minus local time, when the difference is too big to ignore.
    pub clock_offset_secs: i64,
}

#[derive(Default)]
struct Live {
    /// Counted seconds on this PC since the last snapshot, and the day they belong to.
    local_secs: i64,
    local_day: Option<NaiveDate>,
    /// Warnings already shown, keyed by what they announced.
    warned: BTreeSet<String>,
    blocked: bool,
    games_closed: bool,
    /// Login was already disabled by someone else when we blocked: leave it alone.
    external_lock: bool,
    overrun: Vec<String>,
    errors: Vec<String>,
}

pub struct Enforcer {
    pub persisted: Persisted,
    live: BTreeMap<String, Live>,
    banner: Option<Vec<String>>,
    dirty: bool,
}

impl Enforcer {
    pub fn new(persisted: Persisted) -> Self {
        // If someone was blocked when the agent stopped, the banner may still be up: set it on the first tick
        let banner = if persisted.login_disabled.is_empty() { Some(Vec::new()) } else { None };
        Self { persisted, live: BTreeMap::new(), banner, dirty: false }
    }

    pub fn apply_response(&mut self, response: &ReportResponse, local_now: NaiveDateTime) {
        let offset = (response.server_time - local_now).num_seconds();
        let offset = if offset.abs() > MAX_CLOCK_SKEW_SECS { offset } else { 0 };
        if offset != self.persisted.clock_offset_secs {
            if offset != 0 {
                tracing::warn!("this PC's clock is {offset}s off the server's; using the server's time");
            }
            self.persisted.clock_offset_secs = offset;
        }
        for snap in &response.accounts {
            // The server's figure now includes everything this PC reported
            let live = self.live.entry(snap.user.clone()).or_default();
            live.local_secs = 0;
            if self.persisted.snapshots.get(&snap.user) != Some(snap) {
                self.persisted.snapshots.insert(snap.user.clone(), snap.clone());
            }
        }
        self.dirty = true;
    }

    /// Adds a sample's time to the account's local count, the way the server counts games time.
    pub fn count(&mut self, user: &str, state: UserState, apps: &[App], secs: i64, today: NaiveDate) {
        let Some(snap) = self.persisted.snapshots.get(user) else { return };
        let counted = state.counts() && apps.iter().any(|a| !snap.ignored.contains(&a.id));
        let live = self.live.entry(user.to_string()).or_default();
        if live.local_day != Some(today) {
            live.local_day = Some(today);
            live.local_secs = 0;
        }
        if counted {
            live.local_secs += secs;
        }
    }

    pub fn tick(&mut self, local_now: NaiveDateTime, observations: &[Observation], act: &mut dyn Actions) {
        let now = local_now + chrono::Duration::seconds(self.persisted.clock_offset_secs);
        let mut banner = Vec::new();
        for o in observations {
            let snap = match self.persisted.snapshots.get(&o.user) {
                Some(s) if s.enforce => s.clone(),
                _ => {
                    self.let_go(&o.user, act);
                    continue;
                }
            };
            let decision = self.decision(&snap, now);
            let blocked = decision.computer != Computer::Allowed;
            let present = !o.sessions.is_empty() || !o.running.is_empty();
            if blocked {
                let reason = blocked_reason(&snap, &decision, now);
                banner.push(format!("{}: computer time is over {reason}", o.user));
                self.block(o, &reason, act);
            } else {
                self.let_go(&o.user, act);
                if present {
                    self.warn_before_lock(o, &snap, &decision, now, act);
                }
            }
            self.games(o, &snap, &decision, blocked, present, now, act);
        }
        if self.banner.as_ref() != Some(&banner) {
            match act.set_banner(&banner) {
                Ok(()) => self.banner = Some(banner),
                Err(e) => tracing::warn!("login screen banner: {e:#}"),
            }
        }
    }

    pub fn release_all(&mut self, act: &mut dyn Actions) {
        for user in self.persisted.login_disabled.clone() {
            match act.enable_login(&user) {
                Ok(()) => { self.persisted.login_disabled.remove(&user); }
                Err(e) => tracing::error!("re-enabling login for {user}: {e:#}"),
            }
        }
        for user in self.persisted.streams_blocked.clone() {
            match act.unblock_stream(&user) {
                Ok(()) => { self.persisted.streams_blocked.remove(&user); }
                Err(e) => tracing::error!("unblocking {user}'s stream: {e:#}"),
            }
        }
        if let Err(e) = act.set_banner(&[]) {
            tracing::error!("clearing the login screen banner: {e:#}");
        }
        self.dirty = true;
    }

    pub fn overrun(&self, user: &str) -> Vec<String> {
        self.live.get(user).map(|l| l.overrun.clone()).unwrap_or_default()
    }

    pub fn take_errors(&mut self, user: &str) -> Vec<String> {
        self.live.get_mut(user).map(|l| std::mem::take(&mut l.errors)).unwrap_or_default()
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The decision for a snapshot at `now`, with this PC's uncounted time added. A snapshot from another day
    /// keeps its weekday rule only on the same weekday; its usage never carries over.
    fn decision(&self, snap: &AccountSnapshot, now: NaiveDateTime) -> Decision {
        let today = now.date();
        let day = if snap.for_day == today || snap.for_day.weekday() == today.weekday() {
            snap.day.clone()
        } else {
            DayRule::default()
        };
        let mut used = if snap.for_day == today { snap.used_secs.clone() } else { BTreeMap::new() };
        if let Some(live) = self.live.get(&snap.user).filter(|l| l.local_day == Some(today)) {
            *used.entry(GAMES).or_default() += live.local_secs;
        }
        rules::decide(&day, &snap.blackouts, &used, now)
    }

    fn block(&mut self, o: &Observation, reason: &str, act: &mut dyn Actions) {
        let user = &o.user;
        let first = !self.live.get(user).is_some_and(|l| l.blocked);
        if first {
            let live = self.live.entry(user.clone()).or_default();
            live.blocked = true;
            live.external_lock = false;
            if !self.persisted.login_disabled.contains(user) {
                match act.login_disabled(user) {
                    Ok(true) => self.live.get_mut(user).unwrap().external_lock = true,
                    Ok(false) => match act.disable_login(user) {
                        Ok(()) => {
                            self.persisted.login_disabled.insert(user.clone());
                            self.dirty = true;
                        }
                        Err(e) => self.error(user, format!("disabling login failed: {e:#}")),
                    },
                    Err(e) => self.error(user, format!("checking login failed: {e:#}")),
                }
            }
            act.notify(user, &format!("Computer time is over {reason}"));
        } else if !self.persisted.login_disabled.contains(user) && !self.live[user].external_lock {
            // A failed disable is retried every tick
            if let Ok(false) = act.login_disabled(user) {
                match act.disable_login(user) {
                    Ok(()) => { self.persisted.login_disabled.insert(user.clone()); self.dirty = true; }
                    Err(e) => self.error(user, format!("disabling login failed: {e:#}")),
                }
            }
        }
        for s in o.sessions.iter().filter(|s| !s.locked) {
            if let Err(e) = act.lock_session(&s.id) {
                self.error(user, format!("lock failed: {e:#}"));
            }
        }
        if o.streaming_host && !self.persisted.streams_blocked.contains(user) {
            match act.block_stream(user) {
                Ok(()) => { self.persisted.streams_blocked.insert(user.clone()); self.dirty = true; }
                Err(e) => self.error(user, format!("cutting the stream failed: {e:#}")),
            }
        }
    }

    /// Undoes anything this agent did to the account.
    fn let_go(&mut self, user: &str, act: &mut dyn Actions) {
        if let Some(live) = self.live.get_mut(user) {
            live.blocked = false;
            live.external_lock = false;
        }
        if self.persisted.login_disabled.contains(user) {
            match act.enable_login(user) {
                Ok(()) => { self.persisted.login_disabled.remove(user); self.dirty = true; }
                Err(e) => self.error(user, format!("re-enabling login failed: {e:#}")),
            }
        }
        if self.persisted.streams_blocked.contains(user) {
            match act.unblock_stream(user) {
                Ok(()) => { self.persisted.streams_blocked.remove(user); self.dirty = true; }
                Err(e) => self.error(user, format!("restoring the stream failed: {e:#}")),
            }
        }
    }

    fn warn_before_lock(&mut self, o: &Observation, snap: &AccountSnapshot, d: &Decision, now: NaiveDateTime, act: &mut dyn Actions) {
        // Only warn if the clock alone will block them at the next change
        if self.decision(snap, d.next_change).computer == Computer::Allowed {
            return;
        }
        let left = (d.next_change - now).num_seconds();
        let Some(threshold) = nearest_threshold(left) else { return };
        let key = format!("lock:{}:{threshold}", d.next_change);
        let when = clock(d.next_change, now);
        let text = if threshold == 60 {
            format!("1 minute left: save your game. The computer locks at {when}")
        } else {
            format!("{} minutes left today: the computer locks at {when}", threshold / 60)
        };
        self.warn_once(&o.user, key, &text, act);
    }

    #[allow(clippy::too_many_arguments)]
    fn games(&mut self, o: &Observation, snap: &AccountSnapshot, d: &Decision, blocked: bool, present: bool, now: NaiveDateTime, act: &mut dyn Actions) {
        let user = o.user.clone();
        let Some(games) = d.categories.iter().find(|c| c.category == GAMES) else {
            self.live.entry(user).or_default().overrun.clear();
            return;
        };
        let counted_running = o.running.iter().any(|a| !snap.ignored.contains(&a.id));
        if !games.used_up {
            self.live.entry(user.clone()).or_default().overrun.clear();
            self.live.entry(user.clone()).or_default().games_closed = false;
            if present && counted_running && !blocked {
                if let Some(threshold) = nearest_threshold(games.left_secs) {
                    // The budget's size is part of the key, so raising it warns again
                    let budget = snap.day.budgets.get(&GAMES).copied().unwrap_or(0);
                    let key = format!("games:{}:{budget}:{threshold}", now.date());
                    let text = if threshold == 60 {
                        "1 minute of games left today: save your game".to_string()
                    } else {
                        format!("{} minutes of games left today", threshold / 60)
                    };
                    self.warn_once(&user, key, &text, act);
                }
            }
            return;
        }
        // Used up. Behind a lock, games keep running.
        if blocked {
            return;
        }
        let live = self.live.entry(user.clone()).or_default();
        if !live.games_closed {
            live.games_closed = true;
            act.notify(&user, "Games time is used up for today");
        }
        let mut overrun = Vec::new();
        for app in &o.running {
            if snap.games.contains(&app.id) {
                if let Err(e) = act.close(&user, app) {
                    self.error(&user, format!("closing {} failed: {e:#}", app.name));
                }
            } else if !snap.ignored.contains(&app.id) {
                overrun.push(app.id.clone());
            }
        }
        self.live.entry(user).or_default().overrun = overrun;
    }

    fn warn_once(&mut self, user: &str, key: String, text: &str, act: &mut dyn Actions) {
        let live = self.live.entry(user.to_string()).or_default();
        if live.warned.insert(key) {
            act.notify(user, text);
        }
    }

    fn error(&mut self, user: &str, message: String) {
        tracing::warn!("{user}: {message}");
        self.live.entry(user.to_string()).or_default().errors.push(message);
    }
}

/// The smallest warning threshold at or above `left`, if any: a kid who starts with 4 minutes left gets the
/// 5-minute warning only.
fn nearest_threshold(left: i64) -> Option<i64> {
    WARN_AT_SECS.iter().rev().copied().find(|&t| left > 0 && left <= t)
}

/// "until 6:15am", "until 7:00pm (dinner)", "until Sat 9:00am", or "for today".
fn blocked_reason(snap: &AccountSnapshot, d: &Decision, now: NaiveDateTime) -> String {
    match &d.computer {
        Computer::Blackout { until, note } if note.is_empty() => format!("until {}", clock(*until, now)),
        Computer::Blackout { until, note } => format!("until {} ({note})", clock(*until, now)),
        _ => {
            // The next allowed stretch today, or tomorrow's first one if the snapshot's rule is for every day
            let today_start = snap.day.stretches.iter().map(|s| s.start_min).find(|&m| {
                now.date().and_time(chrono::NaiveTime::MIN) + chrono::Duration::minutes(i64::from(m)) > now
            });
            match today_start.or_else(|| snap.day.stretches.iter().map(|s| s.start_min).min()) {
                Some(m) => format!("until {}", minute_clock(m)),
                None => "for today".to_string(),
            }
        }
    }
}

fn minute_clock(minutes: u16) -> String {
    let (h, m) = (u32::from(minutes) / 60, u32::from(minutes) % 60);
    format!("{}:{m:02}{}", if h % 12 == 0 { 12 } else { h % 12 }, if h < 12 { "am" } else { "pm" })
}

/// "9:00pm" today, "Sat 9:00am" another day.
fn clock(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let time = minute_clock((t.hour() * 60 + t.minute()) as u16);
    if t.date() == now.date() { time } else { format!("{} {time}", t.format("%a")) }
}
```

The tomorrow's-first-stretch fallback in `blocked_reason` uses today's rule as an approximation of
tomorrow's; when tomorrow's rule differs the time shown may be wrong until the next snapshot. This is accepted:
the server's next answer arrives within 15 seconds of the lock.

- [ ] **Step 3: Run** `cargo test -p agent enforce` — expected: all tests pass. If a test fails because the
  brief's code and test disagree, report it rather than changing the expected value.

- [ ] **Step 4: Gate and commit**

`enforce.rs` is not wired in yet: add `#![allow(dead_code)]` at its top for this task (Task 7 removes it).

```bash
git add crates/agent
git commit -m "feat(agent): add the enforcement logic"
```

---

### Task 5: The agent sees running apps, sessions and their lock state

**Files:**
- Modify: `crates/agent/src/apps.rs`, `crates/agent/src/sway.rs`, `crates/agent/src/logind.rs`

**Interfaces:**
- Produces:
  - `apps::UserApps` gains `pub running: Vec<enforce::RunningApp>`: every app in `apps` and `streamed_apps`
    with its pids — a desktop scope's `cgroup.procs`; a Steam game's `reaper` pid plus all its descendants.
  - `sway::Window` gains `pub pid: u32, pub con_id: i64`.
  - `logind::Logind::graphical_sessions(&self, uid) -> zbus::Result<Vec<enforce::SessionInfo>>`
    (graphical `user`-class sessions with their `LockedHint`).
  - `apps::descendants(root: u32, parents: &HashMap<u32, u32>) -> Vec<u32>` (pure; `parents` maps pid→ppid,
    built from `/proc/*/stat`).

- [ ] **Step 1: Failing tests** (pure parts)

`apps.rs`:

```rust
    #[test]
    fn descendants_walks_the_whole_tree() {
        let parents = HashMap::from([(2, 1), (3, 2), (4, 2), (5, 4), (9, 8)]);
        let mut d = descendants(2, &parents);
        d.sort_unstable();
        assert_eq!(d, [2, 3, 4, 5]);
    }

    #[test]
    fn ppid_is_read_after_the_command_name() {
        // The command name can contain spaces and parentheses
        assert_eq!(parse_ppid("1234 (Web Content (x)) S 99 1234 1234 0"), Some(99));
    }
```

`sway.rs`, in `finds_focused_window`: add `"id": 77` to the focused node and assert
`(window.pid, window.con_id) == (2, 77)`.

Run: `cargo test -p agent` — expected: compile errors.

- [ ] **Step 2: Implement**

`apps.rs`:

```rust
/// The pid and every process below it.
pub fn descendants(root: u32, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let pid = out[i];
        out.extend(parents.iter().filter(|&(_, &pp)| pp == pid).map(|(&p, _)| p));
        i += 1;
    }
    out
}

fn parse_ppid(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

fn process_parents() -> HashMap<u32, u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let ppid = parse_ppid(&fs::read_to_string(e.path().join("stat")).ok()?)?;
            Some((pid, ppid))
        })
        .collect()
}
```

In `scan`: build `process_parents()` lazily (once per scan, only if a Steam game is found). For each desktop
app scope found, push `RunningApp { id, name, pids: read_pids(&dir), window: None }`; for each Steam game, push
`RunningApp { id: "steam:N", name, pids: descendants(pid, &parents), window: None }`. Use the same
`push_unique` rule by id for `running`.

`sway.rs`: `find_focused` fills `pid: node["pid"].as_u64().unwrap_or(0) as u32` and
`con_id: node["id"].as_i64().unwrap_or(0)`.

`logind.rs`:

```rust
    /// The user's graphical sessions on this PC, and whether each is locked.
    pub async fn graphical_sessions(&self, uid: u32) -> zbus::Result<Vec<crate::enforce::SessionInfo>> {
        let manager = ManagerProxy::new(&self.conn).await?;
        let Ok(user_path) = manager.get_user(uid).await else { return Ok(Vec::new()) };
        let user = UserProxy::builder(&self.conn).path(user_path)?.cache_properties(CacheProperties::No).build().await?;
        let mut out = Vec::new();
        for (id, path) in user.sessions().await? {
            let s = SessionProxy::builder(&self.conn).path(path)?.cache_properties(CacheProperties::No).build().await?;
            if GRAPHICAL_TYPES.contains(&s.session_type().await?.as_str()) && s.class().await? == "user" {
                out.push(crate::enforce::SessionInfo { id, locked: s.locked_hint().await? });
            }
        }
        Ok(out)
    }
```

(Match the builder style `user_state` already uses in this file; if it differs, follow the file.)

- [ ] **Step 3:** `cargo test -p agent` — all pass. `--dump` still works: `cargo run -p agent -- --config
  dev/agent.dev.toml.example --dump` may fail on the example's fake users; that's expected without a real config.
  Gate (`#[allow(dead_code)]` on new items only where clippy demands; Task 7 removes them).

- [ ] **Step 4: Commit**

```bash
git add crates/agent
git commit -m "feat(agent): find running apps' processes and sessions' lock state"
```

---

### Task 6: The real `Actions`

**Files:**
- Create: `crates/agent/src/actions.rs`

**Interfaces:**
- Consumes: `enforce::{Actions, RunningApp}`.
- Produces: `pub struct SystemActions { users: HashMap<String, (u32, PathBuf)>, streaming_sway_socket:
  Option<String>, closing: HashMap<u32, Instant> }` with `SystemActions::new(users, streaming_sway_socket)` and
  `impl Actions for SystemActions`. Pure helpers (tested): `shadow_locked(shadow: &str, user) -> Option<bool>`,
  `sunshine_port(conf: &str) -> u16`, `nft_block_commands(user, port) -> Vec<String>`, `gvariant_string(&str) ->
  String`, `swaynag_command(text) -> String`, `dconf_keyfile(lines: &[String]) -> String`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_lock_state() {
        let shadow = "root:$6$abc:1::::::\nkid1:!$6$def:1::::::\nkid2:$6$ghi:1::::::\n";
        assert_eq!(shadow_locked(shadow, "kid1"), Some(true));
        assert_eq!(shadow_locked(shadow, "kid2"), Some(false));
        assert_eq!(shadow_locked(shadow, "nobody"), None);
    }

    #[test]
    fn sunshine_port_with_default() {
        assert_eq!(sunshine_port("capture = wlr\nport = 48189\n"), 48189);
        assert_eq!(sunshine_port("capture = wlr\n"), 47989);
        assert_eq!(sunshine_port("port=48089"), 48089);
    }

    #[test]
    fn nft_rules_cover_the_port_range_except_loopback() {
        let cmds = nft_block_commands("kid1", 48189);
        assert!(cmds.iter().any(|c| c.contains("tcp dport 48184-48210 drop") && c.contains("iifname != \"lo\"")));
        assert!(cmds.iter().any(|c| c.contains("udp dport 48184-48210 drop")));
        assert!(cmds.iter().all(|c| c.contains("comment \"kid1\"") || !c.contains("dport")));
    }

    #[test]
    fn text_is_escaped_for_each_destination() {
        let text = "until 7:00pm (Sam's \"party\")\nsecond line";
        assert_eq!(gvariant_string(text), r#"'until 7:00pm (Sam\'s "party")\nsecond line'"#);
        let cmd = swaynag_command(text);
        assert!(!cmd.contains('\n'));
        assert!(cmd.starts_with("swaynag --layer overlay --edge top -t warning -m "));
        assert_eq!(dconf_keyfile(&[]), "[org/gnome/login-screen]\nbanner-message-enable=false\n");
        assert!(dconf_keyfile(&["a'b".into()]).contains("banner-message-text='a\\'b'"));
    }
}
```

Run: `cargo test -p agent actions` — expected: compile errors.

- [ ] **Step 2: Implement the helpers**

```rust
//! The real enforcement actions: commands and signals, run as root.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::enforce::{Actions, RunningApp};

const DEFAULT_SUNSHINE_PORT: u16 = 47989;
const KILL_AFTER: Duration = Duration::from_secs(10);
const DCONF_FILE: &str = "/etc/dconf/db/gdm.d/90-kidtime";

pub fn shadow_locked(shadow: &str, user: &str) -> Option<bool> {
    shadow.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next()? == user).then(|| f.next().is_some_and(|hash| hash.starts_with('!')))
    })
}

pub fn sunshine_port(conf: &str) -> u16 {
    conf.lines()
        .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == "port")?.1.trim().parse().ok())
        .unwrap_or(DEFAULT_SUNSHINE_PORT)
}

/// Sunshine uses base-5 to base+21; each account's base is 100 apart.
pub fn nft_block_commands(user: &str, port: u16) -> Vec<String> {
    let (lo, hi) = (port - 5, port + 21);
    vec![
        "add table inet kidtime".into(),
        "add chain inet kidtime input { type filter hook input priority -10 ; policy accept ; }".into(),
        format!("add rule inet kidtime input iifname != \"lo\" tcp dport {lo}-{hi} drop comment \"{user}\""),
        format!("add rule inet kidtime input iifname != \"lo\" udp dport {lo}-{hi} drop comment \"{user}\""),
    ]
}

/// A GVariant string literal: single quotes, with backslash, quote and newline escaped.
pub fn gvariant_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n");
    format!("'{escaped}'")
}

/// Run through `swaymsg exec`, which hands the string to `sh -c`: quote it for the shell, one line.
pub fn swaynag_command(text: &str) -> String {
    let one_line = text.replace('\n', " ");
    format!("swaynag --layer overlay --edge top -t warning -m '{}'", one_line.replace('\'', r"'\''"))
}

pub fn dconf_keyfile(lines: &[String]) -> String {
    if lines.is_empty() {
        return "[org/gnome/login-screen]\nbanner-message-enable=false\n".into();
    }
    format!(
        "[org/gnome/login-screen]\nbanner-message-enable=true\nbanner-message-text={}\n",
        gvariant_string(&lines.join("\n"))
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program).args(args).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
```

- [ ] **Step 3: Implement `SystemActions`**

```rust
pub struct SystemActions {
    /// name -> (uid, home)
    users: HashMap<String, (u32, PathBuf)>,
    streaming_sway_socket: Option<String>,
    /// Processes sent SIGTERM, and when.
    closing: HashMap<u32, Instant>,
}

impl SystemActions {
    pub fn new(users: HashMap<String, (u32, PathBuf)>, streaming_sway_socket: Option<String>) -> Self {
        Self { users, streaming_sway_socket, closing: HashMap::new() }
    }

    fn uid(&self, user: &str) -> Result<u32> {
        self.users.get(user).map(|(uid, _)| *uid).with_context(|| format!("{user} is not tracked"))
    }

    /// Runs a command as the user, with their session bus.
    fn as_user(&self, user: &str, args: &[&str]) -> Result<()> {
        let uid = self.uid(user)?;
        let runtime = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
        let bus = format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus");
        let mut full = vec!["-u", user, "--", "env", runtime.as_str(), bus.as_str()];
        full.extend_from_slice(args);
        run("runuser", &full)
    }
}

impl Actions for SystemActions {
    fn lock_session(&mut self, session_id: &str) -> Result<()> {
        run("loginctl", &["lock-session", session_id])
    }

    fn login_disabled(&mut self, user: &str) -> Result<bool> {
        let shadow = std::fs::read_to_string("/etc/shadow").context("reading /etc/shadow")?;
        shadow_locked(&shadow, user).with_context(|| format!("{user} not in /etc/shadow"))
    }

    fn disable_login(&mut self, user: &str) -> Result<()> {
        run("usermod", &["-L", user])
    }

    fn enable_login(&mut self, user: &str) -> Result<()> {
        run("usermod", &["-U", user])
    }

    fn notify(&mut self, user: &str, text: &str) {
        let body = gvariant_string(text);
        let desktop = self.as_user(user, &[
            "gdbus", "call", "--session", "--dest", "org.freedesktop.Notifications",
            "--object-path", "/org/freedesktop/Notifications",
            "--method", "org.freedesktop.Notifications.Notify",
            "Kidtime", "0", "dialog-warning", "Kidtime", &body, "[]", "{'urgency': <byte 2>}", "0",
        ]);
        if let Err(e) = desktop {
            tracing::debug!("desktop notification for {user}: {e:#}");
        }
        if let (Some(pattern), Ok(uid)) = (self.streaming_sway_socket.clone(), self.uid(user)) {
            let socket = pattern.replace("{uid}", &uid.to_string());
            if std::path::Path::new(&socket).exists() {
                let cmd = swaynag_command(text);
                if let Err(e) = self.as_user(user, &["swaymsg", "-s", &socket, "exec", &cmd]) {
                    tracing::debug!("stream notification for {user}: {e:#}");
                }
            }
        }
    }

    fn close(&mut self, _user: &str, app: &RunningApp) -> Result<()> {
        if let Some(w) = &app.window {
            let criteria = format!("[con_id={}] kill", w.con_id);
            let _ = run("swaymsg", &["-s", &w.socket.to_string_lossy(), &criteria]);
        }
        let now = Instant::now();
        for &pid in &app.pids {
            let signal = match self.closing.get(&pid) {
                Some(&since) if now.duration_since(since) >= KILL_AFTER => libc_kill(pid, 9),
                Some(_) => continue,
                None => {
                    self.closing.insert(pid, now);
                    libc_kill(pid, 15)
                }
            };
            if let Err(e) = signal {
                tracing::debug!("signal to {pid}: {e:#}");
            }
        }
        // Forget processes that have exited
        self.closing.retain(|pid, _| std::path::Path::new(&format!("/proc/{pid}")).exists());
        Ok(())
    }

    fn block_stream(&mut self, user: &str) -> Result<()> {
        let (_, home) = self.users.get(user).with_context(|| format!("{user} is not tracked"))?;
        let conf = std::fs::read_to_string(home.join(".config/sunshine/sunshine.conf")).unwrap_or_default();
        for cmd in nft_block_commands(user, sunshine_port(&conf)) {
            let args: Vec<&str> = cmd.split(' ').collect();
            // `add table`/`add chain` are idempotent; rules are re-added only after unblock removed them
            run("nft", &args)?;
        }
        Ok(())
    }

    fn unblock_stream(&mut self, user: &str) -> Result<()> {
        // Remove this user's rules by handle; drop the table when it's empty
        let listing = Command::new("nft").args(["-a", "list", "table", "inet", "kidtime"]).output()?;
        if !listing.status.success() {
            return Ok(()); // no table: nothing blocked
        }
        let text = String::from_utf8_lossy(&listing.stdout);
        let tag = format!("comment \"{user}\"");
        for line in text.lines().filter(|l| l.contains(&tag)) {
            if let Some(handle) = line.rsplit_once("# handle ").map(|(_, h)| h.trim()) {
                run("nft", &["delete", "rule", "inet", "kidtime", "input", "handle", handle])?;
            }
        }
        if !String::from_utf8_lossy(&Command::new("nft").args(["list", "table", "inet", "kidtime"]).output()?.stdout).contains("comment") {
            run("nft", &["delete", "table", "inet", "kidtime"])?;
        }
        Ok(())
    }

    fn set_banner(&mut self, lines: &[String]) -> Result<()> {
        // Later login screens: the GDM system settings file (the install script enables it)
        if std::path::Path::new("/etc/dconf/db/gdm.d").is_dir() {
            std::fs::write(DCONF_FILE, dconf_keyfile(lines))?;
            run("dconf", &["update"])?;
        }
        // The login screen running now: as its temporary user, over its session bus
        if let Some((uid, gid)) = greeter_ids() {
            let env = [format!("XDG_RUNTIME_DIR=/run/user/{uid}"), format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus")];
            let base = |extra: &[&str]| -> Result<()> {
                let reuid = format!("--reuid={uid}");
                let regid = format!("--regid={gid}");
                let mut args = vec![reuid.as_str(), regid.as_str(), "--clear-groups", "env", env[0].as_str(), env[1].as_str(), "gsettings", "set", "org.gnome.login-screen"];
                args.extend_from_slice(extra);
                run("setpriv", &args)
            };
            if lines.is_empty() {
                base(&["banner-message-enable", "false"])?;
            } else {
                base(&["banner-message-text", &lines.join("\n")])?;
                base(&["banner-message-enable", "true"])?;
            }
        }
        Ok(())
    }
}

/// uid and gid of the process running the login screen (`gnome-shell --mode=gdm`), if one is running.
fn greeter_ids() -> Option<(u32, u32)> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
        let args = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if !args.contains("gnome-shell") || !args.contains("--mode=gdm") {
            return None;
        }
        let status = std::fs::read_to_string(e.path().join("status")).ok()?;
        let id = |key: &str| status.lines().find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok());
        Some((id("Uid:")?, id("Gid:")?))
    })
}

fn libc_kill(pid: u32, signal: i32) -> Result<()> {
    run("kill", &[&format!("-{signal}"), &pid.to_string()])
}
```

`libc_kill` uses the `kill` command to avoid a new dependency; if the implementer prefers `nix` or `libc`,
add it with `cargo add` and say so in the report.

- [ ] **Step 4:** `cargo test -p agent actions` — the pure tests pass. Gate (allow dead code on
  `SystemActions` for this task only).

- [ ] **Step 5: Commit**

```bash
git add crates/agent Cargo.lock
git commit -m "feat(agent): carry out enforcement on the system"
```

---

### Task 7: Wire it into the agent

**Files:**
- Modify: `crates/agent/src/main.rs`, `deploy/kidtime-agent.service`

**Interfaces:**
- Consumes: Tasks 4–6.
- Produces: `--release-all` flag; state file `/var/lib/kidtime/agent-state.json` (mode 600), overridable with
  `state_file` in the agent config; a 5-second enforcement tick in the same `select!` loop as sampling.

- [ ] **Step 1: Failing test** — a pure helper for the state file:

```rust
    #[test]
    fn state_file_round_trips_and_tolerates_garbage() {
        let dir = std::env::temp_dir().join(format!("kidtime-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let mut p = enforce::Persisted::default();
        p.login_disabled.insert("kid1".into());
        save_state(&path, &p).unwrap();
        assert_eq!(load_state(&path), p);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_state(&path), enforce::Persisted::default());
        assert_eq!(load_state(&dir.join("missing.json")), enforce::Persisted::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Implement**

```rust
fn load_state(path: &Path) -> enforce::Persisted {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::error!("ignoring unreadable state file {}: {e}", path.display());
            enforce::Persisted::default()
        }),
        Err(_) => enforce::Persisted::default(),
    }
}

/// Written to a temporary file and renamed, so a crash never leaves half a file.
fn save_state(path: &Path, state: &enforce::Persisted) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string(state)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

`Config` gains `#[serde(default = "default_state_file")] state_file: PathBuf` (default
`/var/lib/kidtime/agent-state.json`).

In `main`:
- Parse `--release-all`: load the state, build `SystemActions`, `Enforcer::new(state).release_all(&mut actions)`,
  save, exit 0.
- Otherwise: `let mut enforcer = Enforcer::new(load_state(&config.state_file));` and
  `let mut actions = SystemActions::new(users_map, config.streaming_sway_socket.clone());`.
- Add `let mut enforce_ticker = tokio::time::interval(Duration::from_secs(5));` and a `select!` arm. On each
  enforce tick: for each tracked user build an `Observation { user, sessions: logind.graphical_sessions(uid)
  (empty on error, logged), running: scanner.scan(uid, home, &streaming_units).running, streaming_host:
  !config.streaming_units.is_empty() }`; call `enforcer.tick(Local::now().naive_local(), &obs, &mut actions)`;
  if `enforcer.take_dirty()`, `save_state` (log errors).
- On each sample: after `take_sample`, for every `UserSample`: `enforcer.count(&u.user, u.state, &u.apps,
  elapsed as i64, Local::now().date_naive())`, then fill `u.overrun = enforcer.overrun(&u.user)` and
  `u.errors = enforcer.take_errors(&u.user)`.
- On a successful report: if the body is non-empty, parse `protocol::ReportResponse` and call
  `enforcer.apply_response(&response, Local::now().naive_local())`; an empty body or a parse failure (old server)
  changes nothing. Save the state if dirty.
- Remove every temporary `#[allow(dead_code)]` from Tasks 4–6.

`chrono` in the agent: `cargo add -p agent chrono --no-default-features --features clock,std`.

`deploy/kidtime-agent.service` `[Service]` gains:

```ini
# State survives restarts; /etc is written by usermod and the login-screen banner settings
StateDirectory=kidtime
StateDirectoryMode=0700
ReadWritePaths=/etc /var/lib/kidtime
```

- [ ] **Step 3:** `cargo +stable test --locked` — all pass. Gate. `grep -rn 'allow(dead_code)' crates/agent/src`
  prints nothing.

- [ ] **Step 4: Check by hand without root** (it can't act, but must not crash):

```bash
cargo build -p agent
mkdir -p /tmp/claude-1000/kidtime-sdd
printf 'server_url="http://127.0.0.1:18470"\ntoken="dev"\nusers=["%s"]\nstate_file="/tmp/claude-1000/kidtime-sdd/state.json"\ninterval_secs=5\n' "$USER" > /tmp/claude-1000/kidtime-sdd/agent.toml
timeout 20 target/debug/kidtime-agent --config /tmp/claude-1000/kidtime-sdd/agent.toml
```

Expected: report failures logged (no server), no panic, enforcement ticks silent (no snapshot).

- [ ] **Step 5: Commit**

```bash
git add crates/agent deploy/kidtime-agent.service Cargo.lock
git commit -m "feat(agent): enforce the rules every 5 seconds and keep state across restarts"
```

---

### Task 8: Dashboard

**Files:**
- Modify: `crates/server/static/manage.js`, `crates/server/static/app.js`, `crates/server/static/style.css`

**Interfaces:**
- Consumes: `GET /api/rules/{user}` `enforce`; `PUT /api/accounts/{user}/enforce`; status `enforce`, `overrun`,
  `errors`; events `enforced`, `host`.

- [ ] **Step 1: Enforce switch (`manage.js`)**

In `renderRules`, above the week, when the rules response arrives:

```js
  const enforceHtml = `
      <label class="switch enforce">
        <input type="checkbox" id="enforce" ${rules.enforce ? "checked" : ""}>
        Enforce for ${esc(rulesUser)}
      </label>
      <p class="footnote">Off: kidtime only shows what it would do. On: the PCs lock, close games and cut streams.</p>
      <p class="form-error" id="enforce-error" role="alert" hidden></p>`;
```

Insert `${enforceHtml}` after the account picker. Handle changes:

```js
rulesEl.addEventListener("change", async (e) => {
  if (e.target.id !== "enforce") return;
  const box = e.target;
  const error = document.getElementById("enforce-error");
  error.hidden = true;
  if (box.checked && !confirm(`Turn on enforcement for ${rulesUser}? Their PCs will lock and close games by these rules.`)) {
    box.checked = false;
    return;
  }
  try {
    await api(`/api/accounts/${encodeURIComponent(rulesUser)}/enforce`, { method: "PUT", body: { enforce: box.checked } });
  } catch (err) {
    box.checked = !box.checked;
    error.textContent = err.message || "Couldn't save. Try again.";
    error.hidden = false;
  }
});
```

(If `rulesEl` already has a `change` listener for the editor checkboxes, add the `enforce` branch at its top and
`return` after handling, rather than a second listener.)

- [ ] **Step 2: Today card and log wording (`app.js`)**

In `decisionHtml`, use `u.enforce` to choose the prefix: blocked states read `Locked: …` when enforced and
`Would be locked: …` otherwise; a used-up games budget reads `Games budget used up: games closed` when
enforced. After the decision block, when `u.overrun.length`:

```js
  const overrun = u.overrun?.length
    ? `<p class="decision-note">${u.overrun.map(esc).join(", ")} ${u.overrun.length === 1 ? "is" : "are"} uncategorised and kept running after the games budget ran out. <a href="#" data-goto="apps">Sort in Apps</a></p>`
    : "";
  const errors = u.errors?.length
    ? `<p class="form-error">${u.errors.map(esc).join("; ")}</p>`
    : "";
```

`data-goto="apps"` switches tabs via `showTab("apps")` in a click handler. `EVENT_TEXT` uses `e.enforced`:
`locked` → `Locked: …` / `Would have locked: …`; `closed` → `Closed games: …` / `Would have closed …`; append
` on ${esc(e.host)}` when `e.host` is non-empty.

- [ ] **Step 3: Styles** — `.enforce { font-weight: 600; }` and `.decision-note { margin: 6px 0 0;
  font-size: 0.85rem; color: var(--text-secondary); }`.

- [ ] **Step 4: Check in a browser** against a local server (login check off), with the Playwright tools:
  the switch asks to confirm, saves, survives a reload, and reverts with a message when the server is stopped;
  an enforced kid's card says "Locked" outside hours; a report with `overrun`/`errors` shows both lines; the
  log shows "on host-a". No console errors.

- [ ] **Step 5: Commit**

```bash
git add crates/server/static
git commit -m "feat(dashboard): add the Enforce switch and show what enforcement did"
```

---

### Task 9: Install script, docs

**Files:**
- Modify: `deploy/install-agent.sh`, `README.md`, `CLAUDE.md`

- [ ] **Step 1: Install script options**

- `--stop-timekpr`: `systemctl disable --now timekpr.service` (ignore "not found"), print the result.
- `--uninstall`: if `/usr/local/bin/kidtime-agent` exists, run it with `--config /etc/kidtime/agent.toml
  --release-all`; then `systemctl disable --now kidtime-agent`, remove the unit, binary, `/etc/kidtime`, and
  `/var/lib/kidtime`; `systemctl daemon-reload`. Print each step.
- Every install: write the GDM profile override if the file doesn't exist, so later login screens read
  kidtime's banner settings:

```bash
if [ -z "$ROOT" ] && [ -e /usr/share/dconf/profile/gdm ] && [ ! -e /etc/dconf/profile/gdm ]; then
  install -d /etc/dconf/profile /etc/dconf/db/gdm.d
  { echo "user-db:user"; echo "system-db:gdm"; grep -v '^user-db:' /usr/share/dconf/profile/gdm; } > /etc/dconf/profile/gdm
  dconf update
  echo "enabled login-screen banner settings (/etc/dconf/profile/gdm)"
fi
```

Test with the existing fake-root mode (`KIDTIME_INSTALL_ROOT`): `--stop-timekpr` and `--uninstall` print what
they would do and skip system calls when `ROOT` is set (follow the script's existing pattern).

- [ ] **Step 2: Docs**

- `README.md`: an "Enforcement" section — the switch, what happens (warnings, lock and login disabled, streams
  cut, games closed), offline behaviour, `--stop-timekpr`, and **recovery**:
  `sudo usermod -U <kid>`, `sudo nft delete table inet kidtime`, `sudo /usr/local/bin/kidtime-agent --release-all`.
- `CLAUDE.md`: agent files (`enforce.rs`, `actions.rs`), the snapshot in the report response, the state file,
  the spike findings that shaped this (one line each), and the "Next steps" update (enforcement built; real-machine
  checklist next; "+30 min"/"lock now"/push after).

- [ ] **Step 3:** Gate. `docker build -t kidtime-server:dev . && docker rmi kidtime-server:dev`.

- [ ] **Step 4: Commit**

```bash
git add deploy README.md CLAUDE.md
git commit -m "docs: describe enforcement, recovery and the new install options"
```

---

## After the last task

Do not push. Report the branch, commits, gate results, browser check results. Landing is the user's decision:

1. Rebase onto `main`, fast-forward, push; the release bot cuts the next version.
2. Deploy the server (old agents keep working).
3. Install the new agent on both PCs with Enforce still off (`git pull`, `cargo build --release -p agent`,
   `sudo deploy/install-agent.sh` with no options to upgrade).
4. **Real-machine checklist, with the user and one kid account**, Enforce on for that kid only:
   - warnings at 10, 5, 1 minute before the end of allowed hours, on the desktop and in a stream (including over a
     fullscreen game);
   - at the end: locked, login refused, re-locked after a fast-user-switch; the login screen banner shows the line;
   - a stream from the gaming PC drops within seconds and can't reconnect; after allowed time, Resume works;
   - games budget: warnings, then games closed and kept closed; an uncategorised app is flagged on the dashboard;
   - the server stopped mid-session: enforcement continues;
   - reboot while blocked: still blocked after boot; allowed in the morning without the server;
   - `--uninstall` while blocked: login works again, no nftables table left;
   - Enforce off: everything released within one report cycle.
5. Then `--stop-timekpr` on both PCs.
