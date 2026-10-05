//! Turns the server's rules into actions on this PC. Pure logic: every effect goes through `Actions`, so the
//! whole thing is tested with a fake.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use protocol::rules::{self, Computer, DayRule, Decision, GAMES};
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

/// An account's password login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    Enabled,
    Disabled,
    /// No password at all: disabling would make the account impossible to unlock again, so it never is.
    NoPassword,
}

pub trait Actions {
    fn lock_session(&mut self, session_id: &str) -> anyhow::Result<()>;
    /// The account's password login right now.
    fn login(&mut self, user: &str) -> anyhow::Result<Login>;
    fn disable_login(&mut self, user: &str) -> anyhow::Result<()>;
    fn enable_login(&mut self, user: &str) -> anyhow::Result<()>;
    /// On the desktop and, on a streaming host, in the stream. Failures are logged by the implementation.
    fn notify(&mut self, user: &str, text: &str);
    fn close(&mut self, user: &str, app: &RunningApp) -> anyhow::Result<()>;
    fn block_stream(&mut self, user: &str) -> anyhow::Result<()>;
    fn unblock_stream(&mut self, user: &str) -> anyhow::Result<()>;
    /// Empty turns the banner off.
    fn set_banner(&mut self, lines: &[String]) -> anyhow::Result<()>;
    /// Writes the state to disk now. Called before disabling a login, so the record of it can't be lost.
    fn persist(&mut self, state: &Persisted) -> anyhow::Result<()>;
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
    /// The day the "games used up" notice was shown.
    games_closed_on: Option<NaiveDate>,
    /// Login was already disabled by someone else when we blocked: leave it alone.
    external_lock: bool,
    /// Sessions observed locked (LockedHint true) while blocked: one of these observed unlocked was unlocked
    /// from inside. A lock request doesn't count: GNOME sets LockedHint a moment later, or never.
    locked_sessions: BTreeSet<String>,
    /// Unlocks while blocked, and when the last was reported.
    unlocks: u64,
    unlock_reported_at: Option<NaiveDateTime>,
    overrun: Vec<String>,
    errors: Vec<String>,
}

pub struct Enforcer {
    pub persisted: Persisted,
    live: BTreeMap<String, Live>,
    banner: Option<Vec<String>>,
    /// The last error setting the banner: a repeat is logged quietly, since it is retried every tick.
    banner_error: Option<String>,
    /// Problems reported once per process, keyed by kind and account.
    reported: BTreeSet<String>,
    dirty: bool,
}

impl Enforcer {
    pub fn new(persisted: Persisted) -> Self {
        // The banner may be up from before a restart: always set it on the first tick
        Self {
            persisted,
            live: BTreeMap::new(),
            banner: None,
            banner_error: None,
            reported: BTreeSet::new(),
            dirty: false,
        }
    }

    pub fn apply_response(&mut self, response: &ReportResponse, local_now: NaiveDateTime) {
        let offset = (response.server_time - local_now).num_seconds();
        let offset = if offset.abs() > MAX_CLOCK_SKEW_SECS {
            offset
        } else {
            0
        };
        let stored = self.persisted.clock_offset_secs;
        if offset != stored {
            if offset != 0 && (offset - stored).abs() > 60 {
                tracing::warn!(
                    "this PC's clock is {offset}s off the server's; using the server's time"
                );
            }
            self.persisted.clock_offset_secs = offset;
            self.dirty = true;
        }
        // The server sends every account in the report: one it left out has no rules any more
        let before = self.persisted.snapshots.len();
        self.persisted
            .snapshots
            .retain(|user, _| response.accounts.iter().any(|a| &a.user == user));
        if self.persisted.snapshots.len() != before {
            self.dirty = true;
        }
        for snap in &response.accounts {
            // The server's figure now includes everything this PC reported
            let live = self.live.entry(snap.user.clone()).or_default();
            live.local_secs = 0;
            if self.persisted.snapshots.get(&snap.user) != Some(snap) {
                self.persisted
                    .snapshots
                    .insert(snap.user.clone(), snap.clone());
                self.dirty = true;
            }
        }
    }

    /// The server answered without rules (an old server, after a rollback): enforce nothing. The next tick
    /// releases everything.
    pub fn clear_snapshots(&mut self) {
        if !self.persisted.snapshots.is_empty() {
            self.persisted.snapshots.clear();
            self.dirty = true;
        }
    }

