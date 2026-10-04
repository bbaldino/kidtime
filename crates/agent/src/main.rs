//! kidtime-agent: samples which tracked users are active and what they're
//! running, reports it to the kidtime server, and enforces the server's rules.

mod actions;
mod apps;
mod enforce;
mod files;
mod logind;
mod sunshine;
mod sway;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDate, NaiveDateTime};
use protocol::{App, Report, ReportResponse, Sample, UserSample, UserState};
use serde::Deserialize;

/// Samples kept while the server is unreachable (a day at the default interval).
const MAX_PENDING: usize = 5760;
const MAX_BATCH: usize = 500;
/// Drop cached app names now and then so newly installed apps and games get picked up.
const CACHE_CLEAR_TICKS: u64 = 240;
const ENFORCE_EVERY: Duration = Duration::from_secs(5);
/// A blocked kid can unlock their own session from inside it: re-lock this often.
const RELOCK_EVERY: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
struct Config {
    server_url: String,
    token: String,
    /// Defaults to the machine's hostname.
    host: Option<String>,
    users: Vec<String>,
    #[serde(default = "default_interval")]
    interval_secs: u32,
    /// systemd units whose Steam games count as streamed play, e.g. "sway-sunshine.service".
    #[serde(default)]
    streaming_units: Vec<String>,
    /// IPC socket of the streaming session's Sway, e.g. "/run/user/{uid}/sway-sunshine.sock".
    /// When set, streamed time goes to the focused window instead of every running app.
    streaming_sway_socket: Option<String>,
    /// What enforcement did to this PC (disabled logins, cut streams) and the last rules, across restarts.
    #[serde(default = "default_state_file")]
    state_file: PathBuf,
    /// Sunshine base port per account, for cutting streams. Without it, the port is read from the kid's
    /// sunshine.conf (which the kid can edit), then Sunshine's default.
    #[serde(default)]
    sunshine_ports: HashMap<String, u16>,
}

fn default_interval() -> u32 {
    15
}

fn default_state_file() -> PathBuf {
    PathBuf::from("/var/lib/kidtime/agent-state.json")
}

