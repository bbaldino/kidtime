//! Wire types shared by the kidtime agent and server.

pub mod rules;

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

/// A batch of samples sent from one agent to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub host: String,
    /// Random per-process id, so the server can tell an agent restart
    /// (sequence numbers start over) from a retried batch.
    pub agent_id: String,
    /// How often the agent samples, so the server knows when a host has gone quiet.
    pub interval_secs: u32,
    pub samples: Vec<Sample>,
}

/// What the agent saw at one tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// Increases by one per tick; the server drops anything it has already seen.
    pub seq: u64,
    /// Wall-clock time of the tick, unix seconds.
    pub at: i64,
    /// Time covered by this sample, measured on the monotonic clock
    /// (so time spent suspended is never counted).
    pub elapsed_secs: u32,
    pub users: Vec<UserSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSample {
    pub user: String,
    pub state: UserState,
    /// Apps attributed to the user at this tick: everything running in their
    /// desktop session, or just the streamed games when `state` is `Streaming`.
    pub apps: Vec<App>,
    /// Uncategorised apps still running after the games budget ran out (they count, but aren't closed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrun: Vec<String>,
    /// Enforcement actions that failed since the last report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// What the server answers a report with: everything an agent needs to enforce the rules for the accounts
/// in that report, including while the server is unreachable afterwards.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportResponse {
    /// The server's local wall-clock time, so an agent can tell if its own clock is off.
    pub server_time: NaiveDateTime,
    pub accounts: Vec<AccountSnapshot>,
    /// Messages from the parent to show now, each to one account at this PC. Not persisted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Message>,
}

/// A message typed on the dashboard, handed to the PC where its account is at the computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub user: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub user: String,
    /// Off: the agent leaves the account alone and undoes anything it did.
    pub enforce: bool,
    /// The day `used_secs` belongs to.
    pub for_day: NaiveDate,
    /// The account's rule for each weekday: exactly 7, Monday first (index = `num_days_from_monday`). The
    /// whole week travels so an agent that is offline over midnight still knows the next day's rule.
    pub week: Vec<rules::DayRule>,
    /// Blackouts that apply to this account and haven't ended.
    pub blackouts: Vec<rules::BlackoutSpan>,
    /// Today's category time across all PCs, merged.
    pub used_secs: BTreeMap<rules::CategoryId, i64>,
    /// App ids in the Games category: the only apps an agent closes.
    pub games: Vec<String>,
    /// App ids in the Ignored category: never counted.
    pub ignored: Vec<String>,
}

impl UserState {
    /// Whether time in this state counts as usage.
    pub fn counts(self) -> bool {
        matches!(self, UserState::Active | UserState::Streaming)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserState {
    /// Graphical session in the foreground, not idle, not locked.
    Active,
    /// Playing a game streamed from this host (e.g. Sunshine), with no local session in use.
    Streaming,
    /// A streaming game is running, but no client is connected to watch it.
    StreamIdle,
    /// Session in the foreground but the user hasn't touched it for a while.
    Idle,
    /// Session in the foreground behind the lock screen.
    Locked,
    /// Logged in, but another session is in the foreground.
    Background,
    /// No graphical session.
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct App {
    /// Stable id: a desktop-file id (`org.mozilla.firefox`) or `steam:<appid>`.
    pub id: String,
    /// Human-readable name for the dashboard.
    pub name: String,
}

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
                week: vec![rules::DayRule::default(); 7],
                blackouts: vec![rules::BlackoutSpan {
                    start: day.and_hms_opt(17, 0, 0).unwrap(),
                    end: day.and_hms_opt(19, 0, 0).unwrap(),
                    note: "dinner".into(),
                }],
                used_secs: BTreeMap::from([(rules::GAMES, 900)]),
                games: vec!["steam:1".into()],
                ignored: vec!["kitty".into()],
            }],
            messages: vec![Message {
                id: 3,
                user: "kid1".into(),
                text: "dinner in 5 minutes".into(),
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<ReportResponse>(&json).unwrap(),
            response
        );

        // A response from a server that predates messages still parses
        let without: ReportResponse =
            serde_json::from_str(r#"{"server_time":"2026-10-05T12:00:00","accounts":[]}"#).unwrap();
        assert!(without.messages.is_empty());

        // A sample from an agent that predates overrun/errors
        let old = r#"{"user":"kid1","state":"active","apps":[]}"#;
        let sample: UserSample = serde_json::from_str(old).unwrap();
        assert!(sample.overrun.is_empty() && sample.errors.is_empty());
    }
}