    /// Adds a sample's time to the account's local count, the way the server counts games time.
    pub fn count(
        &mut self,
        user: &str,
        state: UserState,
        apps: &[App],
        secs: i64,
        today: NaiveDate,
    ) {
        let Some(snap) = self.persisted.snapshots.get(user) else {
            return;
        };
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

    pub fn tick(
        &mut self,
        local_now: NaiveDateTime,
        observations: &[Observation],
        act: &mut dyn Actions,
    ) {
        let now = local_now + chrono::Duration::seconds(self.persisted.clock_offset_secs);
        self.forget_untracked(observations, act);
        let mut banner = Vec::new();
        for o in observations {
            let snap = match self.persisted.snapshots.get(&o.user) {
                Some(s) if s.enforce => s.clone(),
                _ => {
                    self.let_go(&o.user, act);
                    if let Some(live) = self.live.get_mut(&o.user) {
                        live.overrun.clear();
                    }
                    continue;
                }
            };
            let decision = self.decision(&snap, now);
            let blocked = decision.computer != Computer::Allowed;
            let present = !o.sessions.is_empty() || !o.running.is_empty();
            if blocked {
                let reason = blocked_reason(&snap, &decision, now);
                banner.push(format!(
                    "{}: computer time is over{}",
                    o.user,
                    joined(&reason)
                ));
                self.block(o, &reason, now, act);
            } else {
                self.let_go(&o.user, act);
                self.check_login_is_ours(&o.user, act);
                if present {
                    self.warn_before_lock(o, &snap, &decision, now, act);
                }
            }
            self.games(o, &snap, &decision, blocked, present, now, act);
        }
        if self.banner.as_ref() != Some(&banner) {
            match act.set_banner(&banner) {
                Ok(()) => {
                    self.banner = Some(banner);
                    self.banner_error = None;
                }
                Err(e) => {
                    let text = format!("{e:#}");
                    if self.banner_error.as_ref() == Some(&text) {
                        tracing::debug!("login screen banner: {text}");
                    } else {
                        tracing::warn!("login screen banner: {text}");
                        self.banner_error = Some(text);
                    }
                }
            }
        }
    }

    /// Accounts this agent is blocking right now.
    pub fn blocked_users(&self) -> Vec<String> {
        self.live
            .iter()
            .filter(|(_, l)| l.blocked)
            .map(|(u, _)| u.clone())
            .collect()
    }

    /// The fast re-lock check for a blocked account: locks any of `sessions` that isn't locked. The kid can
    /// unlock their own session from inside it, so this runs every second, between enforcement ticks.
    pub fn relock(
        &mut self,
        local_now: NaiveDateTime,
        user: &str,
        sessions: &[SessionInfo],
        act: &mut dyn Actions,
    ) {
        if !self.live.get(user).is_some_and(|l| l.blocked) {
            return;
        }
        let now = local_now + chrono::Duration::seconds(self.persisted.clock_offset_secs);
        self.lock_sessions(now, user, sessions, act);
    }

    /// An account no longer tracked (taken out of the config) gets everything undone and its rules dropped.
    fn forget_untracked(&mut self, observations: &[Observation], act: &mut dyn Actions) {
        let tracked: BTreeSet<&str> = observations.iter().map(|o| o.user.as_str()).collect();
        let untracked: BTreeSet<String> = self
            .persisted
            .login_disabled
            .iter()
            .chain(&self.persisted.streams_blocked)
            .chain(self.persisted.snapshots.keys())
            .filter(|u| !tracked.contains(u.as_str()))
            .cloned()
            .collect();
        for user in untracked {
            self.let_go(&user, act);
            if self.persisted.snapshots.remove(&user).is_some() {
                self.dirty = true;
            }
        }
    }

    /// Whether everything was undone, the banner included. What could not be undone stays recorded.
    pub fn release_all(&mut self, act: &mut dyn Actions) -> bool {
        let mut ok = true;
        for user in self.persisted.login_disabled.clone() {
            match act.enable_login(&user) {
                Ok(()) => {
                    self.persisted.login_disabled.remove(&user);
                }
                Err(e) => {
                    ok = false;
                    tracing::error!("re-enabling login for {user}: {e:#}");
                }
            }
        }
        for user in self.persisted.streams_blocked.clone() {
            match act.unblock_stream(&user) {
                Ok(()) => {
                    self.persisted.streams_blocked.remove(&user);
                }
                Err(e) => {
                    ok = false;
                    tracing::error!("unblocking {user}'s stream: {e:#}");
                }
            }
        }
        if let Err(e) = act.set_banner(&[]) {
            ok = false;
            tracing::error!("clearing the login screen banner: {e:#}");
        }
        self.dirty = true;
        ok
    }

    pub fn overrun(&self, user: &str) -> Vec<String> {
        self.live
            .get(user)
            .map(|l| l.overrun.clone())
            .unwrap_or_default()
    }

    pub fn take_errors(&mut self, user: &str) -> Vec<String> {
        self.live
            .get_mut(user)
            .map(|l| std::mem::take(&mut l.errors))
            .unwrap_or_default()
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The decision for a snapshot at `now`, with this PC's uncounted time added. The rule comes from the
    /// snapshot's week, so it holds on any day; usage counts only on the snapshot's own day.
    fn decision(&self, snap: &AccountSnapshot, now: NaiveDateTime) -> Decision {
        let today = now.date();
        let day = week_rule(snap, today);
        let mut used = if snap.for_day == today {
            snap.used_secs.clone()
        } else {
            BTreeMap::new()
        };
        if let Some(live) = self
            .live
            .get(&snap.user)
            .filter(|l| l.local_day == Some(today))
        {
            *used.entry(GAMES).or_default() += live.local_secs;
        }
        rules::decide_with_timer(&day, &snap.blackouts, &used, snap.timer.as_ref(), now)
    }

    fn block(&mut self, o: &Observation, reason: &str, now: NaiveDateTime, act: &mut dyn Actions) {
        let user = &o.user;
        let first = !self.live.get(user).is_some_and(|l| l.blocked);
        if first {
            let live = self.live.entry(user.clone()).or_default();
            live.blocked = true;
            live.external_lock = false;
            if !self.persisted.login_disabled.contains(user) {
                match act.login(user) {
                    Ok(Login::Disabled) => {
                        self.live.get_mut(user).unwrap().external_lock = true;
                        self.report_once(
                            user,
                            "already-locked",
                            format!(
                                "{user}'s login was already disabled when kidtime blocked it; kidtime won't re-enable it"
                            ),
                        );
                    }
                    Ok(Login::NoPassword) => {
                        self.live.get_mut(user).unwrap().external_lock = true;
                        self.report_once(
                            user,
                            "no-password",
                            "can't disable login: no password; only the screen lock applies".into(),
                        );
                    }
                    Ok(Login::Enabled) => self.disable(user, act),
                    Err(e) => self.error(user, format!("checking login failed: {e:#}")),
                }
            }
        } else if !self.live[user].external_lock {
            // A failed disable (or check) is retried every tick
            if let Ok(Login::Enabled) = act.login(user) {
                self.disable(user, act);
            }
        }
        self.lock_sessions(now, user, &o.sessions, act);
        // After the locks, so nothing can delay them
        if first {
            act.notify(user, &format!("Computer time is over{}", joined(reason)));
        }
        // Every blocked tick: the nftables table does not survive a reboot, and the rules may have been
        // removed or the kid's port changed. block_stream checks the rules and repairs only what is wrong.
        if o.streaming_host {
            match act.block_stream(user) {
                Ok(()) => {
                    if self.persisted.streams_blocked.insert(user.clone()) {
                        self.dirty = true;
                    }
                }
                Err(e) => self.error(user, format!("cutting the stream failed: {e:#}")),
            }
        }
    }

    /// Locks every unlocked session. A session observed locked before and observed unlocked now was unlocked
    /// from inside (logind lets the owner do that): reported, at most once a minute per account.
    fn lock_sessions(
        &mut self,
        now: NaiveDateTime,
        user: &str,
        sessions: &[SessionInfo],
        act: &mut dyn Actions,
    ) {
        let live = self.live.entry(user.to_string()).or_default();
        live.locked_sessions
            .retain(|id| sessions.iter().any(|s| &s.id == id));
        let mut unlocked = false;
        let mut failures = Vec::new();
        for s in sessions {
            if s.locked {
                live.locked_sessions.insert(s.id.clone());
                continue;
            }
            unlocked |= live.locked_sessions.remove(&s.id);
            if let Err(e) = act.lock_session(&s.id) {
                failures.push(format!("lock failed: {e:#}"));
            }
        }
        let mut report = false;
        if unlocked {
            live.unlocks += 1;
            tracing::warn!(
                "{user} unlocked a session while blocked ({} times in this process)",
                live.unlocks
            );
            if live
                .unlock_reported_at
                .is_none_or(|t| (now - t).num_seconds() >= 60)
            {
                live.unlock_reported_at = Some(now);
                report = true;
            }
        }
        for f in failures {
            self.error(user, f);
        }
        if report {
            self.error(user, format!("{user} unlocked the session while blocked"));
        }
    }

    /// Records the account as disabled by us, and writes that to disk, *before* disabling it: a disable that took
    /// effect but reported failure is still undone later (re-enabling an account that was enabled is harmless),
    /// and a crash right after the disable can't lose the record. If the record can't be saved, the login stays
    /// enabled (the session lock still applies) and the disable is retried next tick.
    fn disable(&mut self, user: &str, act: &mut dyn Actions) {
        // Already recorded (a retry): the record went to disk when it was added
        if self.persisted.login_disabled.insert(user.to_string()) {
            if let Err(e) = act.persist(&self.persisted) {
                self.persisted.login_disabled.remove(user);
                self.error(
                    user,
                    format!("couldn't save state; not disabling login: {e:#}"),
                );
                return;
            }
            self.dirty = true;
        }
        if let Err(e) = act.disable_login(user) {
            self.error(user, format!("disabling login failed: {e:#}"));
        }
    }

    /// Undoes anything this agent did to the account.
    fn let_go(&mut self, user: &str, act: &mut dyn Actions) {
        if let Some(live) = self.live.get_mut(user) {
            live.blocked = false;
            live.external_lock = false;
            live.locked_sessions.clear();
        }
        if self.persisted.login_disabled.contains(user) {
            match act.enable_login(user) {
                Ok(()) => {
                    self.persisted.login_disabled.remove(user);
                    self.dirty = true;
                }
                Err(e) => self.error(user, format!("re-enabling login failed: {e:#}")),
            }
        }
        if self.persisted.streams_blocked.contains(user) {
            match act.unblock_stream(user) {
                Ok(()) => {
                    self.persisted.streams_blocked.remove(user);
                    self.dirty = true;
                }
                Err(e) => self.error(user, format!("restoring the stream failed: {e:#}")),
            }
        }
    }

    /// For an allowed account: a disabled login kidtime didn't record is someone else's doing (or a lost record).
    /// It is never undone automatically, only reported.
    fn check_login_is_ours(&mut self, user: &str, act: &mut dyn Actions) {
        if self.persisted.login_disabled.contains(user)
            || self.reported.contains(&format!("unrecorded-lock:{user}"))
        {
            return;
        }
        if let Ok(Login::Disabled) = act.login(user) {
            self.report_once(
                user,
                "unrecorded-lock",
                format!(
                    "login for {user} is disabled but kidtime didn't do it; if it should be enabled run `sudo usermod -U {user}`"
                ),
            );
        }
    }

    /// An error for the dashboard, at most once per process for this kind and account.
    fn report_once(&mut self, user: &str, kind: &str, message: String) {
        if self.reported.insert(format!("{kind}:{user}")) {
            self.error(user, message);
        }
    }

    fn warn_before_lock(
        &mut self,
        o: &Observation,
        snap: &AccountSnapshot,
        d: &Decision,
        now: NaiveDateTime,
        act: &mut dyn Actions,
    ) {
        // Only warn if the clock alone will block them at the next change
        if self.decision(snap, d.next_change).computer == Computer::Allowed {
            return;
        }
        let left = (d.next_change - now).num_seconds();
        let Some(threshold) = nearest_threshold(left) else {
            return;
        };
        let key = format!("lock:{}:{threshold}", d.next_change);
        let when = clock(d.next_change, now);
        let text = if threshold == 60 {
            format!("1 minute left: save your game. The computer locks at {when}")
        } else {
            format!(
                "{} minutes left today: the computer locks at {when}",
                threshold / 60
            )
        };
        self.warn_once(&o.user, key, &text, act);
    }

    #[allow(clippy::too_many_arguments)]
    fn games(
        &mut self,
        o: &Observation,
        snap: &AccountSnapshot,
        d: &Decision,
        blocked: bool,
        present: bool,
        now: NaiveDateTime,
        act: &mut dyn Actions,
    ) {
        let user = o.user.clone();
        let Some(games) = d.categories.iter().find(|c| c.category == GAMES) else {
            self.live.entry(user).or_default().overrun.clear();
            return;
        };
        let counted_running = o.running.iter().any(|a| !snap.ignored.contains(&a.id));
        if !games.used_up {
            self.live.entry(user.clone()).or_default().overrun.clear();
            self.live.entry(user.clone()).or_default().games_closed_on = None;
            if present
                && counted_running
                && !blocked
                && let Some(threshold) = nearest_threshold(games.left_secs)
            {
                // The budget's size is part of the key, so raising it warns again
                let budget = week_rule(snap, now.date())
                    .budgets
                    .get(&GAMES)
                    .copied()
                    .unwrap_or(0);
                let key = format!("games:{}:{budget}:{threshold}", now.date());
                let text = if threshold == 60 {
                    "1 minute of games left today: save your game".to_string()
                } else {
                    format!("{} minutes of games left today", threshold / 60)
                };
                self.warn_once(&user, key, &text, act);
            }
            return;
        }
        // Used up. Behind a lock, games keep running.
        if blocked {
            return;
        }
        let live = self.live.entry(user.clone()).or_default();
        if present && live.games_closed_on != Some(now.date()) {
            live.games_closed_on = Some(now.date());
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
        self.live
            .entry(user.to_string())
            .or_default()
            .errors
            .push(message);
    }
}

/// The smallest warning threshold at or above `left`, if any: a kid who starts with 4 minutes left gets the
/// 5-minute warning only.
fn nearest_threshold(left: i64) -> Option<i64> {
    WARN_AT_SECS
        .iter()
        .rev()
        .copied()
        .find(|&t| left > 0 && left <= t)
}

/// "until 6:15am", "until 7:00pm (dinner)", "until Tue 6:15am", or "for now" when nothing in the week allows it.
/// A reason after "computer time is over": "until 6:15am" takes a space, ": timer ended …" doesn't.
fn joined(reason: &str) -> String {
    if reason.starts_with(':') {
        reason.to_string()
    } else {
        format!(" {reason}")
    }
}

fn blocked_reason(snap: &AccountSnapshot, d: &Decision, now: NaiveDateTime) -> String {
    match &d.computer {
        Computer::Blackout { until, note } if note.is_empty() => {
            format!("until {}", clock(*until, now))
        }
        Computer::Blackout { until, note } => format!("until {} ({note})", clock(*until, now)),
        Computer::TimerEnded { at } => format!(": timer ended at {}", clock(*at, now)),
        _ => match next_allowed(snap, now) {
            Some(t) => format!("until {}", clock(t, now)),
            None => "for now".to_string(),
        },
    }
}

/// The snapshot's rule for any date; a short week (never sent by the server) means no rule.
fn week_rule(snap: &AccountSnapshot, date: NaiveDate) -> DayRule {
    snap.week
        .get(date.weekday().num_days_from_monday() as usize)
        .cloned()
        .unwrap_or_default()
}

/// The first moment after `now`, within a week, that the schedule allows the computer.
fn next_allowed(snap: &AccountSnapshot, now: NaiveDateTime) -> Option<NaiveDateTime> {
    for days in 0..=7u64 {
        let date = now.date() + chrono::Days::new(days);
        let midnight = date.and_time(chrono::NaiveTime::MIN);
        let rule = week_rule(snap, date);
        if !rule.restricted {
            if midnight > now {
                return Some(midnight);
            }
            continue;
        }
        let start = rule
            .stretches
            .iter()
            .map(|s| midnight + chrono::Duration::minutes(i64::from(s.start_min)))
            .filter(|&t| t > now)
            .min();
        if start.is_some() {
            return start;
        }
    }
    None
}

/// "9:00pm", "12:15am", and "midnight" for 00:00.
fn minute_clock(minutes: u16) -> String {
    if minutes == 0 {
        return "midnight".into();
    }
    let (h, m) = (u32::from(minutes) / 60, u32::from(minutes) % 60);
    format!(
        "{}:{m:02}{}",
        if h % 12 == 0 { 12 } else { h % 12 },
        if h < 12 { "am" } else { "pm" }
    )
}

/// "9:00pm" today, "Sat 9:00am" another day, "midnight" for the coming one (the start of tomorrow).
/// A midnight further off reads "Thu 12:00am": "Thu midnight" could mean either end of Thursday.
fn clock(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let minutes = (t.hour() * 60 + t.minute()) as u16;
    let tomorrow = now.date().succ_opt();
    if minutes == 0 && t.date() != now.date() && Some(t.date()) != tomorrow {
        return format!("{} 12:00am", t.format("%a"));
    }
    let time = minute_clock(minutes);
    if t.date() == now.date() || minutes == 0 {
        time
    } else {
        format!("{} {time}", t.format("%a"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use protocol::rules::{BlackoutSpan, DayRule, GAMES, Stretch};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        already_locked: BTreeSet<String>,
        no_password: BTreeSet<String>,
        fail_lock: bool,
        fail_disable: bool,
        fail_enable: bool,
        fail_banner: bool,
        fail_persist: bool,
        banner: Vec<String>,
        /// The last state persisted.
        persisted: Option<Persisted>,
    }

    impl Actions for Fake {
        fn lock_session(&mut self, id: &str) -> anyhow::Result<()> {
            self.calls.push(format!("lock {id}"));
            if self.fail_lock {
                anyhow::bail!("logind said no")
            } else {
                Ok(())
            }
        }
        fn login(&mut self, user: &str) -> anyhow::Result<Login> {
            Ok(if self.no_password.contains(user) {
                Login::NoPassword
            } else if self.already_locked.contains(user) {
                Login::Disabled
            } else {
                Login::Enabled
            })
        }
        fn disable_login(&mut self, user: &str) -> anyhow::Result<()> {
            self.calls.push(format!("disable {user}"));
            self.already_locked.insert(user.to_string());
            if self.fail_disable {
                anyhow::bail!("usermod said no");
            }
            Ok(())
        }
        fn enable_login(&mut self, user: &str) -> anyhow::Result<()> {
            self.calls.push(format!("enable {user}"));
            if self.fail_enable {
                anyhow::bail!("usermod said no");
            }
            self.already_locked.remove(user);
            Ok(())
        }
        fn notify(&mut self, user: &str, text: &str) {
            self.calls.push(format!("notify {user}: {text}"));
        }
        fn close(&mut self, user: &str, app: &RunningApp) -> anyhow::Result<()> {
            self.calls.push(format!("close {user} {}", app.id));
            Ok(())
        }
        fn block_stream(&mut self, user: &str) -> anyhow::Result<()> {
            self.calls.push(format!("block {user}"));
            Ok(())
        }
        fn unblock_stream(&mut self, user: &str) -> anyhow::Result<()> {
            self.calls.push(format!("unblock {user}"));
            Ok(())
        }
        fn set_banner(&mut self, lines: &[String]) -> anyhow::Result<()> {
            self.calls.push(format!("banner {}", lines.join(" | ")));
            if self.fail_banner {
                anyhow::bail!("dconf said no");
            }
            self.banner = lines.to_vec();
            Ok(())
        }
        fn persist(&mut self, state: &Persisted) -> anyhow::Result<()> {
            self.calls.push("persist".into());
            if self.fail_persist {
                anyhow::bail!("disk full");
            }
            self.persisted = Some(state.clone());
            Ok(())
        }
    }

    impl Fake {
        fn take(&mut self) -> Vec<String> {
            std::mem::take(&mut self.calls)
        }
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()
    } // a Monday
    fn at(h: u32, m: u32) -> NaiveDateTime {
        day().and_hms_opt(h, m, 0).unwrap()
    }

    fn snapshot(user: &str, rule: DayRule) -> AccountSnapshot {
        AccountSnapshot {
            user: user.into(),
            enforce: true,
            for_day: day(),
            week: vec![rule; 7],
            blackouts: vec![],
            used_secs: BTreeMap::new(),
            games: vec!["steam:1".into()],
            ignored: vec!["kitty".into()],
            timer: None,
        }
    }

    fn evenings() -> DayRule {
        // allowed 6:15am–9pm
        DayRule {
            restricted: true,
            stretches: vec![Stretch {
                start_min: 375,
                end_min: 1260,
            }],
            budgets: BTreeMap::new(),
        }
    }

    fn games_budget(minutes: u32) -> DayRule {
        DayRule {
            restricted: false,
            stretches: vec![],
            budgets: BTreeMap::from([(GAMES, minutes)]),
        }
    }

    fn enforcer(snaps: Vec<AccountSnapshot>, server_time: NaiveDateTime) -> Enforcer {
        let mut e = Enforcer::new(Persisted::default());
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time,
                accounts: snaps,
            },
            server_time,
        );
        e
    }

    fn app(id: &str) -> RunningApp {
        RunningApp {
            id: id.into(),
            name: id.into(),
            pids: vec![42],
            window: None,
        }
    }

    fn obs(user: &str, locked: bool, running: Vec<RunningApp>) -> Observation {
        Observation {
            user: user.into(),
            sessions: vec![SessionInfo {
                id: "7".into(),
                locked,
            }],
            running,
            streaming_host: false,
        }
    }

    #[test]
    fn an_ended_lock_timer_blocks_and_says_so() {
        use protocol::rules::{Timer, TimerMode};
        let mut snap = snapshot("kid1", DayRule::default());
        snap.timer = Some(Timer {
            ends: at(19, 42),
            mode: TimerMode::Lock,
        });
        let mut e = enforcer(vec![snap.clone()], at(19, 0));
        let mut fake = Fake::default();
        // Ten minutes out: the usual lock warning, counting to the timer
        e.tick(at(19, 32), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.calls.contains(
            &"notify kid1: 10 minutes left today: the computer locks at 7:42pm".to_string()
        ));
        fake.take();
        e.tick(at(19, 42), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.calls.contains(&"disable kid1".to_string()));
        assert!(
            fake.calls
                .contains(&"notify kid1: Computer time is over: timer ended at 7:42pm".to_string())
        );
        assert_eq!(
            fake.banner,
            ["kid1: computer time is over: timer ended at 7:42pm"]
        );
        // "Allow again": the next snapshot has no timer
        snap.timer = None;
        e.apply_response(
            &ReportResponse {
                server_time: at(19, 50),
                accounts: vec![snap],
                messages: Vec::new(),
            },
            at(19, 50),
        );
        fake.take();
        e.tick(at(19, 50), &[obs("kid1", true, vec![])], &mut fake);
        assert!(fake.calls.contains(&"enable kid1".to_string()));
    }

    #[test]
    fn an_ended_games_timer_closes_games() {
        use protocol::rules::{Timer, TimerMode};
        let mut snap = snapshot("kid1", DayRule::default());
        snap.timer = Some(Timer {
            ends: at(16, 30),
            mode: TimerMode::Games,
        });
        let mut e = enforcer(vec![snap], at(16, 0));
        let mut fake = Fake::default();
        e.tick(
            at(16, 30),
            &[obs("kid1", false, vec![app("steam:1")])],
            &mut fake,
        );
        assert!(fake.calls.contains(&"close kid1 steam:1".to_string()));
        assert!(
            !fake.calls.iter().any(|c| c.starts_with("disable")),
            "games mode doesn't lock"
        );
    }

    #[test]
    fn warns_once_at_each_threshold_before_a_lock() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(20, 50), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            [
                "notify kid1: 10 minutes left today: the computer locks at 9:00pm",
                "banner "
            ]
        );
        e.tick(at(20, 51), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.take().is_empty(), "the 10-minute warning fires once");
        e.tick(at(20, 55), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            ["notify kid1: 5 minutes left today: the computer locks at 9:00pm"]
        );
        e.tick(at(20, 59), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            ["notify kid1: 1 minute left: save your game. The computer locks at 9:00pm"]
        );
    }

    #[test]
    fn a_late_start_gets_only_the_nearest_warning() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(20, 57), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            [
                "notify kid1: 5 minutes left today: the computer locks at 9:00pm",
                "banner "
            ]
        );
    }

    #[test]
    fn no_warnings_for_a_kid_who_isnt_on_this_pc() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        let away = Observation {
            user: "kid1".into(),
            sessions: vec![],
            running: vec![],
            streaming_host: false,
        };
        e.tick(at(20, 55), &[away], &mut fake);
        assert_eq!(fake.take(), ["banner "]);
    }

    #[test]
    fn blocking_disables_login_locks_and_relocks_then_restores() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            [
                "persist",
                "disable kid1",
                "lock 7",
                "notify kid1: Computer time is over until Tue 6:15am",
                "banner kid1: computer time is over until Tue 6:15am",
            ]
        );
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
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time: next,
                accounts: vec![snap],
            },
            next,
        );
        e.tick(next, &[obs("kid1", true, vec![])], &mut fake);
        assert_eq!(fake.take(), ["enable kid1", "banner "]);
        assert!(e.persisted.login_disabled.is_empty());
    }

    #[test]
    fn an_account_already_locked_by_someone_else_is_never_unlocked() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake {
            already_locked: BTreeSet::from(["kid1".into()]),
            ..Default::default()
        };
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert!(!fake.calls.iter().any(|c| c.starts_with("disable")));
        assert!(e.persisted.login_disabled.is_empty());
        fake.take();
        let mut snap = snapshot("kid1", DayRule::default());
        snap.for_day = day();
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time: at(21, 5),
                accounts: vec![snap],
            },
            at(21, 5),
        );
        e.tick(at(21, 5), &[obs("kid1", true, vec![])], &mut fake);
        assert!(!fake.calls.iter().any(|c| c.starts_with("enable")));
    }

    #[test]
    fn blackout_text_and_banner_name_the_blackout() {
        let mut snap = snapshot("kid1", DayRule::default());
        snap.blackouts = vec![BlackoutSpan {
            start: at(17, 0),
            end: at(19, 0),
            note: "dinner".into(),
        }];
        let mut e = enforcer(vec![snap], at(17, 0));
        let mut fake = Fake::default();
        e.tick(at(17, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert!(
            fake.calls
                .contains(&"notify kid1: Computer time is over until 7:00pm (dinner)".to_string())
        );
        assert_eq!(
            fake.banner,
            ["kid1: computer time is over until 7:00pm (dinner)"]
        );
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
        // Checked and repaired every tick (block_stream is idempotent); the record stays, nothing else repeats
        e.tick(at(21, 1), std::slice::from_ref(&o), &mut fake);
        assert_eq!(fake.take(), ["block kid1"]);
        assert!(e.persisted.streams_blocked.contains("kid1"));
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
        assert_eq!(
            fake.take(),
            [
                "notify kid1: 1 minute of games left today: save your game",
                "banner "
            ]
        );
        // A minute of local play later
        e.count(
            "kid1",
            UserState::Active,
            &[protocol::App {
                id: "steam:1".into(),
                name: "x".into(),
            }],
            60,
            day(),
        );
        e.tick(at(15, 1), &[obs("kid1", false, running.clone())], &mut fake);
        assert_eq!(
            fake.take(),
            [
                "notify kid1: Games time is used up for today",
                "close kid1 steam:1"
            ]
        );
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
        assert_eq!(fake.take().len(), 2); // the warning and the first banner
        snap.week = vec![
            DayRule {
                restricted: true,
                stretches: vec![Stretch {
                    start_min: 375,
                    end_min: 1320,
                }], // until 10pm now
                budgets: BTreeMap::new(),
            };
            7
        ];
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time: at(20, 56),
                accounts: vec![snap],
            },
            at(20, 56),
        );
        e.tick(at(21, 55), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            ["notify kid1: 5 minutes left today: the computer locks at 10:00pm"]
        );
    }

    #[test]
    fn offline_counting_adds_to_the_snapshot_and_restarts_at_midnight() {
        let mut snap = snapshot("kid1", games_budget(30));
        snap.used_secs = BTreeMap::from([(GAMES, 20 * 60)]);
        let mut e = enforcer(vec![snap], at(23, 0));
        let games = [protocol::App {
            id: "steam:1".into(),
            name: "x".into(),
        }];
        e.count("kid1", UserState::Active, &games, 600, day());
        let mut fake = Fake::default();
        e.tick(
            at(23, 30),
            &[obs("kid1", false, vec![app("steam:1")])],
            &mut fake,
        );
        assert!(
            fake.calls.contains(&"close kid1 steam:1".to_string()),
            "20 + 10 minutes used up the 30"
        );
        fake.take();
        // After midnight, without a new snapshot: usage restarts, so nothing is closed
        let tuesday = day().succ_opt().unwrap();
        e.count("kid1", UserState::Active, &games, 15, tuesday);
        e.tick(
            tuesday.and_hms_opt(0, 5, 0).unwrap(),
            &[obs("kid1", false, vec![app("steam:1")])],
            &mut fake,
        );
        assert!(fake.take().is_empty());
    }

    #[test]
    fn ignored_and_idle_time_is_not_counted() {
        let mut snap = snapshot("kid1", games_budget(1));
        snap.used_secs = BTreeMap::new();
        let mut e = enforcer(vec![snap], at(15, 0));
        e.count(
            "kid1",
            UserState::Active,
            &[protocol::App {
                id: "kitty".into(),
                name: "kitty".into(),
            }],
            120,
            day(),
        );
        e.count(
            "kid1",
            UserState::Idle,
            &[protocol::App {
                id: "steam:1".into(),
                name: "x".into(),
            }],
            120,
            day(),
        );
        let mut fake = Fake::default();
        e.tick(
            at(15, 0),
            &[obs("kid1", false, vec![app("steam:1")])],
            &mut fake,
        );
        assert!(!fake.calls.iter().any(|c| c.starts_with("close")));
    }

    #[test]
    fn a_new_snapshot_resets_the_local_count() {
        let mut snap = snapshot("kid1", games_budget(30));
        let mut e = enforcer(vec![snap.clone()], at(15, 0));
        let games = [protocol::App {
            id: "steam:1".into(),
            name: "x".into(),
        }];
        e.count("kid1", UserState::Active, &games, 25 * 60, day());
        snap.used_secs = BTreeMap::from([(GAMES, 25 * 60)]); // the server has those samples now
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time: at(15, 25),
                accounts: vec![snap],
            },
            at(15, 25),
        );
        let mut fake = Fake::default();
        e.tick(
            at(15, 25),
            &[obs("kid1", false, vec![app("steam:1")])],
            &mut fake,
        );
        assert!(
            !fake.calls.iter().any(|c| c.starts_with("close")),
            "25 minutes, not 50"
        );
    }

    #[test]
    fn enforce_off_or_no_snapshot_means_hands_off() {
        let mut snap = snapshot("kid1", evenings());
        snap.enforce = false;
        let mut e = enforcer(vec![snap], at(22, 0));
        let mut fake = Fake::default();
        e.tick(
            at(22, 0),
            &[
                obs("kid1", false, vec![app("steam:1")]),
                obs("kid2", false, vec![app("steam:1")]),
            ],
            &mut fake,
        );
        assert_eq!(fake.take(), ["banner "]);
    }

    #[test]
    fn the_server_clock_wins_when_ours_is_far_off() {
        // Our clock says 8:00pm, the server's says 9:00pm
        let mut e = Enforcer::new(Persisted::default());
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time: at(21, 0),
                accounts: vec![snapshot("kid1", evenings())],
            },
            at(20, 0),
        );
        let mut fake = Fake::default();
        e.tick(at(20, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.calls.contains(&"disable kid1".to_string()));
        // A small difference is ignored
        let mut e = Enforcer::new(Persisted::default());
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time: at(20, 2),
                accounts: vec![snapshot("kid1", evenings())],
            },
            at(20, 0),
        );
        assert_eq!(e.persisted.clock_offset_secs, 0);
    }

    #[test]
    fn failures_are_reported_and_retried() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake {
            fail_lock: true,
            ..Default::default()
        };
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
        assert!(e.release_all(&mut fake));
        assert_eq!(fake.take(), ["enable kid1", "unblock kid1", "banner "]);
        assert!(e.persisted.login_disabled.is_empty() && e.persisted.streams_blocked.is_empty());
    }

    #[test]
    fn release_all_reports_what_it_could_not_undo() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", true, vec![])], &mut fake);
        fake.fail_enable = true;
        assert!(!e.release_all(&mut fake));
        assert!(e.persisted.login_disabled.contains("kid1"));
    }

    #[test]
    fn a_stream_block_from_before_a_reboot_is_put_back() {
        // The nftables table is gone after a reboot, but the state file still records the block
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        e.persisted.streams_blocked.insert("kid1".into());
        let saved = serde_json::to_string(&e.persisted).unwrap();
        let mut e = Enforcer::new(serde_json::from_str(&saved).unwrap());
        let mut fake = Fake::default();
        let mut o = obs("kid1", true, vec![]);
        o.streaming_host = true;
        e.tick(at(21, 0), std::slice::from_ref(&o), &mut fake);
        assert!(fake.take().contains(&"block kid1".to_string()));
        e.tick(at(21, 1), &[o], &mut fake);
        assert_eq!(fake.take(), ["block kid1"]);
        assert!(e.persisted.streams_blocked.contains("kid1"));
    }

    #[test]
    fn a_repeated_banner_failure_is_remembered() {
        let mut e = enforcer(vec![], at(20, 0));
        let mut fake = Fake {
            fail_banner: true,
            ..Default::default()
        };
        e.tick(at(20, 0), &[], &mut fake);
        assert_eq!(e.banner_error.as_deref(), Some("dconf said no"));
        e.tick(at(20, 1), &[], &mut fake);
        assert_eq!(
            fake.take(),
            ["banner ", "banner "],
            "still retried every tick"
        );
        fake.fail_banner = false;
        e.tick(at(20, 2), &[], &mut fake);
        assert_eq!(e.banner_error, None);
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
        assert!(
            fake.calls.contains(&"enable kid1".to_string()),
            "Tuesday's rule allows 6:15am, so the block lifts without the server"
        );
    }

    #[test]
    fn banner_lists_every_blocked_kid_and_is_only_set_on_change() {
        let mut e = enforcer(
            vec![snapshot("kid1", evenings()), snapshot("kid2", evenings())],
            at(20, 0),
        );
        let mut fake = Fake::default();
        e.tick(
            at(21, 0),
            &[obs("kid1", true, vec![]), obs("kid2", true, vec![])],
            &mut fake,
        );
        assert_eq!(
            fake.banner,
            [
                "kid1: computer time is over until Tue 6:15am",
                "kid2: computer time is over until Tue 6:15am"
            ]
        );
        fake.take();
        e.tick(
            at(21, 1),
            &[obs("kid1", true, vec![]), obs("kid2", true, vec![])],
            &mut fake,
        );
        assert!(!fake.calls.iter().any(|c| c.starts_with("banner")));
    }

    #[test]
    fn midnight_reads_midnight() {
        assert_eq!(minute_clock(0), "midnight");
        assert_eq!(minute_clock(15), "12:15am");
        assert_eq!(minute_clock(720), "12:00pm");
        // Tonight's midnight needs no day name; one further away keeps it, as a time
        assert_eq!(clock(tuesday(0, 0), at(23, 50)), "midnight");
        assert_eq!(
            clock(tuesday(0, 0) + chrono::Days::new(2), at(23, 50)),
            "Thu 12:00am"
        );
        assert_eq!(clock(tuesday(9, 0), at(23, 50)), "Tue 9:00am");
    }

    fn rule_from(start_min: u16, end_min: u16) -> DayRule {
        DayRule {
            restricted: true,
            stretches: vec![Stretch { start_min, end_min }],
            budgets: BTreeMap::new(),
        }
    }

    fn tuesday(h: u32, m: u32) -> NaiveDateTime {
        day().succ_opt().unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn blocked_at_night_stays_blocked_past_midnight() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        fake.take();
        e.tick(tuesday(0, 0), &[obs("kid1", true, vec![])], &mut fake);
        e.tick(tuesday(0, 1), &[obs("kid1", true, vec![])], &mut fake);
        // Only the banner's wording changes (the day name drops out once it is Tuesday)
        assert_eq!(
            fake.take(),
            ["banner kid1: computer time is over until 6:15am"]
        );
        e.tick(tuesday(6, 15), &[obs("kid1", true, vec![])], &mut fake);
        assert_eq!(fake.take(), ["enable kid1", "banner "]);
    }

    #[test]
    fn warns_before_a_lock_at_midnight() {
        let mut snap = snapshot("kid1", rule_from(375, 1440));
        // Tuesday starts at 10am
        snap.week[1] = rule_from(600, 1200);
        let mut e = enforcer(vec![snap], at(23, 0));
        let mut fake = Fake::default();
        e.tick(at(23, 50), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            [
                "notify kid1: 10 minutes left today: the computer locks at midnight",
                "banner "
            ]
        );
        e.tick(tuesday(0, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            fake.take(),
            [
                "persist",
                "disable kid1",
                "lock 7",
                "notify kid1: Computer time is over until 10:00am",
                "banner kid1: computer time is over until 10:00am"
            ]
        );
    }

    #[test]
    fn offline_for_days_keeps_the_weekly_rules() {
        let mut snap = snapshot("kid1", DayRule::default());
        snap.week[1] = rule_from(600, 1200); // Tuesday 10am-8pm
        let mut e = enforcer(vec![snap], at(12, 0));
        let mut fake = Fake::default();
        e.tick(tuesday(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert!(fake.calls.contains(&"disable kid1".to_string()));
        assert!(
            fake.calls
                .contains(&"notify kid1: Computer time is over until midnight".to_string())
        );
        let mut e = enforcer(
            vec![{
                let mut s = snapshot("kid1", DayRule::default());
                s.week[1] = rule_from(600, 1200);
                s
            }],
            at(12, 0),
        );
        let mut fake = Fake::default();
        e.tick(tuesday(12, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(fake.take(), ["banner "]);
    }

    #[test]
    fn the_record_is_on_disk_before_login_is_disabled() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        let calls = fake.take();
        let persist = calls.iter().position(|c| c == "persist");
        let disable = calls.iter().position(|c| c == "disable kid1");
        assert!(
            persist.is_some() && persist < disable,
            "persist before disable: {calls:?}"
        );
        assert!(
            fake.persisted
                .as_ref()
                .is_some_and(|p| p.login_disabled.contains("kid1")),
            "the persisted state records the disable"
        );
    }

    #[test]
    fn login_is_not_disabled_when_the_record_cant_be_saved() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake {
            fail_persist: true,
            ..Default::default()
        };
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        let calls = fake.take();
        assert!(!calls.contains(&"disable kid1".to_string()), "{calls:?}");
        assert!(
            calls.contains(&"lock 7".to_string()),
            "the lock still applies"
        );
        assert!(e.persisted.login_disabled.is_empty());
        assert_eq!(
            e.take_errors("kid1"),
            ["couldn't save state; not disabling login: disk full"]
        );
        // Retried on the next tick, and done once the disk works again
        fake.fail_persist = false;
        e.tick(at(21, 1), &[obs("kid1", true, vec![])], &mut fake);
        assert_eq!(fake.take(), ["persist", "disable kid1"]);
        assert!(e.persisted.login_disabled.contains("kid1"));
    }

    #[test]
    fn a_login_disabled_behind_kidtimes_back_is_reported_once() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(12, 0));
        let mut fake = Fake {
            already_locked: BTreeSet::from(["kid1".into()]),
            ..Default::default()
        };
        e.tick(at(12, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            e.take_errors("kid1"),
            [
                "login for kid1 is disabled but kidtime didn't do it; if it should be enabled run `sudo usermod -U kid1`"
            ]
        );
        e.tick(at(12, 1), &[obs("kid1", false, vec![])], &mut fake);
        assert!(e.take_errors("kid1").is_empty(), "once per process");
        assert!(!fake.calls.iter().any(|c| c.starts_with("enable")));
    }

    fn session(id: &str, locked: bool) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            locked,
        }
    }

    #[test]
    fn the_fast_check_relocks_and_reports_unlocks_once_a_minute() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        assert!(e.blocked_users().is_empty());
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(e.blocked_users(), ["kid1"]);
        e.take_errors("kid1");
        fake.take();
        // Still locked: nothing to do
        e.relock(at(21, 0), "kid1", &[session("7", true)], &mut fake);
        assert!(fake.take().is_empty());
        assert!(e.take_errors("kid1").is_empty());
        // The kid unlocked it from inside the session
        let t = at(21, 0) + chrono::Duration::seconds(3);
        e.relock(t, "kid1", &[session("7", false)], &mut fake);
        assert_eq!(fake.take(), ["lock 7"]);
        assert_eq!(
            e.take_errors("kid1"),
            ["kid1 unlocked the session while blocked"]
        );
        // Seen locked again, then unlocked 20 seconds after the report: locked again, not reported yet
        e.relock(
            t + chrono::Duration::seconds(10),
            "kid1",
            &[session("7", true)],
            &mut fake,
        );
        let t = t + chrono::Duration::seconds(20);
        e.relock(t, "kid1", &[session("7", false)], &mut fake);
        assert_eq!(fake.take(), ["lock 7"]);
        assert!(e.take_errors("kid1").is_empty());
        // Seen locked, then unlocked a minute after the report: reported again
        e.relock(
            t + chrono::Duration::seconds(10),
            "kid1",
            &[session("7", true)],
            &mut fake,
        );
        let t = t + chrono::Duration::seconds(41);
        e.relock(t, "kid1", &[session("7", false)], &mut fake);
        assert_eq!(fake.take(), ["lock 7"]);
        assert_eq!(
            e.take_errors("kid1"),
            ["kid1 unlocked the session while blocked"]
        );
        // Allowed again: no longer blocked, and the fast check does nothing
        e.tick(tuesday(6, 15), &[obs("kid1", true, vec![])], &mut fake);
        assert!(e.blocked_users().is_empty());
        fake.take();
        e.relock(tuesday(6, 15), "kid1", &[session("7", false)], &mut fake);
        assert!(fake.take().is_empty());
    }

    #[test]
    fn a_lock_that_hasnt_taken_effect_yet_is_not_an_unlock() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        e.take_errors("kid1");
        fake.take();
        // Lock requested, but GNOME hasn't set LockedHint yet: ask again, report nothing
        let t = at(21, 0) + chrono::Duration::seconds(1);
        e.relock(t, "kid1", &[session("7", false)], &mut fake);
        assert_eq!(fake.take(), ["lock 7"]);
        assert!(e.take_errors("kid1").is_empty());
    }

    #[test]
    fn a_session_that_never_shows_locked_is_never_reported_as_unlocked() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        e.take_errors("kid1");
        for secs in 1..=180 {
            let t = at(21, 0) + chrono::Duration::seconds(secs);
            e.relock(t, "kid1", &[session("7", false)], &mut fake);
        }
        assert!(e.take_errors("kid1").is_empty());
    }

    #[test]
    fn a_session_seen_locked_then_unlocked_is_reported_once() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        e.take_errors("kid1");
        let t = at(21, 0) + chrono::Duration::seconds(1);
        e.relock(t, "kid1", &[session("7", true)], &mut fake);
        let t = t + chrono::Duration::seconds(1);
        e.relock(t, "kid1", &[session("7", false)], &mut fake);
        // Not yet seen locked again: no second report
        let t = t + chrono::Duration::seconds(1);
        e.relock(t, "kid1", &[session("7", false)], &mut fake);
        assert_eq!(
            e.take_errors("kid1"),
            ["kid1 unlocked the session while blocked"]
        );
    }

    #[test]
    fn a_lock_that_failed_is_not_reported_as_an_unlock() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake {
            fail_lock: true,
            ..Default::default()
        };
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        e.take_errors("kid1");
        e.relock(at(21, 0), "kid1", &[session("7", false)], &mut fake);
        assert_eq!(e.take_errors("kid1"), ["lock failed: logind said no"]);
        // The 5-second tick seeing the kid unlock a session that was locked counts too
        fake.fail_lock = false;
        e.tick(at(21, 1), &[obs("kid1", true, vec![])], &mut fake);
        e.tick(at(21, 2), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            e.take_errors("kid1"),
            ["kid1 unlocked the session while blocked"]
        );
    }

    #[test]
    fn a_response_without_an_account_drops_its_rules() {
        let mut e = enforcer(
            vec![snapshot("kid1", evenings()), snapshot("kid2", evenings())],
            at(20, 0),
        );
        let mut fake = Fake::default();
        let both = [obs("kid1", true, vec![]), obs("kid2", true, vec![])];
        e.tick(at(21, 0), &both, &mut fake);
        fake.take();
        e.apply_response(
            &ReportResponse {
                messages: Vec::new(),
                server_time: at(21, 1),
                accounts: vec![snapshot("kid2", evenings())],
            },
            at(21, 1),
        );
        assert!(!e.persisted.snapshots.contains_key("kid1"));
        assert!(e.take_dirty());
        e.tick(at(21, 1), &both, &mut fake);
        assert!(fake.take().contains(&"enable kid1".to_string()));
        assert!(e.persisted.login_disabled.contains("kid2"));
    }

    #[test]
    fn a_reply_without_rules_releases_everything() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake::default();
        e.tick(at(21, 0), &[obs("kid1", true, vec![])], &mut fake);
        e.take_dirty();
        fake.take();
        e.clear_snapshots();
        assert!(e.persisted.snapshots.is_empty());
        assert!(e.take_dirty());
        e.tick(at(21, 1), &[obs("kid1", true, vec![])], &mut fake);
        assert_eq!(fake.take(), ["enable kid1", "banner "]);
    }

    #[test]
    fn a_kid_removed_from_the_config_is_released() {
        let mut e = enforcer(
            vec![snapshot("kid1", evenings()), snapshot("kid2", evenings())],
            at(20, 0),
        );
        let mut fake = Fake::default();
        let mut o1 = obs("kid1", true, vec![]);
        o1.streaming_host = true;
        let mut o2 = obs("kid2", true, vec![]);
        o2.streaming_host = true;
        e.tick(at(21, 0), &[o1, o2.clone()], &mut fake);
        assert!(e.persisted.login_disabled.contains("kid1"));
        assert!(e.persisted.streams_blocked.contains("kid1"));
        fake.take();
        // kid1 is no longer tracked: everything done to it is undone, and its rules are dropped
        e.tick(at(21, 1), &[o2], &mut fake);
        let calls = fake.take();
        assert!(calls.contains(&"enable kid1".to_string()), "{calls:?}");
        assert!(calls.contains(&"unblock kid1".to_string()), "{calls:?}");
        assert!(!calls.contains(&"enable kid2".to_string()));
        assert!(!e.persisted.login_disabled.contains("kid1"));
        assert!(!e.persisted.streams_blocked.contains("kid1"));
        assert!(!e.persisted.snapshots.contains_key("kid1"));
        assert!(e.persisted.snapshots.contains_key("kid2"));
        assert_eq!(
            fake.banner,
            ["kid2: computer time is over until Tue 6:15am"]
        );
    }

    #[test]
    fn a_kid_without_a_password_gets_the_lock_only_and_a_dashboard_error() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake {
            no_password: BTreeSet::from(["kid1".into()]),
            ..Default::default()
        };
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        let calls = fake.take();
        assert!(
            !calls
                .iter()
                .any(|c| c == "persist" || c.starts_with("disable"))
        );
        assert!(calls.contains(&"lock 7".to_string()));
        assert_eq!(
            e.take_errors("kid1"),
            ["can't disable login: no password; only the screen lock applies"]
        );
        // Unblocked and blocked again: not repeated in this process
        e.tick(tuesday(6, 15), &[obs("kid1", true, vec![])], &mut fake);
        e.tick(tuesday(22, 0), &[obs("kid1", true, vec![])], &mut fake);
        assert!(e.take_errors("kid1").is_empty());
        assert!(!fake.calls.iter().any(|c| c.starts_with("enable")));
    }

    #[test]
    fn an_account_already_locked_when_blocked_is_a_dashboard_error() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake {
            already_locked: BTreeSet::from(["kid1".into()]),
            ..Default::default()
        };
        e.tick(at(21, 0), &[obs("kid1", false, vec![])], &mut fake);
        assert_eq!(
            e.take_errors("kid1"),
            [
                "kid1's login was already disabled when kidtime blocked it; kidtime won't re-enable it"
            ]
        );
    }

    #[test]
    fn a_disable_that_took_effect_but_reported_failure_is_still_undone() {
        let mut e = enforcer(vec![snapshot("kid1", evenings())], at(20, 0));
        let mut fake = Fake {
            fail_disable: true,
            ..Default::default()
        };
        e.tick(at(21, 0), &[obs("kid1", true, vec![])], &mut fake);
        assert!(e.persisted.login_disabled.contains("kid1"));
        assert_eq!(
            e.take_errors("kid1"),
            ["disabling login failed: usermod said no"]
        );
        fake.take();
        e.tick(tuesday(6, 15), &[obs("kid1", true, vec![])], &mut fake);
        assert!(fake.calls.contains(&"enable kid1".to_string()));
        assert!(e.persisted.login_disabled.is_empty());
    }
}