struct TrackedUser {
    name: String,
    uid: u32,
    home: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let mut args = std::env::args().skip(1);
    let mut config_path = PathBuf::from("/etc/kidtime/agent.toml");
    let mut dump = false;
    let mut release_all = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().context("--config needs a path")?.into(),
            // Print one sample and exit, without contacting the server
            "--dump" => dump = true,
            // Undo everything enforcement did on this PC (logins, streams, banner) and exit
            "--release-all" => release_all = true,
            other => bail!("unknown argument: {other}"),
        }
    }

    let config = read_config(&config_path);
    if release_all {
        return release_everything(release_setup(config));
    }
    let config = config?;
    let host = match &config.host {
        Some(h) => h.clone(),
        None => std::fs::read_to_string("/proc/sys/kernel/hostname")?
            .trim()
            .to_string(),
    };
    let users = config
        .users
        .iter()
        .map(|name| lookup_user(name))
        .collect::<Result<Vec<_>>>()?;
    let users_map: HashMap<String, (u32, PathBuf)> = users
        .iter()
        .map(|u| (u.name.clone(), (u.uid, u.home.clone())))
        .collect();

    let logind = logind::Logind::connect()
        .await
        .context("connecting to logind")?;
    let mut scanner = apps::Scanner::default();
    let mut streams = sunshine::Monitor::default();

    if dump {
        // Two samples, so the stream check has encoder readings to compare
        take_sample(&logind, &mut scanner, &mut streams, &users, &config, 0, 0).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let sample = take_sample(&logind, &mut scanner, &mut streams, &users, &config, 0, 0).await;
        println!("{}", serde_json::to_string_pretty(&sample)?);
        return Ok(());
    }

    let agent_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")?
        .trim()
        .to_string();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let report_url = format!("{}/api/report", config.server_url.trim_end_matches('/'));
    let interval = Duration::from_secs(config.interval_secs.into());

    tracing::info!(
        "{host}: tracking {} every {}s, reporting to {report_url}",
        config.users.join(", "),
        config.interval_secs
    );

    let mut enforcer = enforce::Enforcer::new(load_state(&config.state_file));
    let mut actions = actions::SystemActions::new(
        users_map,
        config.streaming_sway_socket.clone(),
        config.state_file.clone(),
        config.sunshine_ports.clone(),
    );

    let mut pending: VecDeque<Sample> = VecDeque::new();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    // The first enforcement tick is immediate: a block from before a restart resumes at once
    let mut enforce_ticker = tokio::time::interval(ENFORCE_EVERY);
    enforce_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut relock_ticker = tokio::time::interval(RELOCK_EVERY);
    relock_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Instant uses CLOCK_MONOTONIC, which stops while the machine is suspended
    let mut last_tick = Instant::now();
    let mut seq = 0u64;
    // systemd stops the agent with SIGTERM: exit between iterations, with the state saved
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("listening for SIGTERM")?;

    // One task: each arm runs to completion before the next tick is picked, and a sample tick
    // sends at most one batch, so enforcement waits at most one request (the client timeout).
    // Every command an arm runs is killed after 5 seconds, so no arm can block the others for long.
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = Instant::now();
                // Cap in case the process was stalled; better to undercount than overcount
                let elapsed = now.duration_since(last_tick).min(interval * 2).as_secs() as u32;
                last_tick = now;
                seq += 1;
                if seq.is_multiple_of(CACHE_CLEAR_TICKS) {
                    scanner.clear_caches();
                }

                let mut sample = take_sample(
                    &logind,
                    &mut scanner,
                    &mut streams,
                    &users,
                    &config,
                    seq,
                    elapsed,
                )
                .await;
                let today = enforcer_today(&enforcer);
                for u in &mut sample.users {
                    enforcer.count(&u.user, u.state, &u.apps, i64::from(elapsed), today);
                    u.overrun = enforcer.overrun(&u.user);
                    u.errors = enforcer.take_errors(&u.user);
                }
                pending.push_back(sample);
                while pending.len() > MAX_PENDING {
                    pending.pop_front();
                }

                let batch: Vec<Sample> = pending.iter().take(MAX_BATCH).cloned().collect();
                let sent = batch.len();
                let report = Report {
                    host: host.clone(),
                    agent_id: agent_id.clone(),
                    interval_secs: config.interval_secs,
                    samples: batch,
                };
                let result = client
                    .post(&report_url)
                    .bearer_auth(&config.token)
                    .json(&report)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status());
                match result {
                    Ok(response) => {
                        pending.drain(..sent);
                        if !pending.is_empty() {
                            tracing::info!("{} samples still queued", pending.len());
                        }
                        let body = response.bytes().await.unwrap_or_else(|e| {
                            tracing::debug!("reading the report response: {e}");
                            Default::default()
                        });
                        // The response resets the local count, so it only applies once the server has every
                        // sample counted here; with a backlog, the reply to the last batch does
                        if pending.is_empty() {
                            handle_reply(&mut enforcer, &body, Local::now().naive_local());
                            save_if_dirty(&mut enforcer, &config.state_file);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("report failed ({} samples queued): {e}", pending.len());
                    }
                }
            }
            _ = enforce_ticker.tick() => {
                let observations = observe(&logind, &mut scanner, &users, &config).await;
                enforcer.tick(Local::now().naive_local(), &observations, &mut actions);
                save_if_dirty(&mut enforcer, &config.state_file);
            }
            _ = relock_ticker.tick() => {
                let blocked = enforcer.blocked_users();
                if !blocked.is_empty() {
                    let now = Local::now().naive_local();
                    for user in users.iter().filter(|u| blocked.contains(&u.name)) {
                        match logind.graphical_sessions(user.uid).await {
                            Ok(sessions) => enforcer.relock(now, &user.name, &sessions, &mut actions),
                            Err(e) => tracing::debug!("logind sessions for {}: {e}", user.name),
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }
    }
    tracing::info!("stopping");
    save_if_dirty(&mut enforcer, &config.state_file);
    Ok(())
}

fn read_config(path: &Path) -> Result<Config> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// What `--release-all` works from.
struct ReleaseSetup {
    users: HashMap<String, (u32, PathBuf)>,
    streaming_sway_socket: Option<String>,
    state_file: PathBuf,
    sunshine_ports: HashMap<String, u16>,
}

/// `--release-all` must work even when the config is broken or an account is gone: undoing needs only the
/// account names in the state file (`usermod -U`, the nftables rules) and nothing at all for the banner.
fn release_setup(config: Result<Config>) -> ReleaseSetup {
    match config {
        Ok(c) => ReleaseSetup {
            users: c
                .users
                .iter()
                .filter_map(|name| lookup_user(name).map_err(|e| tracing::warn!("{e:#}")).ok())
                .map(|u| (u.name, (u.uid, u.home)))
                .collect(),
            streaming_sway_socket: c.streaming_sway_socket,
            state_file: c.state_file,
            sunshine_ports: c.sunshine_ports,
        },
        Err(e) => {
            let state_file = default_state_file();
            tracing::warn!(
                "{e:#}; releasing from the state file alone ({})",
                state_file.display()
            );
            ReleaseSetup {
                users: HashMap::new(),
                streaming_sway_socket: None,
                state_file,
                sunshine_ports: HashMap::new(),
            }
        }
    }
}

/// Undoes everything recorded in the state file; an error if anything remains.
fn release_everything(setup: ReleaseSetup) -> Result<()> {
    let mut actions = actions::SystemActions::new(
        setup.users,
        setup.streaming_sway_socket,
        setup.state_file.clone(),
        setup.sunshine_ports,
    );
    let mut enforcer = enforce::Enforcer::new(load_state(&setup.state_file));
    let released = enforcer.release_all(&mut actions);
    save_state(&setup.state_file, &enforcer.persisted)?;
    let state = &enforcer.persisted;
    if !state.login_disabled.is_empty() || !state.streams_blocked.is_empty() {
        let list = |set: &std::collections::BTreeSet<String>| {
            set.iter().cloned().collect::<Vec<_>>().join(", ")
        };
        bail!(
            "could not undo everything: login still disabled for [{}], stream still blocked for [{}]",
            list(&state.login_disabled),
            list(&state.streams_blocked)
        );
    }
    if !released {
        bail!("could not clear the login screen banner");
    }
    Ok(())
}

/// The date the enforcer decides with: local time corrected by the server's clock, if ours is off.
fn enforcer_today(enforcer: &enforce::Enforcer) -> NaiveDate {
    (Local::now().naive_local() + chrono::Duration::seconds(enforcer.persisted.clock_offset_secs))
        .date()
}

/// What a successful (2xx) report response says.
#[derive(Debug)]
enum Reply {
    /// An empty body: an old server, which has no rules (for instance after a rollback).
    NoRules,
    Rules(ReportResponse),
    /// A body that doesn't parse: something is wrong, so keep enforcing what we have.
    Unreadable(String),
}

fn parse_reply(body: &[u8]) -> Reply {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Reply::NoRules;
    }
    match serde_json::from_slice(body) {
        Ok(response) => Reply::Rules(response),
        Err(e) => Reply::Unreadable(e.to_string()),
    }
}

