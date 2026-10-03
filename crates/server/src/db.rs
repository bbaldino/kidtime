//! SQLite storage.
//!
//! - `usage`: seconds per day/user/host/app, for the per-computer and per-app breakdowns.
//! - `activity`: when each user was active, as time intervals. A user's total is
//!   the union of their intervals, so time spent on two machines at once (e.g.
//!   streaming from one to the other) is only counted once.
//! - `agents`: the last sequence number recorded from each agent.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Datelike, Days, Local, NaiveDate, NaiveDateTime, TimeZone};
use protocol::{App, Report, Sample, UserSample, UserState};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::rules::{self, BlackoutSpan, CategoryId, Computer, DayRule, Decision, Stretch};

/// The one category created at first start.
pub const GAMES: CategoryId = 1;
/// Apps a person has marked as not worth tracking: hidden from the dashboard and never budgeted.
/// Their time is still recorded, so moving one back restores its history.
pub const IGNORED: CategoryId = 2;

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

/// A one-off blackout. `user: None` applies to every restricted account.
#[derive(Debug, Serialize)]
pub struct Blackout {
    pub id: i64,
    pub user: Option<String>,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub note: String,
}

/// A change in what the rules say for an account. Nothing is enforced yet, so these record what
/// would have happened.
#[derive(Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub user: String,
    pub at: i64,
    /// "locked", "closed" or "allowed". Rows of kind "state" are never returned.
    pub kind: String,
    pub detail: String,
}

const KEEP_SECS: i64 = 30 * 86400;
const ALLOWED_KEY: &str = "allowed|";
/// Kind of an event row that only remembers the latest decision, for later comparisons. Not shown.
const STATE_KIND: &str = "state";

/// Longest account name, host name, app id or app name that is stored, in characters.
const MAX_NAME_CHARS: usize = 200;
/// Most apps recorded for one user in one sample.
const MAX_APPS_PER_USER: usize = 50;

fn clip(text: &str) -> String {
    text.chars().take(MAX_NAME_CHARS).collect()
}

