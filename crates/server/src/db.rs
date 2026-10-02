//! SQLite storage.
//!
//! - `usage`: seconds per day/user/host/app, for the per-computer and per-app breakdowns.
//! - `activity`: when each user was active, as time intervals. A user's total is
//!   the union of their intervals, so time spent on two machines at once (e.g.
//!   streaming from one to the other) is only counted once.
//! - `agents`: the last sequence number recorded from each agent.

use std::path::Path;

use anyhow::Result;
use chrono::{Days, Local, NaiveDate, TimeZone};
use protocol::Report;
use rusqlite::{Connection, OptionalExtension, params};

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
        let conn = Connection::open(path)?;
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
             );",
        )?;
        Ok(Self { conn })
    }

    /// Adds the report's usage, skipping samples already recorded.
    pub fn record(&mut self, report: &Report) -> Result<()> {
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
            for sample in report.samples.iter().filter(|s| s.seq > last_seq) {
                max_seq = max_seq.max(sample.seq);
                let Some(at) = Local.timestamp_opt(sample.at, 0).single() else {
                    continue;
                };
                let day = at.date_naive().to_string();
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
        Ok(())
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

    /// Seconds per app for the user on `day`, across hosts, largest first.
    pub fn app_totals(&self, user: &str, day: NaiveDate) -> Result<Vec<AppUsage>> {
        let mut stmt = self.conn.prepare(
            "SELECT MAX(app_name), SUM(secs) AS total FROM usage WHERE user = ?1 AND app_id != ?2 AND day = ?3
             GROUP BY app_id ORDER BY total DESC",
        )?;
        let rows = stmt
            .query_map(params![user, TOTAL, day.to_string()], |r| {
                Ok(AppUsage {
                    name: r.get(0)?,
                    secs: r.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}

/// Unix time of local midnight at the start of `day`.
fn midnight(day: NaiveDate) -> i64 {
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight is valid"))
        .earliest()
        .map_or(0, |t| t.timestamp())
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
    use protocol::{App, Sample, UserSample, UserState};

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
}