/// Applies a successful report response: no rules drops every snapshot (so the next tick releases
/// everything); rules replace the snapshots; an unreadable body changes nothing.
fn handle_reply(enforcer: &mut enforce::Enforcer, body: &[u8], local_now: NaiveDateTime) {
    match parse_reply(body) {
        Reply::NoRules => {
            tracing::debug!("the server sent no rules");
            enforcer.clear_snapshots();
        }
        Reply::Rules(response) => enforcer.apply_response(&response, local_now),
        Reply::Unreadable(e) => {
            tracing::warn!("ignoring a report response that doesn't parse: {e}")
        }
    }
}

/// What enforcement needs to see of each tracked account right now.
async fn observe(
    logind: &logind::Logind,
    scanner: &mut apps::Scanner,
    users: &[TrackedUser],
    config: &Config,
) -> Vec<enforce::Observation> {
    let streaming_host = !config.streaming_units.is_empty();
    let mut observations = Vec::with_capacity(users.len());
    for user in users {
        let sessions = logind
            .graphical_sessions(user.uid)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("logind sessions for {} failed: {e}", user.name);
                Vec::new()
            });
        let mut running = scanner
            .scan(user.uid, &user.home, &config.streaming_units)
            .running;
        // The stream's focused window: this is how a non-Steam game in a stream can be closed
        if streaming_host && let Some(pattern) = &config.streaming_sway_socket {
            let socket = PathBuf::from(pattern.replace("{uid}", &user.uid.to_string()));
            if let Some(window) = sway::focused_window(&socket) {
                let app = sway::app_for(&window, |appid| {
                    scanner.steam_name(user.uid, &user.home, appid, "")
                });
                let pid = window.pid;
                merge_focused(
                    &mut running,
                    app,
                    pid,
                    enforce::WindowRef {
                        socket,
                        con_id: window.con_id,
                    },
                );
            }
        }
        observations.push(enforce::Observation {
            user: user.name.clone(),
            sessions,
            running,
            streaming_host,
        });
    }
    observations
}

