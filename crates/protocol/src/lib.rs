//! Wire types shared by the kidtime agent and server.

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