/// A copy of the report with what agents send cut down to what is worth storing: long names are
/// clipped, apps without an id are dropped, and each user keeps only their first apps.
fn sanitised(report: &Report) -> Report {
    Report {
        host: clip(&report.host),
        agent_id: report.agent_id.clone(),
        interval_secs: report.interval_secs,
        samples: report
            .samples
            .iter()
            .map(|sample| Sample {
                seq: sample.seq,
                at: sample.at,
                elapsed_secs: sample.elapsed_secs,
                users: sample
                    .users
                    .iter()
                    .map(|user| UserSample {
                        user: clip(&user.user),
                        state: user.state,
                        apps: user
                            .apps
                            .iter()
                            .filter(|app| !app.id.is_empty())
                            .take(MAX_APPS_PER_USER)
                            .map(|app| App {
                                id: clip(&app.id),
                                name: clip(&app.name),
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn decision_key(decision: &Decision) -> String {
    let computer = match &decision.computer {
        Computer::Allowed => "allowed",
        Computer::OutsideSchedule => "outside_schedule",
        Computer::Blackout { .. } => "blackout",
    };
    let used_up: Vec<String> = decision
        .categories
        .iter()
        .filter(|c| c.used_up)
        .map(|c| c.category.to_string())
        .collect();
    format!("{computer}|{}", used_up.join(","))
}

/// How local date-times are stored; this form sorts correctly as text.
const LOCAL_TIME: &str = "%Y-%m-%dT%H:%M:%S";

fn local_text(t: NaiveDateTime) -> String {
    t.format(LOCAL_TIME).to_string()
}

fn parse_local(text: String) -> rusqlite::Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(&text, LOCAL_TIME).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// `app_id` used for a user's overall total, as opposed to a single app.
const TOTAL: &str = "";

pub struct Db {
    conn: Connection,
}

pub struct AppUsage {
    pub name: String,
    pub secs: i64,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // The usual cause in a container: a bind-mounted data directory is created owned by root,
        // and the server runs as another user.
        let conn = Connection::open(path).with_context(|| {
            format!(
                "is {} writable by the user the server runs as?",
                path.parent().unwrap_or(path).display()
            )
        })?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS usage (
                 day      TEXT NOT NULL,
                 user     TEXT NOT NULL,
                 host     TEXT NOT NULL,
                 app_id   TEXT NOT NULL,
                 app_name TEXT NOT NULL,
                 secs     INTEGER NOT NULL,
                 PRIMARY KEY (day, user, host, app_id)
             );
             CREATE TABLE IF NOT EXISTS activity (
                 user  TEXT NOT NULL,
                 host  TEXT NOT NULL,
                 start INTEGER NOT NULL,
                 end   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS activity_user_end ON activity (user, end);
             CREATE TABLE IF NOT EXISTS agents (
                 host     TEXT PRIMARY KEY,
                 agent_id TEXT NOT NULL,
                 last_seq INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS account (
                 user      TEXT PRIMARY KEY,
                 last_seen INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS category (
                 id   INTEGER PRIMARY KEY,
                 name TEXT NOT NULL
             );
             INSERT OR IGNORE INTO category (id, name) VALUES (1, 'Games'), (2, 'Ignored');
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
             CREATE TABLE IF NOT EXISTS event (
                 id     INTEGER PRIMARY KEY,
                 user   TEXT NOT NULL,
                 at     INTEGER NOT NULL,
                 key    TEXT NOT NULL,
                 kind   TEXT NOT NULL,
                 detail TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS event_user_id ON event (user, id);",
        )?;
        Ok(Self { conn })
    }

    /// Adds the report's usage, skipping samples already recorded. Returns the report as it was
    /// recorded: names clipped and surplus apps dropped.
    pub fn record(&mut self, report: &Report) -> Result<Report> {
        // Everything below uses only this copy, so every table sees the same values
        let report = sanitised(report);
        let tx = self.conn.transaction()?;
        let last_seq: u64 = tx
            .query_row(
                "SELECT agent_id, last_seq FROM agents WHERE host = ?1",
                [&report.host],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?
            // A new agent id means the agent restarted and its numbering began again
            .filter(|(agent_id, _)| *agent_id == report.agent_id)
            .map_or(0, |(_, seq)| seq);

        let mut max_seq = last_seq;
        {
            let mut add = tx.prepare(
                "INSERT INTO usage (day, user, host, app_id, app_name, secs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (day, user, host, app_id) DO UPDATE SET secs = secs + excluded.secs, app_name = excluded.app_name",
            )?;
            let mut active = tx
                .prepare("INSERT INTO activity (user, host, start, end) VALUES (?1, ?2, ?3, ?4)")?;
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
            for sample in report.samples.iter().filter(|s| s.seq > last_seq) {
                max_seq = max_seq.max(sample.seq);
                let Some(at) = Local.timestamp_opt(sample.at, 0).single() else {
                    continue;
                };
                let day = at.date_naive().to_string();
                for user in &sample.users {
                    seen.execute(params![user.user, sample.at])?;
                }
                for user in sample.users.iter().filter(|u| u.state.counts()) {
                    active.execute(params![
                        user.user,
                        report.host,
                        sample.at - i64::from(sample.elapsed_secs),
                        sample.at
                    ])?;
                    add.execute(params![
                        day,
                        user.user,
                        report.host,
                        TOTAL,
                        "",
                        sample.elapsed_secs
                    ])?;
                    for app in &user.apps {
                        add.execute(params![
                            day,
                            user.user,
                            report.host,
                            app.id,
                            app.name,
                            sample.elapsed_secs
                        ])?;
                        let start = sample.at - i64::from(sample.elapsed_secs);
                        let auto = automatic_category(&app.id, user.state);
                        catalogue.execute(params![app.id, app.name, sample.at, auto])?;
                        if extend.execute(params![
                            user.user,
                            report.host,
                            app.id,
                            start,
                            sample.at
                        ])? == 0
                        {
                            stretch.execute(params![
                                user.user,
                                report.host,
                                app.id,
                                start,
                                sample.at
                            ])?;
                        }
                    }
                }
            }
        }
        tx.execute(
            "INSERT INTO agents (host, agent_id, last_seq) VALUES (?1, ?2, ?3)
             ON CONFLICT (host) DO UPDATE SET agent_id = excluded.agent_id, last_seq = excluded.last_seq",
            params![report.host, report.agent_id, max_seq as i64],
        )?;
        tx.commit()?;
        Ok(report)
    }

    /// Users with any usage since `since`.
    pub fn users_since(&self, since: NaiveDate) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT user FROM usage WHERE day >= ?1")?;
        let users = stmt
            .query_map([since.to_string()], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(users)
    }

    /// Seconds per day the user was active on any machine, for days in `from..=to`, oldest first.
    pub fn daily_totals(
        &self,
        user: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<(NaiveDate, i64)>> {
        let range_start = midnight(from);
        let range_end = midnight(to + Days::new(1));
        let mut stmt = self.conn.prepare(
            "SELECT start, end FROM activity WHERE user = ?1 AND end > ?2 AND start < ?3 ORDER BY start",
        )?;
        let intervals: Vec<(i64, i64)> = stmt
            .query_map(params![user, range_start, range_end], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let merged = merge(intervals);

        let mut totals = Vec::new();
        let mut day = from;
        while day <= to {
            let (start, end) = (midnight(day), midnight(day + Days::new(1)));
            let secs = merged
                .iter()
                .map(|&(s, e)| (e.min(end) - s.max(start)).max(0))
                .sum();
            totals.push((day, secs));
            day = day + Days::new(1);
        }
        Ok(totals)
    }

    /// Seconds per host for the user on `day`.
    pub fn host_totals(&self, user: &str, day: NaiveDate) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT host, secs FROM usage WHERE user = ?1 AND app_id = ?2 AND day = ?3 ORDER BY secs DESC",
        )?;
        let rows = stmt
            .query_map(params![user, TOTAL, day.to_string()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Seconds per app for the user on `day`, across hosts, largest first. Ignored apps are left out.
    pub fn app_totals(&self, user: &str, day: NaiveDate) -> Result<Vec<AppUsage>> {
        let mut stmt = self.conn.prepare(
            "SELECT MAX(u.app_name), SUM(u.secs) AS total FROM usage u LEFT JOIN app ON app.app_id = u.app_id
             WHERE u.user = ?1 AND u.app_id != ?2 AND u.day = ?3
               AND (app.category_id IS NULL OR app.category_id != ?4)
             GROUP BY u.app_id ORDER BY total DESC",
        )?;
        let rows = stmt
            .query_map(params![user, TOTAL, day.to_string(), IGNORED], |r| {
                Ok(AppUsage {
                    name: r.get(0)?,
                    secs: r.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Ids of apps in the Ignored category.
    pub fn ignored_app_ids(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT app_id FROM app WHERE category_id = ?1")?;
        let ids = stmt
            .query_map([IGNORED], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

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
    pub fn set_app_category(
        &mut self,
        app_id: &str,
        category_id: Option<CategoryId>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE app SET category_id = ?2, set_by_person = 1, reviewed = 1 WHERE app_id = ?1",
            params![app_id, category_id],
        )?;
        Ok(changed > 0)
    }

    pub fn categories(&self) -> Result<Vec<(CategoryId, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM category ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn is_account(&self, user: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM account WHERE user = ?1)",
            [user],
            |r| r.get(0),
        )?)
    }

    /// Seconds on `day` during which the user had an app of each category in use, on any host.
    /// Categories with no time are left out.
    ///
    /// An app nobody has categorised counts as a game: missing a play session is worse than
    /// overcounting, and sorting it into Ignored (or another category) stops it counting. Ignored
    /// apps count toward nothing.
    pub fn category_secs(&self, user: &str, day: NaiveDate) -> Result<BTreeMap<CategoryId, i64>> {
        let (day_start, day_end) = (midnight(day), midnight(day + Days::new(1)));
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(app.category_id, ?4) AS category, a.start, a.end
             FROM app_activity a JOIN app ON app.app_id = a.app_id
             WHERE a.user = ?1 AND a.end > ?2 AND a.start < ?3
               AND (app.category_id IS NULL OR app.category_id != ?5)
             ORDER BY category, a.start",
        )?;
        let mut by_category: BTreeMap<CategoryId, Vec<(i64, i64)>> = BTreeMap::new();
        let rows = stmt.query_map(params![user, day_start, day_end, GAMES, IGNORED], |r| {
            Ok((
                r.get::<_, CategoryId>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (category, start, end) = row?;
            by_category.entry(category).or_default().push((start, end));
        }
        Ok(by_category
            .into_iter()
            .map(|(category, intervals)| {
                let secs = merge(intervals)
                    .iter()
                    .map(|&(s, e)| (e.min(day_end) - s.max(day_start)).max(0))
                    .sum();
                (category, secs)
            })
            .collect())
    }

    pub fn day_rule(&self, user: &str, weekday: u8) -> Result<DayRule> {
        let restricted = self
            .conn
            .query_row(
                "SELECT restricted FROM day_rule WHERE user = ?1 AND weekday = ?2",
                params![user, weekday],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        let mut stmt = self.conn.prepare(
            "SELECT start_min, end_min FROM stretch WHERE user = ?1 AND weekday = ?2 ORDER BY start_min",
        )?;
        let stretches = stmt
            .query_map(params![user, weekday], |r| {
                Ok(Stretch {
                    start_min: r.get(0)?,
                    end_min: r.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        let mut stmt = self
            .conn
            .prepare("SELECT category_id, minutes FROM budget WHERE user = ?1 AND weekday = ?2")?;
        let budgets = stmt
            .query_map(params![user, weekday], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(DayRule {
            restricted,
            stretches,
            budgets,
        })
    }

    /// Monday first.
    pub fn week_rules(&self, user: &str) -> Result<Vec<DayRule>> {
        (0..7).map(|weekday| self.day_rule(user, weekday)).collect()
    }

    /// Replaces the weekday's rule. The caller validates it first.
    pub fn set_day_rule(&mut self, user: &str, weekday: u8, rule: &DayRule) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM day_rule WHERE user = ?1 AND weekday = ?2",
            params![user, weekday],
        )?;
        tx.execute(
            "DELETE FROM stretch WHERE user = ?1 AND weekday = ?2",
            params![user, weekday],
        )?;
        tx.execute(
            "DELETE FROM budget WHERE user = ?1 AND weekday = ?2",
            params![user, weekday],
        )?;
        if rule.restricted {
            tx.execute(
                "INSERT INTO day_rule (user, weekday, restricted) VALUES (?1, ?2, 1)",
                params![user, weekday],
            )?;
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

    pub fn add_blackout(
        &mut self,
        user: Option<&str>,
        start: NaiveDateTime,
        end: NaiveDateTime,
        note: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO blackout (user, start, end, note) VALUES (?1, ?2, ?3, ?4)",
            params![user, local_text(start), local_text(end), note],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_blackout(&mut self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM blackout WHERE id = ?1", [id])?
            > 0)
    }

    /// Blackouts that haven't ended at `now`, earliest start first.
    pub fn blackouts(&self, now: NaiveDateTime) -> Result<Vec<Blackout>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user, start, end, note FROM blackout WHERE end > ?1 ORDER BY start, id",
        )?;
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
            .map(|b| BlackoutSpan {
                start: b.start,
                end: b.end,
                note: b.note,
            })
            .collect();
        let used = self.category_secs(user, now.date())?;
        Ok(rules::decide(&rule, &spans, &used, now))
    }

    /// Remembers the decision if it differs from the account's last one. Returns whether that added
    /// an event to show: a new lock, a newly used-up category, or everything allowed again. Other
    /// changes are stored as rows that `events` leaves out.
    pub fn log_decision(&mut self, user: &str, at: i64, decision: &Decision) -> Result<bool> {
        let key = decision_key(decision);
        let last: String = self
            .conn
            .query_row(
                "SELECT key FROM event WHERE user = ?1 ORDER BY id DESC LIMIT 1",
                [user],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| ALLOWED_KEY.to_string());
        if key == last {
            return Ok(false);
        }
        let (last_computer, last_used_up) = last.split_once('|').unwrap_or((&last, ""));
        let (computer, _) = key.split_once('|').unwrap_or((&key, ""));

        let (kind, detail) = if computer != "allowed" && computer != last_computer {
            let detail = match &decision.computer {
                Computer::Blackout { note, .. } if !note.is_empty() => {
                    format!("blackout: {note}")
                }
                Computer::Blackout { .. } => "blackout".to_string(),
                _ => "outside schedule".to_string(),
            };
            ("locked", detail)
        } else {
            let before: Vec<&str> = last_used_up.split(',').collect();
            let names = self.categories()?;
            let name_of = |id: CategoryId| {
                names
                    .iter()
                    .find(|(known, _)| *known == id)
                    .map_or_else(|| id.to_string(), |(_, name)| name.clone())
            };
            let newly: Vec<String> = decision
                .categories
                .iter()
                .filter(|c| c.used_up && !before.contains(&c.category.to_string().as_str()))
                .map(|c| name_of(c.category))
                .collect();
            if !newly.is_empty() {
                ("closed", newly.join(", "))
            } else if key == ALLOWED_KEY {
                ("allowed", String::new())
            } else {
                // Something eased but not everything, e.g. the lock ended with games still used
                // up. "Allowed again" would be wrong, so only the new key is kept.
                (STATE_KIND, String::new())
            }
        };
        self.conn.execute(
            "INSERT INTO event (user, at, key, kind, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user, at, key, kind, detail],
        )?;
        Ok(kind != STATE_KIND)
    }

    /// Newest first.
    pub fn events(&self, user: Option<&str>, limit: u32) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user, at, kind, detail FROM event
             WHERE (?1 IS NULL OR user = ?1) AND kind != ?3 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![user, limit, STATE_KIND], |r| {
                Ok(Event {
                    id: r.get(0)?,
                    user: r.get(1)?,
                    at: r.get(2)?,
                    kind: r.get(3)?,
                    detail: r.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Drops per-app stretches and events older than 30 days. Daily totals are kept.
    pub fn prune(&mut self, now: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM app_activity WHERE end < ?1", [now - KEEP_SECS])?;
        self.conn
            .execute("DELETE FROM event WHERE at < ?1", [now - KEEP_SECS])?;
        Ok(())
    }
}

/// Unix time of local midnight at the start of `day`.
fn midnight(day: NaiveDate) -> i64 {
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight is valid"))
        .earliest()
        .map_or(0, |t| t.timestamp())
}

/// The category an app gets when nobody has chosen one: Steam-launched apps, and anything in a stream
/// except the Steam client, are games.
fn automatic_category(app_id: &str, state: UserState) -> Option<CategoryId> {
    let streamed_game = state == UserState::Streaming && app_id != "steam";
    (app_id.starts_with("steam:") || streamed_game).then_some(GAMES)
}

/// Merges intervals sorted by start into non-overlapping ones.
fn merge(intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(intervals.len());
    for (start, end) in intervals {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(agent_id: &str, seqs: &[u64], state: UserState) -> Report {
        report_from("pc", agent_id, seqs, state, start_of_test())
    }

    /// Early enough today that the test's samples stay within today.
    fn start_of_test() -> i64 {
        midnight(Local::now().date_naive()) + 60
    }

    /// Samples 15s apart; sample `seq` ends at `t0 + 15 * seq`.
    fn report_from(host: &str, agent_id: &str, seqs: &[u64], state: UserState, t0: i64) -> Report {
        Report {
            host: host.into(),
            agent_id: agent_id.into(),
            interval_secs: 15,
            samples: seqs
                .iter()
                .map(|&seq| Sample {
                    seq,
                    at: t0 + 15 * seq as i64,
                    elapsed_secs: 15,
                    users: vec![UserSample {
                        user: "kid1".into(),
                        state,
                        apps: vec![App {
                            id: "steam:1".into(),
                            name: "Minecraft".into(),
                        }],
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn counts_each_sample_once() {
        let path = std::env::temp_dir().join(format!("kidtime-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Db::open(&path).unwrap();
        let today = Local::now().date_naive();
        let total = |db: &Db| db.daily_totals("kid1", today, today).unwrap()[0].1;

        db.record(&report("a", &[1, 2], UserState::Active)).unwrap();
        // A retried batch overlapping what was already recorded
        db.record(&report("a", &[2, 3], UserState::Active)).unwrap();
        assert_eq!(total(&db), 45);
        // Idle time isn't usage
        db.record(&report("a", &[4], UserState::Idle)).unwrap();
        assert_eq!(total(&db), 45);
        // After a restart the agent numbers from 1 again
        db.record(&report_from(
            "pc",
            "b",
            &[1],
            UserState::Streaming,
            start_of_test() + 600,
        ))
        .unwrap();
        assert_eq!(total(&db), 60);
        assert_eq!(db.app_totals("kid1", today).unwrap()[0].secs, 60);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn overlapping_hosts_count_once() {
        let path =
            std::env::temp_dir().join(format!("kidtime-test-overlap-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Db::open(&path).unwrap();
        let today = Local::now().date_naive();

        // A kid streams: the host sees a stream and the client PC sees an active desktop, for the same minute
        db.record(&report_from(
            "host-a",
            "a",
            &[1, 2, 3, 4],
            UserState::Streaming,
            start_of_test(),
        ))
        .unwrap();
        db.record(&report_from(
            "host-b",
            "b",
            &[1, 2, 3, 4],
            UserState::Active,
            start_of_test(),
        ))
        .unwrap();
        assert_eq!(db.daily_totals("kid1", today, today).unwrap()[0].1, 60);
        // Each computer still shows its own time
        assert_eq!(
            db.host_totals("kid1", today)
                .unwrap()
                .iter()
                .map(|h| h.1)
                .sum::<i64>(),
            120
        );

        std::fs::remove_file(&path).unwrap();
    }

    fn temp_db(name: &str) -> (Db, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("kidtime-test-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (Db::open(&path).unwrap(), path)
    }

    /// One 15-second sample ending at `at`, for kid1, with the given apps.
    fn sample_report(
        host: &str,
        seq: u64,
        at: i64,
        state: UserState,
        apps: &[(&str, &str)],
    ) -> Report {
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
                    apps: apps
                        .iter()
                        .map(|&(id, name)| App {
                            id: id.into(),
                            name: name.into(),
                        })
                        .collect(),
                }],
            }],
        }
    }

    #[test]
    fn apps_are_catalogued_and_auto_categorised() {
        let (mut db, path) = temp_db("catalogue");
        let t0 = start_of_test();
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Active,
            &[("steam:1", "Minecraft"), ("org.example.Editor", "Editor")],
        ))
        .unwrap();
        db.record(&sample_report(
            "host-a",
            2,
            t0 + 30,
            UserState::Streaming,
            &[("window:Some Game", "Some Game"), ("steam", "Steam")],
        ))
        .unwrap();

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
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Active,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
        assert!(db.set_app_category("steam:1", None).unwrap());
        db.record(&sample_report(
            "host-a",
            2,
            t0 + 30,
            UserState::Streaming,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
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
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Active,
            &[("window:Game", "Game")],
        ))
        .unwrap();
        assert_eq!(db.apps().unwrap()[0].category_id, None);
        db.record(&sample_report(
            "host-a",
            2,
            t0 + 30,
            UserState::Streaming,
            &[("window:Game", "Game")],
        ))
        .unwrap();
        assert_eq!(db.apps().unwrap()[0].category_id, Some(GAMES));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn back_to_back_samples_merge_into_one_stretch() {
        let (mut db, path) = temp_db("merge");
        let t0 = start_of_test();
        for seq in 1..=4 {
            db.record(&sample_report(
                "host-a",
                seq,
                t0 + 15 * seq as i64,
                UserState::Active,
                &[("steam:1", "Minecraft")],
            ))
            .unwrap();
        }
        // A gap, then one more sample
        db.record(&sample_report(
            "host-a",
            5,
            t0 + 600,
            UserState::Active,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM app_activity", [], |r| r.get(0))
            .unwrap();
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
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Streaming,
            &[("steam:1", "Minecraft"), ("steam:2", "Portal")],
        ))
        .unwrap();
        db.record(&sample_report(
            "host-b",
            1,
            t0 + 15,
            UserState::Active,
            &[("steam:1", "Minecraft"), ("org.example.Viewer", "Viewer")],
        ))
        .unwrap();
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
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Active,
            &[("org.example.Launcher", "Launcher")],
        ))
        .unwrap();
        let today = Local::now().date_naive();
        db.set_app_category("org.example.Launcher", Some(IGNORED))
            .unwrap();
        assert!(db.category_secs("kid1", today).unwrap().is_empty());
        db.set_app_category("org.example.Launcher", Some(GAMES))
            .unwrap();
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 15);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn uncategorised_time_counts_toward_games_and_ignored_time_does_not() {
        let (mut db, path) = temp_db("uncategorised");
        let t0 = start_of_test();
        let today = Local::now().date_naive();
        // 15s with an uncategorised app only
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Active,
            &[("org.example.New", "New")],
        ))
        .unwrap();
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 15);
        // Ignoring it removes that time from Games, and Ignored itself is never reported
        db.set_app_category("org.example.New", Some(IGNORED))
            .unwrap();
        assert!(db.category_secs("kid1", today).unwrap().is_empty());
        // A Games app and an uncategorised one at the same time count once
        db.record(&sample_report(
            "host-a",
            2,
            t0 + 300,
            UserState::Active,
            &[("steam:1", "Minecraft"), ("org.example.Other", "Other")],
        ))
        .unwrap();
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 15);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn ignored_apps_are_still_recorded_and_hidden_from_app_totals() {
        let (mut db, path) = temp_db("ignored");
        let t0 = start_of_test();
        let today = Local::now().date_naive();
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Active,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
        db.set_app_category("steam:1", Some(IGNORED)).unwrap();
        db.record(&sample_report(
            "host-a",
            2,
            t0 + 30,
            UserState::Active,
            &[("steam:1", "Minecraft"), ("org.example.Editor", "Editor")],
        ))
        .unwrap();
        let names: Vec<String> = db
            .app_totals("kid1", today)
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, ["Editor"]);
        assert_eq!(
            db.ignored_app_ids().unwrap(),
            std::collections::HashSet::from(["steam:1".to_string()])
        );
        // Unignoring brings the whole day back: both samples were recorded
        db.set_app_category("steam:1", Some(GAMES)).unwrap();
        let minecraft = db
            .app_totals("kid1", today)
            .unwrap()
            .into_iter()
            .find(|a| a.name == "Minecraft")
            .unwrap();
        assert_eq!(minecraft.secs, 30);
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 30);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_sample_crossing_midnight_is_split_between_the_days() {
        let (mut db, path) = temp_db("midnight");
        let today = Local::now().date_naive();
        let yesterday = today - Days::new(1);
        // Ends 5 seconds into today, so 10 seconds belong to yesterday
        db.record(&sample_report(
            "host-a",
            1,
            midnight(today) + 5,
            UserState::Active,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
        assert_eq!(db.category_secs("kid1", yesterday).unwrap()[&GAMES], 10);
        assert_eq!(db.category_secs("kid1", today).unwrap()[&GAMES], 5);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn idle_samples_add_no_app_time() {
        let (mut db, path) = temp_db("idle");
        db.record(&sample_report(
            "host-a",
            1,
            start_of_test() + 15,
            UserState::Idle,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
        assert!(
            db.category_secs("kid1", Local::now().date_naive())
                .unwrap()
                .is_empty()
        );
        assert!(db.apps().unwrap().is_empty());
        // The account is still known: it reported
        assert!(db.is_account("kid1").unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    use crate::rules::{CategoryStatus, Computer, DayRule, Decision, Stretch};

    fn noon_today() -> chrono::NaiveDateTime {
        Local::now().date_naive().and_hms_opt(12, 0, 0).unwrap()
    }

    fn weekday_today() -> u8 {
        Local::now().date_naive().weekday().num_days_from_monday() as u8
    }

    #[test]
    fn day_rules_round_trip_and_replace() {
        let (mut db, path) = temp_db("rules");
        assert_eq!(db.day_rule("kid1", 0).unwrap(), DayRule::default());
        assert!(!db.is_restricted("kid1").unwrap());

        let rule = DayRule {
            restricted: true,
            stretches: vec![
                Stretch {
                    start_min: 960,
                    end_min: 1200,
                },
                Stretch {
                    start_min: 375,
                    end_min: 450,
                },
            ],
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
        let rule = DayRule {
            restricted: false,
            stretches: vec![],
            budgets: BTreeMap::from([(GAMES, 30)]),
        };
        db.set_day_rule("kid1", 3, &rule).unwrap();
        assert!(db.is_restricted("kid1").unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn blackouts_list_only_those_not_ended() {
        let (mut db, path) = temp_db("blackouts");
        let now = noon_today();
        let hour = chrono::Duration::hours(1);
        db.add_blackout(Some("kid1"), now - hour * 3, now - hour, "over")
            .unwrap();
        let current = db
            .add_blackout(None, now - hour, now + hour, "now")
            .unwrap();
        db.add_blackout(Some("kid2"), now + hour * 5, now + hour * 6, "later")
            .unwrap();
        let listed = db.blackouts(now).unwrap();
        assert_eq!(
            listed.iter().map(|b| b.note.as_str()).collect::<Vec<_>>(),
            ["now", "later"]
        );
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
        db.add_blackout(None, now - hour, now + hour, "everyone")
            .unwrap();

        // No rules: an "everyone" blackout doesn't touch this account
        assert_eq!(
            db.decision("parent", now).unwrap().computer,
            Computer::Allowed
        );

        let rule = DayRule {
            restricted: false,
            stretches: vec![],
            budgets: BTreeMap::from([(GAMES, 1)]),
        };
        db.set_day_rule("kid1", weekday_today(), &rule).unwrap();
        let d = db.decision("kid1", now).unwrap();
        assert_eq!(
            d.computer,
            Computer::Blackout {
                until: now + hour,
                note: "everyone".into()
            }
        );

        // A blackout naming another account doesn't apply
        db.set_day_rule("kid2", weekday_today(), &rule).unwrap();
        db.add_blackout(Some("kid1"), now - hour, now + hour * 4, "kid1 only")
            .unwrap();
        let d = db.decision("kid2", now).unwrap();
        assert_eq!(
            d.computer,
            Computer::Blackout {
                until: now + hour,
                note: "everyone".into()
            }
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn decision_uses_todays_rule_and_todays_category_time() {
        let (mut db, path) = temp_db("decision-budget");
        let rule = DayRule {
            restricted: false,
            stretches: vec![],
            budgets: BTreeMap::from([(GAMES, 1)]),
        };
        db.set_day_rule("kid1", weekday_today(), &rule).unwrap();
        let t0 = start_of_test();
        for seq in 1..=4 {
            db.record(&sample_report(
                "host-a",
                seq,
                t0 + 15 * seq as i64,
                UserState::Active,
                &[("steam:1", "Minecraft")],
            ))
            .unwrap();
        }
        let d = db.decision("kid1", noon_today()).unwrap();
        assert_eq!(d.categories[0].used_secs, 60);
        assert!(d.categories[0].used_up);
        std::fs::remove_file(&path).unwrap();
    }

    fn decision_of(computer: Computer, used_up: &[CategoryId]) -> Decision {
        Decision {
            computer,
            categories: used_up
                .iter()
                .map(|&category| CategoryStatus {
                    category,
                    used_secs: 60,
                    left_secs: 0,
                    used_up: true,
                })
                .collect(),
            next_change: noon_today(),
        }
    }

    #[test]
    fn events_are_logged_once_per_change() {
        let (mut db, path) = temp_db("events");
        let allowed = decision_of(Computer::Allowed, &[]);
        let outside = decision_of(Computer::OutsideSchedule, &[]);
        let blackout = decision_of(
            Computer::Blackout {
                until: noon_today(),
                note: "dinner".into(),
            },
            &[],
        );
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
        let seen: Vec<(&str, &str)> = kid1
            .iter()
            .rev()
            .map(|e| (e.kind.as_str(), e.detail.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("locked", "outside schedule"),
                ("locked", "blackout: dinner"),
                ("allowed", ""),
                ("closed", "Games")
            ]
        );
        assert_eq!(db.events(Some("kid1"), 2).unwrap().len(), 2);
        std::fs::remove_file(&path).unwrap();
    }

    fn kinds(db: &Db) -> Vec<(String, String)> {
        db.events(Some("kid1"), 50)
            .unwrap()
            .into_iter()
            .rev()
            .map(|e| (e.kind, e.detail))
            .collect()
    }

    fn pair(kind: &str, detail: &str) -> (String, String) {
        (kind.to_string(), detail.to_string())
    }

    #[test]
    fn a_budget_reset_while_locked_is_not_logged_as_allowed() {
        let (mut db, path) = temp_db("events-reset");
        db.log_decision(
            "kid1",
            100,
            &decision_of(Computer::OutsideSchedule, &[GAMES]),
        )
        .unwrap();
        // Midnight: the budget is fresh, but the computer is still outside its hours
        assert!(
            !db.log_decision("kid1", 110, &decision_of(Computer::OutsideSchedule, &[]))
                .unwrap()
        );
        assert_eq!(kinds(&db), [pair("locked", "outside schedule")]);
        // The change was still remembered: the same decision again adds no row
        let rows = |db: &Db| -> i64 {
            db.conn
                .query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(rows(&db), 2);
        db.log_decision("kid1", 120, &decision_of(Computer::OutsideSchedule, &[]))
            .unwrap();
        assert_eq!(rows(&db), 2);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unlocking_with_games_still_used_up_is_not_logged_as_allowed() {
        let (mut db, path) = temp_db("events-unlock");
        db.log_decision(
            "kid1",
            100,
            &decision_of(Computer::OutsideSchedule, &[GAMES]),
        )
        .unwrap();
        assert!(
            !db.log_decision("kid1", 110, &decision_of(Computer::Allowed, &[GAMES]))
                .unwrap()
        );
        // Games were already used up before, so there is nothing new to close either
        assert_eq!(kinds(&db), [pair("locked", "outside schedule")]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unlocking_with_nothing_used_up_is_logged_as_allowed() {
        let (mut db, path) = temp_db("events-allowed");
        db.log_decision(
            "kid1",
            100,
            &decision_of(Computer::OutsideSchedule, &[GAMES]),
        )
        .unwrap();
        assert!(
            db.log_decision("kid1", 110, &decision_of(Computer::Allowed, &[]))
                .unwrap()
        );
        assert_eq!(
            kinds(&db),
            [pair("locked", "outside schedule"), pair("allowed", "")]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unlocking_into_a_newly_used_up_category_is_logged_as_closed() {
        let (mut db, path) = temp_db("events-unlock-closed");
        db.log_decision("kid1", 100, &decision_of(Computer::OutsideSchedule, &[]))
            .unwrap();
        assert!(
            db.log_decision("kid1", 110, &decision_of(Computer::Allowed, &[GAMES]))
                .unwrap()
        );
        assert_eq!(
            kinds(&db),
            [pair("locked", "outside schedule"), pair("closed", "Games")]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn categories_used_up_one_after_the_other_each_log_closed() {
        let (mut db, path) = temp_db("events-two");
        db.conn
            .execute("INSERT INTO category (id, name) VALUES (3, 'Videos')", [])
            .unwrap();
        db.log_decision("kid1", 100, &decision_of(Computer::Allowed, &[GAMES]))
            .unwrap();
        db.log_decision("kid1", 110, &decision_of(Computer::Allowed, &[GAMES, 3]))
            .unwrap();
        assert_eq!(
            kinds(&db),
            [pair("closed", "Games"), pair("closed", "Videos")]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_used_up_category_cleared_is_logged_as_allowed() {
        let (mut db, path) = temp_db("events-cleared");
        db.log_decision("kid1", 100, &decision_of(Computer::Allowed, &[GAMES]))
            .unwrap();
        assert!(
            db.log_decision("kid1", 110, &decision_of(Computer::Allowed, &[]))
                .unwrap()
        );
        assert_eq!(kinds(&db), [pair("closed", "Games"), pair("allowed", "")]);
        std::fs::remove_file(&path).unwrap();
    }

    fn count(db: &Db, sql: &str) -> i64 {
        db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn long_reported_strings_are_cut_to_the_limit() {
        let (mut db, path) = temp_db("long");
        // Multi-byte characters, so a byte-index cut would land inside one
        let long = "é".repeat(1000);
        let mut report = sample_report(
            &long,
            1,
            start_of_test() + 15,
            UserState::Active,
            &[(&long, &long)],
        );
        report.samples[0].users[0].user = long.clone();
        db.record(&report).unwrap();

        let app = db.apps().unwrap().remove(0);
        assert_eq!(app.app_id.chars().count(), 200);
        assert_eq!(app.name.chars().count(), 200);
        let cut: String = long.chars().take(200).collect();
        assert!(db.is_account(&cut).unwrap());
        let today = Local::now().date_naive();
        // Every table saw the same cut values
        assert_eq!(db.daily_totals(&cut, today, today).unwrap()[0].1, 15);
        assert_eq!(db.host_totals(&cut, today).unwrap()[0].0, cut);
        assert_eq!(db.app_totals(&cut, today).unwrap()[0].name, cut);
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM app_activity WHERE LENGTH(user) = 200 AND LENGTH(host) = 200 AND LENGTH(app_id) = 200"
            ),
            1
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_app_with_an_empty_id_is_skipped() {
        let (mut db, path) = temp_db("empty-id");
        db.record(&sample_report(
            "host-a",
            1,
            start_of_test() + 15,
            UserState::Active,
            &[("", "Nameless")],
        ))
        .unwrap();
        assert!(db.apps().unwrap().is_empty());
        // The only row with an empty app id is the host total, counted once
        assert_eq!(count(&db, "SELECT COUNT(*) FROM usage"), 1);
        assert_eq!(
            count(&db, "SELECT secs FROM usage WHERE app_id = ''"),
            15,
            "the host total must not be doubled by the nameless app"
        );
        assert_eq!(count(&db, "SELECT COUNT(*) FROM app_activity"), 0);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn only_the_first_fifty_apps_of_a_sample_are_recorded() {
        let (mut db, path) = temp_db("many-apps");
        let ids: Vec<String> = (0..60).map(|i| format!("app-{i:02}")).collect();
        let apps: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "App")).collect();
        db.record(&sample_report(
            "host-a",
            1,
            start_of_test() + 15,
            UserState::Active,
            &apps,
        ))
        .unwrap();
        let catalogue = db.apps().unwrap();
        assert_eq!(catalogue.len(), 50);
        assert!(catalogue.iter().any(|a| a.app_id == "app-49"));
        assert!(catalogue.iter().all(|a| a.app_id != "app-50"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn prune_removes_rows_older_than_thirty_days() {
        let (mut db, path) = temp_db("prune");
        let now = start_of_test();
        let old = now - 31 * 86400;
        db.record(&sample_report(
            "host-a",
            1,
            old,
            UserState::Active,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
        db.record(&sample_report(
            "host-a",
            2,
            now,
            UserState::Active,
            &[("steam:1", "Minecraft")],
        ))
        .unwrap();
        db.log_decision("kid1", old, &decision_of(Computer::OutsideSchedule, &[]))
            .unwrap();
        db.log_decision("kid1", now, &decision_of(Computer::Allowed, &[]))
            .unwrap();

        db.prune(now).unwrap();
        let count = |table: &str| -> i64 {
            db.conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("app_activity"), 1);
        assert_eq!(count("event"), 1);
        // Totals don't depend on the pruned tables
        // Two days, each with a host total and one app
        assert_eq!(count("usage"), 4);
        std::fs::remove_file(&path).unwrap();
    }
}
