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

use protocol::rules::{
    self, BlackoutSpan, CategoryId, Computer, DayRule, Decision, Stretch, Timer, TimerMode,
};
pub use protocol::rules::{GAMES, IGNORED};

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

/// How long a message waits for its account to be at a computer.
pub const MESSAGE_TTL_SECS: i64 = 600;
/// The longest message the dashboard may send.
pub const MAX_MESSAGE_CHARS: usize = 200;

/// A message as the dashboard lists it.
#[derive(Debug, Serialize)]
pub struct MessageRow {
    pub id: i64,
    pub user: String,
    pub text: String,
    pub created: i64,
    pub expires: i64,
    /// The PC it was shown on, and when; `None` while waiting or after it expired unshown.
    pub shown_host: Option<String>,
    pub shown_at: Option<i64>,
}

/// A change in what the rules say for an account: what enforcement did, or for an account with
/// Enforce off, what it would have done.
#[derive(Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub user: String,
    pub at: i64,
    /// "locked", "closed" or "allowed". Rows of kind "state" are never returned.
    pub kind: String,
    pub detail: String,
    /// Whether Enforce was on for the account when this happened.
    pub enforced: bool,
    pub host: String,
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

/// Each string clipped, and only the first `MAX_APPS_PER_USER` kept.
fn clipped_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .take(MAX_APPS_PER_USER)
        .map(|item| clip(item))
        .collect()
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
                        overrun: clipped_list(&user.overrun),
                        errors: clipped_list(&user.errors),
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
        Computer::TimerEnded { .. } => "timer_ended",
    };
    let used_up: Vec<String> = decision
        .categories
        .iter()
        .filter(|c| c.used_up)
        .map(|c| c.category.to_string())
        .collect();
    format!("{computer}|{}", used_up.join(","))
}

/// SQLite has no `ADD COLUMN IF NOT EXISTS`; databases from older versions get the column here.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let exists: bool = conn.query_row(
        &format!("SELECT EXISTS (SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
        [column],
        |r| r.get(0),
    )?;
    if !exists {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

/// How local date-times are stored; this form sorts correctly as text.
const LOCAL_TIME: &str = "%Y-%m-%dT%H:%M:%S";
fn timer_mode_text(mode: TimerMode) -> &'static str {
    match mode {
        TimerMode::Lock => "lock",
        TimerMode::Games => "games",
    }
}

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

/// One block on a day's timeline: an app in use from `start` to `end` (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineBlock {
    pub name: String,
    pub start: i64,
    pub end: i64,
    /// The computers it ran on during the block, sorted.
    pub hosts: Vec<String>,
}

/// One recorded stretch of an app on one computer.
pub struct AppStretch {
    pub app_id: String,
    pub name: String,
    pub host: String,
    pub start: i64,
    pub end: i64,
}

/// A pause shorter than this doesn't split a timeline block, and a block shorter than this is left out.
const TIMELINE_MIN_SECS: i64 = 60;