/// Adds the focused window's app to `running`, or attaches the window to the entry the process scan
/// already found for it (a Steam game).
fn merge_focused(
    running: &mut Vec<enforce::RunningApp>,
    app: App,
    pid: u32,
    window: enforce::WindowRef,
) {
    match running.iter_mut().find(|a| a.id == app.id) {
        Some(existing) => existing.window = Some(window),
        None => running.push(enforce::RunningApp {
            id: app.id,
            name: app.name,
            pids: vec![pid],
            window: Some(window),
        }),
    }
}

/// Saved at once, so a disabled login is on disk before anything else can go wrong.
fn save_if_dirty(enforcer: &mut enforce::Enforcer, path: &Path) {
    if enforcer.take_dirty()
        && let Err(e) = save_state(path, &enforcer.persisted)
    {
        tracing::error!("saving the state to {}: {e:#}", path.display());
    }
}

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
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    // systemd's StateDirectory normally creates it
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string(state)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn take_sample(
    logind: &logind::Logind,
    scanner: &mut apps::Scanner,
    streams: &mut sunshine::Monitor,
    users: &[TrackedUser],
    config: &Config,
    seq: u64,
    elapsed_secs: u32,
) -> Sample {
    let mut samples = Vec::with_capacity(users.len());
    for user in users {
        let state = logind.user_state(user.uid).await.unwrap_or_else(|e| {
            tracing::warn!("logind query for {} failed: {e}", user.name);
            UserState::Offline
        });
        let found = scanner.scan(user.uid, &user.home, &config.streaming_units);
        // Checked every tick (not only when needed) so there's always a previous reading to compare with
        let stream = if config.streaming_units.is_empty() {
            sunshine::Stream::Unknown
        } else {
            streams.check(user.uid)
        };
        let game_open = found
            .streamed_apps
            .iter()
            .any(|a| a.id.starts_with("steam:"));
        let anything_open = !found.streamed_apps.is_empty();
        // What's on the stream: the focused window when Sway can tell us, otherwise whatever is running
        let streamed_apps = config
            .streaming_sway_socket
            .as_ref()
            .and_then(|pattern| {
                sway::focused_window(Path::new(&pattern.replace("{uid}", &user.uid.to_string())))
            })
            .map(|window| {
                vec![sway::app_for(&window, |appid| {
                    scanner.steam_name(user.uid, &user.home, appid, "")
                })]
            })
            .unwrap_or(found.streamed_apps);
        let (state, apps) = match stream {
            _ if state.counts() => (state, found.apps),
            // Anything being streamed counts, even just the Steam menus
            sunshine::Stream::Connected => (UserState::Streaming, streamed_apps),
            // Something open over there, but nobody's watching
            sunshine::Stream::Disconnected if anything_open => {
                (UserState::StreamIdle, streamed_apps)
            }
            // Encoder can't be checked: count games rather than miss real play
            sunshine::Stream::Unknown if game_open => (UserState::Streaming, streamed_apps),
            _ => (state, found.apps),
        };
        samples.push(UserSample {
            overrun: Vec::new(),
            errors: Vec::new(),
            user: user.name.clone(),
            state,
            apps,
        });
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Sample {
        seq,
        at,
        elapsed_secs,
        users: samples,
    }
}

fn lookup_user(name: &str) -> Result<TrackedUser> {
    let passwd = std::fs::read_to_string("/etc/passwd")?;
    passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 6 && f[0] == name)
        .map(|f| TrackedUser {
            name: name.to_string(),
            uid: f[2].parse().unwrap_or(u32::MAX),
            home: Path::new(f[5]).to_path_buf(),
        })
        .with_context(|| format!("user {name} not found in /etc/passwd"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use enforce::{RunningApp, WindowRef};

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
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_state(&path), enforce::Persisted::default());
        assert_eq!(
            load_state(&dir.join("missing.json")),
            enforce::Persisted::default()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn focused_window_is_added_to_running_or_attached_to_its_game() {
        let window = |con_id| WindowRef {
            socket: PathBuf::from("/run/user/1001/sway-sunshine.sock"),
            con_id,
        };
        let game = RunningApp {
            id: "steam:1".into(),
            name: "Game".into(),
            pids: vec![10, 11],
            window: None,
        };
        let mut running = vec![game.clone()];

        // A non-Steam game in the stream: a new entry for the window's process
        let shortcut = App {
            id: "window:Heroes".into(),
            name: "Heroes".into(),
        };
        merge_focused(&mut running, shortcut, 42, window(7));
        assert_eq!(
            running,
            [
                game.clone(),
                RunningApp {
                    id: "window:Heroes".into(),
                    name: "Heroes".into(),
                    pids: vec![42],
                    window: Some(window(7)),
                },
            ]
        );

        // A Steam game the scan already found: keeps its processes, gains the window
        let mut running = vec![game.clone()];
        let steam_game = App {
            id: "steam:1".into(),
            name: "Game".into(),
        };
        merge_focused(&mut running, steam_game, 11, window(9));
        assert_eq!(
            running,
            [RunningApp {
                window: Some(window(9)),
                ..game
            }]
        );
    }

    #[test]
    fn release_all_works_from_the_state_file_when_the_config_is_broken() {
        let setup = release_setup(Err(anyhow::anyhow!("parsing /etc/kidtime/agent.toml")));
        assert_eq!(setup.state_file, default_state_file());
        assert!(setup.users.is_empty());
        // An account that no longer exists doesn't stop the others being released
        let config: Config = toml::from_str(
            "server_url = \"http://x\"\ntoken = \"t\"\nusers = [\"no-such-kid-here\"]\nstate_file = \"/tmp/s.json\"\n",
        )
        .unwrap();
        let setup = release_setup(Ok(config));
        assert_eq!(setup.state_file, PathBuf::from("/tmp/s.json"));
        assert!(setup.users.is_empty());
    }

    #[test]
    fn sunshine_ports_are_read_from_the_config() {
        let config: Config = toml::from_str(
            "server_url = \"http://x\"\ntoken = \"t\"\nusers = [\"kid1\"]\n[sunshine_ports]\nkid1 = 48189\n",
        )
        .unwrap();
        assert_eq!(config.sunshine_ports.get("kid1"), Some(&48189));
    }

    #[test]
    fn replies_are_classified() {
        assert!(matches!(parse_reply(b""), Reply::NoRules));
        assert!(matches!(parse_reply(b" \n"), Reply::NoRules));
        assert!(matches!(parse_reply(b"ok"), Reply::Unreadable(_)));
        assert!(matches!(
            parse_reply(b"{\"something\":1}"),
            Reply::Unreadable(_)
        ));
        let json = r#"{"server_time":"2026-10-05T12:00:00","accounts":[]}"#;
        assert!(matches!(parse_reply(json.as_bytes()), Reply::Rules(r) if r.accounts.is_empty()));
    }

    fn enforcer_with(users: &[&str]) -> (enforce::Enforcer, NaiveDateTime) {
        let now = NaiveDate::from_ymd_opt(2026, 10, 5)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut e = enforce::Enforcer::new(enforce::Persisted::default());
        handle_reply(&mut e, reply_json(users, now).as_bytes(), now);
        assert_eq!(e.persisted.snapshots.len(), users.len());
        e.take_dirty();
        (e, now)
    }

    fn reply_json(users: &[&str], now: NaiveDateTime) -> String {
        let accounts: Vec<protocol::AccountSnapshot> = users
            .iter()
            .map(|u| protocol::AccountSnapshot {
                user: u.to_string(),
                enforce: true,
                week: vec![Default::default(); 7],
                blackouts: vec![],
                used_secs: Default::default(),
                games: vec![],
                ignored: vec![],
                for_day: now.date(),
            })
            .collect();
        serde_json::to_string(&ReportResponse {
            server_time: now,
            accounts,
        })
        .unwrap()
    }

    #[test]
    fn an_empty_reply_drops_every_snapshot() {
        let (mut e, now) = enforcer_with(&["kid1", "kid2"]);
        handle_reply(&mut e, b"", now);
        assert!(e.persisted.snapshots.is_empty());
        assert!(e.take_dirty());
    }

    #[test]
    fn a_reply_with_rules_replaces_them_and_drops_omitted_accounts() {
        let (mut e, now) = enforcer_with(&["kid1", "kid2"]);
        handle_reply(&mut e, reply_json(&["kid2"], now).as_bytes(), now);
        assert_eq!(e.persisted.snapshots.keys().collect::<Vec<_>>(), ["kid2"]);
    }

    #[test]
    fn an_unreadable_reply_keeps_everything() {
        let (mut e, now) = enforcer_with(&["kid1", "kid2"]);
        handle_reply(&mut e, b"<html>bad gateway</html>", now);
        assert_eq!(e.persisted.snapshots.len(), 2);
        assert!(!e.take_dirty());
    }
}