/// Turns recorded stretches into timeline blocks for the day `day_start..day_end`: computers are merged,
/// pauses under a minute are bridged, blocks under a minute are dropped, and blocks are cut at the day's
/// edges. Sorted by start, then name.
pub fn timeline_blocks(
    mut stretches: Vec<AppStretch>,
    day_start: i64,
    day_end: i64,
) -> Vec<TimelineBlock> {
    stretches.sort_by(|a, b| (&a.app_id, a.start).cmp(&(&b.app_id, b.start)));
    let mut blocks: Vec<(String, TimelineBlock)> = Vec::new();
    for s in stretches {
        let (start, end) = (s.start.max(day_start), s.end.min(day_end));
        if end <= start {
            continue;
        }
        match blocks.last_mut() {
            Some((app_id, last)) if *app_id == s.app_id && start - last.end < TIMELINE_MIN_SECS => {
                last.end = last.end.max(end);
                if !last.hosts.contains(&s.host) {
                    last.hosts.push(s.host);
                }
            }
            _ => blocks.push((
                s.app_id,
                TimelineBlock {
                    name: s.name,
                    start,
                    end,
                    hosts: vec![s.host],
                },
            )),
        }
    }
    let mut blocks: Vec<TimelineBlock> = blocks
        .into_iter()
        .map(|(_, mut b)| {
            b.hosts.sort();
            b
        })
        .filter(|b| b.end - b.start >= TIMELINE_MIN_SECS)
        .collect();
    blocks.sort_by(|a, b| (a.start, &a.name).cmp(&(b.start, &b.name)));
    blocks
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
             CREATE INDEX IF NOT EXISTS event_user_id ON event (user, id);
             CREATE TABLE IF NOT EXISTS timer (
                 user    TEXT PRIMARY KEY,
                 started INTEGER NOT NULL,
                 ends    TEXT NOT NULL,
                 mode    TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS message (
                 id         INTEGER PRIMARY KEY,
                 user       TEXT NOT NULL,
                 text       TEXT NOT NULL,
                 created    INTEGER NOT NULL,
                 expires    INTEGER NOT NULL,
                 shown_host TEXT,
                 shown_at   INTEGER
             );
             CREATE INDEX IF NOT EXISTS message_user_expires ON message (user, expires);
             CREATE TABLE IF NOT EXISTS account_settings (
                 user    TEXT PRIMARY KEY,
                 enforce INTEGER NOT NULL
             );",
        )?;
        add_column_if_missing(&conn, "event", "enforced", "INTEGER NOT NULL DEFAULT 0")?;
        add_column_if_missing(&conn, "event", "host", "TEXT NOT NULL DEFAULT ''")?;
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

    /// Accounts with recorded time on any day in `from..=to`.
    pub fn users_between(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT user FROM usage WHERE day >= ?1 AND day <= ?2")?;
        let users = stmt
            .query_map([from.to_string(), to.to_string()], |r| r.get(0))?
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

    /// The day's timeline for one account: which app was in use when, leaving out Ignored apps. Names are
    /// the ones `app_totals` gives for that day, so the two can be matched up.
    pub fn timeline(&self, user: &str, day: NaiveDate) -> Result<Vec<TimelineBlock>> {
        let (day_start, day_end) = (midnight(day), midnight(day + Days::new(1)));
        let mut stmt = self.conn.prepare(
            "SELECT a.app_id,
                    COALESCE((SELECT MAX(u.app_name) FROM usage u
                              WHERE u.user = a.user AND u.app_id = a.app_id AND u.day = ?4),
                             app.name, a.app_id),
                    a.host, a.start, a.end
             FROM app_activity a LEFT JOIN app ON app.app_id = a.app_id
             WHERE a.user = ?1 AND a.end > ?2 AND a.start < ?3
               AND (app.category_id IS NULL OR app.category_id != ?5)",
        )?;
        let stretches = stmt
            .query_map(
                params![user, day_start, day_end, day.to_string(), IGNORED],
                |r| {
                    Ok(AppStretch {
                        app_id: r.get(0)?,
                        name: r.get(1)?,
                        host: r.get(2)?,
                        start: r.get(3)?,
                        end: r.get(4)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(timeline_blocks(stretches, day_start, day_end))
    }

    /// The unix seconds at which `day` starts and ends in the server's time zone.
    pub fn day_bounds(day: NaiveDate) -> (i64, i64) {
        (midnight(day), midnight(day + Days::new(1)))
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
        write_day_rule(&tx, user, weekday, rule)?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces each target account's whole week with `from`'s, in one transaction. The caller
    /// checks that the accounts exist.
    pub fn copy_week(&mut self, from: &str, to: &[String]) -> Result<()> {
        let week = self.week_rules(from)?;
        let tx = self.conn.transaction()?;
        for user in to {
            for (weekday, rule) in (0..).zip(&week) {
                write_day_rule(&tx, user, weekday, rule)?;
            }
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
            .map(|b| BlackoutSpan {
                start: b.start,
                end: b.end,
                note: b.note,
            })
            .collect())
    }

    pub fn enforce(&self, user: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT enforce FROM account_settings WHERE user = ?1",
                [user],
                |r| r.get(0),
            )
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
        let mut stmt = self
            .conn
            .prepare("SELECT app_id FROM app WHERE category_id = ?1 ORDER BY app_id")?;
        let ids = stmt
            .query_map([category], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    pub fn snapshot(&self, user: &str, now: NaiveDateTime) -> Result<protocol::AccountSnapshot> {
        Ok(protocol::AccountSnapshot {
            user: user.to_string(),
            enforce: self.enforce(user)?,
            for_day: now.date(),
            week: self.week_rules(user)?,
            blackouts: self.blackout_spans(user, now)?,
            used_secs: self.category_secs(user, now.date())?,
            games: self.app_ids_in(GAMES)?,
            ignored: self.app_ids_in(IGNORED)?,
            timer: self.timer(user, now)?,
        })
    }

    /// What the rules say for the account at `now`.
    pub fn decision(&self, user: &str, now: NaiveDateTime) -> Result<Decision> {
        let weekday = now.date().weekday().num_days_from_monday() as u8;
        let rule = self.day_rule(user, weekday)?;
        let spans = self.blackout_spans(user, now)?;
        let used = self.category_secs(user, now.date())?;
        let timer = self.timer(user, now)?;
        Ok(rules::decide_with_timer(
            &rule,
            &spans,
            &used,
            timer.as_ref(),
            now,
        ))
    }

    /// Starts (or replaces) the account's timer: it ends `minutes` after `now`.
    pub fn start_timer(
        &mut self,
        user: &str,
        minutes: u32,
        mode: TimerMode,
        now: NaiveDateTime,
    ) -> Result<Timer> {
        // Whole seconds: that's what is stored, so the reply and later reads agree
        let ends = now + chrono::Duration::minutes(i64::from(minutes));
        let ends = chrono::Timelike::with_nanosecond(&ends, 0).unwrap_or(ends);
        let timer = Timer { ends, mode };
        self.conn.execute(
            "INSERT INTO timer (user, started, ends, mode) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user) DO UPDATE SET started = excluded.started, ends = excluded.ends, mode = excluded.mode",
            params![user, now.and_local_timezone(Local).earliest().map_or(0, |t| t.timestamp()), local_text(timer.ends), timer_mode_text(mode)],
        )?;
        Ok(timer)
    }

    /// Removes the account's timer ("Cancel" or "Allow again"). False if it had none.
    pub fn cancel_timer(&mut self, user: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM timer WHERE user = ?1", [user])?
            > 0)
    }

    /// The account's timer, unless its stop has already passed at `now`.
    pub fn timer(&self, user: &str, now: NaiveDateTime) -> Result<Option<Timer>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT ends, mode FROM timer WHERE user = ?1",
                [user],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((ends, mode)) = row else {
            return Ok(None);
        };
        let timer = Timer {
            ends: parse_local(ends)?,
            mode: if mode == "games" {
                TimerMode::Games
            } else {
                TimerMode::Lock
            },
        };
        Ok((now < timer.stop_until()).then_some(timer))
    }

    /// Remembers the decision if it differs from the account's last one. Returns whether that added
    /// an event to show: a new lock, a newly used-up category, or everything allowed again. Other
    /// changes are stored as rows that `events` leaves out.
    pub fn log_decision(
        &mut self,
        user: &str,
        at: i64,
        decision: &Decision,
        enforced: bool,
        host: &str,
    ) -> Result<bool> {
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
                Computer::TimerEnded { .. } => "timer ended".to_string(),
                _ => "outside schedule".to_string(),
            };
            ("locked", detail)
        } else {
            let before: Vec<&str> = last_used_up.split(',').collect();
            let names = self.categories()?;
            // Games used up because a games timer ended, not because of the budget
            let at_local = Local.timestamp_opt(at, 0).single().map(|t| t.naive_local());
            let games_timer_ended = match at_local {
                Some(t) => self
                    .timer(user, t)?
                    .is_some_and(|timer| timer.mode == TimerMode::Games && t >= timer.ends),
                None => false,
            };
            let name_of = |id: CategoryId| {
                if id == GAMES && games_timer_ended {
                    return "timer ended".to_string();
                }
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
            "INSERT INTO event (user, at, key, kind, detail, enforced, host)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![user, at, key, kind, detail, enforced, host],
        )?;
        Ok(kind != STATE_KIND)
    }

    /// Newest first.
    pub fn events(&self, user: Option<&str>, limit: u32) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user, at, kind, detail, enforced, host FROM event
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
                    enforced: r.get(5)?,
                    host: r.get(6)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Drops per-app stretches and events older than 30 days. Daily totals are kept.
    /// Stores a message for one account; it is handed out until `MESSAGE_TTL_SECS` after `now`.
    pub fn add_message(&mut self, user: &str, text: &str, now: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO message (user, text, created, expires) VALUES (?1, ?2, ?3, ?4)",
            params![user, text, now, now + MESSAGE_TTL_SECS],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The account's unexpired messages nobody has been shown yet, marked as shown on `host`.
    pub fn take_messages(
        &mut self,
        user: &str,
        host: &str,
        now: i64,
    ) -> Result<Vec<protocol::Message>> {
        let tx = self.conn.transaction()?;
        let messages: Vec<protocol::Message> = {
            let mut stmt = tx.prepare(
                "SELECT id, user, text FROM message
                 WHERE user = ?1 AND shown_host IS NULL AND expires > ?2 ORDER BY id",
            )?;
            stmt.query_map(params![user, now], |r| {
                Ok(protocol::Message {
                    id: r.get(0)?,
                    user: r.get(1)?,
                    text: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?
        };
        for m in &messages {
            tx.execute(
                "UPDATE message SET shown_host = ?2, shown_at = ?3 WHERE id = ?1",
                params![m.id, host, now],
            )?;
        }
        tx.commit()?;
        Ok(messages)
    }

    /// Newest first.
    pub fn recent_messages(&self, limit: u32) -> Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user, text, created, expires, shown_host, shown_at FROM message ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |r| {
                Ok(MessageRow {
                    id: r.get(0)?,
                    user: r.get(1)?,
                    text: r.get(2)?,
                    created: r.get(3)?,
                    expires: r.get(4)?,
                    shown_host: r.get(5)?,
                    shown_at: r.get(6)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn prune(&mut self, now: i64) -> Result<()> {
        // A timer's stop lasts until the midnight after it ended, so anything that ended before today is done
        if let Some(today) = Local.timestamp_opt(now, 0).single().map(|t| t.date_naive()) {
            self.conn.execute(
                "DELETE FROM timer WHERE ends < ?1",
                [local_text(today.and_time(chrono::NaiveTime::MIN))],
            )?;
        }
        self.conn
            .execute("DELETE FROM message WHERE created < ?1", [now - KEEP_SECS])?;
        self.conn
            .execute("DELETE FROM app_activity WHERE end < ?1", [now - KEEP_SECS])?;
        self.conn
            .execute("DELETE FROM event WHERE at < ?1", [now - KEEP_SECS])?;
        Ok(())
    }
}

/// Replaces one weekday's rule inside a transaction the caller commits.
fn write_day_rule(
    tx: &rusqlite::Transaction,
    user: &str,
    weekday: u8,
    rule: &DayRule,
) -> Result<()> {
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
    Ok(())
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

    fn stretch(app_id: &str, host: &str, start: i64, end: i64) -> AppStretch {
        AppStretch {
            app_id: app_id.into(),
            name: app_id.to_uppercase(),
            host: host.into(),
            start,
            end,
        }
    }

    fn block(name: &str, start: i64, end: i64, hosts: &[&str]) -> TimelineBlock {
        TimelineBlock {
            name: name.into(),
            start,
            end,
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn timeline_bridges_short_pauses_and_keeps_long_ones() {
        let blocks = timeline_blocks(
            vec![
                stretch("a", "host-a", 1000, 1300),
                // 59 seconds later: the same block
                stretch("a", "host-a", 1359, 1600),
                // 60 seconds later: a new one
                stretch("a", "host-a", 1660, 2000),
            ],
            0,
            86_400,
        );
        assert_eq!(
            blocks,
            [
                block("A", 1000, 1600, &["host-a"]),
                block("A", 1660, 2000, &["host-a"])
            ]
        );
    }

    #[test]
    fn timeline_merges_computers_and_keeps_apps_apart() {
        let blocks = timeline_blocks(
            vec![
                stretch("b", "host-a", 500, 900),
                stretch("a", "host-b", 1200, 1800),
                stretch("a", "host-a", 1000, 1500),
                // Entirely inside the block from the other computer
                stretch("a", "host-b", 1300, 1400),
            ],
            0,
            86_400,
        );
        assert_eq!(
            blocks,
            [
                block("B", 500, 900, &["host-a"]),
                block("A", 1000, 1800, &["host-a", "host-b"])
            ]
        );
    }

    #[test]
    fn timeline_drops_blips_and_cuts_at_the_days_edges() {
        let blocks = timeline_blocks(
            vec![
                // Under a minute, even after bridging its two halves
                stretch("blip", "host-a", 5000, 5020),
                stretch("blip", "host-a", 5030, 5055),
                // Exactly a minute stays
                stretch("short", "host-a", 6000, 6060),
                // Started yesterday, ends tomorrow
                stretch("late", "host-a", -500, 400),
                stretch("night", "host-a", 86_000, 90_000),
                // Only 30 seconds of it are today
                stretch("edge", "host-a", 86_370, 87_000),
                // Not today at all
                stretch("gone", "host-a", 90_000, 95_000),
            ],
            0,
            86_400,
        );
        assert_eq!(
            blocks,
            [
                block("LATE", 0, 400, &["host-a"]),
                block("SHORT", 6000, 6060, &["host-a"]),
                block("NIGHT", 86_000, 86_400, &["host-a"]),
            ]
        );
    }

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
                        overrun: Vec::new(),
                        errors: Vec::new(),
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
                    overrun: Vec::new(),
                    errors: Vec::new(),
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

    use protocol::rules::{CategoryStatus, Computer, DayRule, Decision, Stretch};

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
    fn copy_week_replaces_the_targets_whole_week() {
        let (mut db, path) = temp_db("copy-week");
        let evenings = DayRule {
            restricted: true,
            stretches: vec![Stretch {
                start_min: 375,
                end_min: 1260,
            }],
            budgets: BTreeMap::from([(GAMES, 60)]),
        };
        // kid1: Monday and Saturday set, the rest unrestricted
        db.set_day_rule("kid1", 0, &evenings).unwrap();
        db.set_day_rule(
            "kid1",
            5,
            &DayRule {
                budgets: BTreeMap::from([(GAMES, 120)]),
                ..Default::default()
            },
        )
        .unwrap();
        // kid2 has a Wednesday rule that kid1 doesn't: it must not survive the copy
        db.set_day_rule("kid2", 2, &evenings).unwrap();
        let other = DayRule {
            restricted: true,
            stretches: vec![],
            budgets: BTreeMap::new(),
        };
        db.set_day_rule("kid3", 4, &other).unwrap();

        db.copy_week("kid1", &["kid2".to_string()]).unwrap();
        assert_eq!(
            db.week_rules("kid2").unwrap(),
            db.week_rules("kid1").unwrap()
        );
        assert_eq!(db.day_rule("kid2", 2).unwrap(), DayRule::default());
        // Untouched: the source and anyone not named
        assert_eq!(db.day_rule("kid1", 0).unwrap(), evenings);
        assert_eq!(db.day_rule("kid3", 4).unwrap(), other);
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
        assert!(
            !db.log_decision("kid1", 100, &allowed, false, "host-a")
                .unwrap()
        );
        assert!(
            db.log_decision("kid1", 110, &outside, false, "host-a")
                .unwrap()
        );
        assert!(
            !db.log_decision("kid1", 120, &outside, false, "host-a")
                .unwrap()
        );
        assert!(
            db.log_decision("kid1", 130, &blackout, false, "host-a")
                .unwrap()
        );
        assert!(
            db.log_decision("kid1", 140, &allowed, false, "host-a")
                .unwrap()
        );
        assert!(
            db.log_decision("kid1", 150, &games_gone, false, "host-a")
                .unwrap()
        );
        assert!(
            db.log_decision("kid2", 160, &outside, false, "host-a")
                .unwrap()
        );

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
            false,
            "host-a",
        )
        .unwrap();
        // Midnight: the budget is fresh, but the computer is still outside its hours
        assert!(
            !db.log_decision(
                "kid1",
                110,
                &decision_of(Computer::OutsideSchedule, &[]),
                false,
                "host-a"
            )
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
        db.log_decision(
            "kid1",
            120,
            &decision_of(Computer::OutsideSchedule, &[]),
            false,
            "host-a",
        )
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
            false,
            "host-a",
        )
        .unwrap();
        assert!(
            !db.log_decision(
                "kid1",
                110,
                &decision_of(Computer::Allowed, &[GAMES]),
                false,
                "host-a"
            )
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
            false,
            "host-a",
        )
        .unwrap();
        assert!(
            db.log_decision(
                "kid1",
                110,
                &decision_of(Computer::Allowed, &[]),
                false,
                "host-a"
            )
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
        db.log_decision(
            "kid1",
            100,
            &decision_of(Computer::OutsideSchedule, &[]),
            false,
            "host-a",
        )
        .unwrap();
        assert!(
            db.log_decision(
                "kid1",
                110,
                &decision_of(Computer::Allowed, &[GAMES]),
                false,
                "host-a"
            )
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
        db.log_decision(
            "kid1",
            100,
            &decision_of(Computer::Allowed, &[GAMES]),
            false,
            "host-a",
        )
        .unwrap();
        db.log_decision(
            "kid1",
            110,
            &decision_of(Computer::Allowed, &[GAMES, 3]),
            false,
            "host-a",
        )
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
        db.log_decision(
            "kid1",
            100,
            &decision_of(Computer::Allowed, &[GAMES]),
            false,
            "host-a",
        )
        .unwrap();
        assert!(
            db.log_decision(
                "kid1",
                110,
                &decision_of(Computer::Allowed, &[]),
                false,
                "host-a"
            )
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
        db.log_decision(
            "kid1",
            old,
            &decision_of(Computer::OutsideSchedule, &[]),
            false,
            "host-a",
        )
        .unwrap();
        db.log_decision(
            "kid1",
            now,
            &decision_of(Computer::Allowed, &[]),
            false,
            "host-a",
        )
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

    #[test]
    fn overrun_and_errors_are_clipped_and_limited() {
        let long = "é".repeat(1000);
        let many: Vec<String> = (0..60).map(|i| format!("entry-{i:02}")).collect();
        let mut report = sample_report("host-a", 1, start_of_test() + 15, UserState::Active, &[]);
        report.samples[0].users[0].overrun = vec![long.clone()];
        report.samples[0].users[0].errors = many;
        let (mut db, path) = temp_db("overrun-clip");
        let recorded = db.record(&report).unwrap();
        let user = &recorded.samples[0].users[0];
        assert_eq!(user.overrun, ["é".repeat(200)]);
        assert_eq!(user.errors.len(), 50);
        assert_eq!(user.errors[49], "entry-49");
        std::fs::remove_file(&path).unwrap();
    }

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
        db.record(&sample_report(
            "host-a",
            1,
            t0 + 15,
            UserState::Active,
            &[("steam:1", "Minecraft"), ("kitty", "kitty")],
        ))
        .unwrap();
        db.set_app_category("kitty", Some(IGNORED)).unwrap();
        let rule = DayRule {
            restricted: true,
            stretches: vec![Stretch {
                start_min: 375,
                end_min: 1260,
            }],
            budgets: BTreeMap::from([(GAMES, 60)]),
        };
        db.set_day_rule("kid1", weekday_today(), &rule).unwrap();
        let now = noon_today();
        let hour = chrono::Duration::hours(1);
        db.add_blackout(Some("kid1"), now + hour, now + hour * 2, "dinner")
            .unwrap();
        db.add_blackout(Some("kid2"), now + hour, now + hour * 2, "not mine")
            .unwrap();
        db.set_enforce("kid1", true).unwrap();

        let snap = db.snapshot("kid1", now).unwrap();
        assert!(snap.enforce);
        assert_eq!(snap.for_day, now.date());
        assert_eq!(snap.week[weekday_today() as usize], rule);
        assert_eq!(snap.week.len(), 7);
        assert_eq!(snap.used_secs[&GAMES], 15);
        assert_eq!(
            snap.blackouts
                .iter()
                .map(|b| b.note.as_str())
                .collect::<Vec<_>>(),
            ["dinner"]
        );
        assert_eq!(snap.games, ["steam:1"]);
        assert_eq!(snap.ignored, ["kitty"]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn timers_start_replace_cancel_and_expire() {
        use protocol::rules::{Timer, TimerMode};
        let (mut db, path) = temp_db("timers");
        let now = noon_today();
        assert_eq!(db.timer("kid1", now).unwrap(), None);
        let t = db.start_timer("kid1", 30, TimerMode::Lock, now).unwrap();
        assert_eq!(
            t,
            Timer {
                ends: now + chrono::Duration::minutes(30),
                mode: TimerMode::Lock
            }
        );
        assert_eq!(db.timer("kid1", now).unwrap(), Some(t.clone()));
        // A new timer replaces the old one
        let t2 = db.start_timer("kid1", 15, TimerMode::Games, now).unwrap();
        assert_eq!(db.timer("kid1", now).unwrap(), Some(t2));
        // The snapshot carries it, and the server's decision follows it
        assert_eq!(
            db.snapshot("kid1", now).unwrap().timer,
            db.timer("kid1", now).unwrap()
        );
        db.start_timer("kid1", 1, TimerMode::Lock, now).unwrap();
        let later = now + chrono::Duration::minutes(5);
        assert!(matches!(
            db.decision("kid1", later).unwrap().computer,
            Computer::TimerEnded { .. }
        ));
        // Past the midnight after it ended, it's gone (and pruned)
        let tomorrow = now + chrono::Duration::days(1);
        assert_eq!(db.timer("kid1", tomorrow).unwrap(), None);
        db.start_timer("kid2", 10, TimerMode::Lock, now - chrono::Duration::days(2))
            .unwrap();
        db.prune(Local::now().timestamp()).unwrap();
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM timer", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "kid2's two-day-old timer is pruned, kid1's stays");
        assert!(db.cancel_timer("kid1").unwrap());
        assert!(!db.cancel_timer("kid1").unwrap());
        assert_eq!(db.timer("kid1", later).unwrap(), None);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_games_timer_ending_is_logged_as_closed_timer_ended() {
        use protocol::rules::TimerMode;
        let (mut db, path) = temp_db("games-timer-event");
        let start = Local::now().naive_local() - chrono::Duration::minutes(10);
        db.start_timer("kid1", 5, TimerMode::Games, start).unwrap();
        let d = decision_of(Computer::Allowed, &[GAMES]);
        db.log_decision("kid1", Local::now().timestamp(), &d, true, "host-a")
            .unwrap();
        let e = &db.events(Some("kid1"), 10).unwrap()[0];
        assert_eq!(
            (e.kind.as_str(), e.detail.as_str()),
            ("closed", "timer ended")
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_new_timer_replaces_an_ended_one_and_lifts_its_stop() {
        use protocol::rules::TimerMode;
        let (mut db, path) = temp_db("timer-replace-ended");
        let now = noon_today();
        db.start_timer(
            "kid1",
            1,
            TimerMode::Lock,
            now - chrono::Duration::minutes(10),
        )
        .unwrap();
        assert!(matches!(
            db.decision("kid1", now).unwrap().computer,
            Computer::TimerEnded { .. }
        ));
        db.start_timer("kid1", 30, TimerMode::Lock, now).unwrap();
        assert_eq!(
            db.decision("kid1", now).unwrap().computer,
            Computer::Allowed
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_timer_ending_is_logged_as_locked_timer_ended() {
        let (mut db, path) = temp_db("timer-event");
        let d = decision_of(Computer::TimerEnded { at: noon_today() }, &[]);
        db.log_decision("kid1", 100, &d, true, "host-a").unwrap();
        let e = &db.events(Some("kid1"), 10).unwrap()[0];
        assert_eq!(
            (e.kind.as_str(), e.detail.as_str()),
            ("locked", "timer ended")
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn messages_are_handed_out_once_and_expire() {
        let (mut db, path) = temp_db("messages");
        let now = 1_000_000;
        let dinner = db.add_message("kid1", "dinner in 5 minutes", now).unwrap();
        let old = db
            .add_message("kid1", "too late", now - MESSAGE_TTL_SECS - 1)
            .unwrap();
        db.add_message("kid2", "not yours", now).unwrap();

        let handed = db.take_messages("kid1", "host-a", now + 10).unwrap();
        assert_eq!(
            handed,
            [protocol::Message {
                id: dinner,
                user: "kid1".into(),
                text: "dinner in 5 minutes".into()
            }]
        );
        // Only once, and not to a second PC
        assert!(
            db.take_messages("kid1", "host-b", now + 20)
                .unwrap()
                .is_empty()
        );

        let recent = db.recent_messages(10).unwrap();
        assert_eq!(recent.len(), 3);
        let shown = recent.iter().find(|m| m.id == dinner).unwrap();
        assert_eq!(
            (shown.shown_host.as_deref(), shown.shown_at),
            (Some("host-a"), Some(now + 10))
        );
        let expired = recent.iter().find(|m| m.id == old).unwrap();
        assert_eq!(expired.shown_host, None);
        assert!(expired.expires < now);
        // Newest first
        assert!(recent[0].created >= recent[2].created);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn events_record_enforcement_and_host() {
        let (mut db, path) = temp_db("event-fields");
        db.log_decision(
            "kid1",
            100,
            &decision_of(Computer::OutsideSchedule, &[]),
            true,
            "host-a",
        )
        .unwrap();
        let e = &db.events(Some("kid1"), 10).unwrap()[0];
        assert!(e.enforced);
        assert_eq!(e.host, "host-a");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn event_columns_are_added_to_an_existing_database() {
        let path =
            std::env::temp_dir().join(format!("kidtime-test-oldevents-{}.db", std::process::id()));
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
}
