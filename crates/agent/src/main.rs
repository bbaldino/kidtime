//! kidtime-agent: samples which tracked users are active and what they're
//! running, and reports it to the kidtime server.

mod apps;
mod logind;
mod sunshine;
mod sway;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use protocol::{Report, Sample, UserSample, UserState};
use serde::Deserialize;

/// Samples kept while the server is unreachable (a day at the default interval).
const MAX_PENDING: usize = 5760;
const MAX_BATCH: usize = 500;
/// Drop cached app names now and then so newly installed apps and games get picked up.
const CACHE_CLEAR_TICKS: u64 = 240;

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
}

fn default_interval() -> u32 {
    15
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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().context("--config needs a path")?.into(),
            // Print one sample and exit, without contacting the server
            "--dump" => dump = true,
            other => bail!("unknown argument: {other}"),
        }
    }

    let config: Config = toml::from_str(
        &std::fs::read_to_string(&config_path).with_context(|| format!("reading {}", config_path.display()))?,
    )
    .with_context(|| format!("parsing {}", config_path.display()))?;
    let host = match &config.host {
        Some(h) => h.clone(),
        None => std::fs::read_to_string("/proc/sys/kernel/hostname")?.trim().to_string(),
    };
    let users = config
        .users
        .iter()
        .map(|name| lookup_user(name))
        .collect::<Result<Vec<_>>>()?;

    let logind = logind::Logind::connect().await.context("connecting to logind")?;
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

    let agent_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")?.trim().to_string();
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let report_url = format!("{}/api/report", config.server_url.trim_end_matches('/'));
    let interval = Duration::from_secs(config.interval_secs.into());

    tracing::info!("{host}: tracking {} every {}s, reporting to {report_url}", config.users.join(", "), config.interval_secs);

    let mut pending: VecDeque<Sample> = VecDeque::new();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    // Instant uses CLOCK_MONOTONIC, which stops while the machine is suspended
    let mut last_tick = Instant::now();
    let mut seq = 0u64;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        let now = Instant::now();
        // Cap in case the process was stalled; better to undercount than overcount
        let elapsed = now.duration_since(last_tick).min(interval * 2).as_secs() as u32;
        last_tick = now;
        seq += 1;
        if seq.is_multiple_of(CACHE_CLEAR_TICKS) {
            scanner.clear_caches();
        }

        pending.push_back(take_sample(&logind, &mut scanner, &mut streams, &users, &config, seq, elapsed).await);
        while pending.len() > MAX_PENDING {
            pending.pop_front();
        }

        while !pending.is_empty() {
            let batch: Vec<Sample> = pending.iter().take(MAX_BATCH).cloned().collect();
            let sent = batch.len();
            let report = Report { host: host.clone(), agent_id: agent_id.clone(), interval_secs: config.interval_secs, samples: batch };
            let result = client
                .post(&report_url)
                .bearer_auth(&config.token)
                .json(&report)
                .send()
                .await
                .and_then(|r| r.error_for_status());
            match result {
                Ok(_) => {
                    pending.drain(..sent);
                }
                Err(e) => {
                    tracing::warn!("report failed ({} samples queued): {e}", pending.len());
                    break;
                }
            }
        }
    }
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
        let stream = if config.streaming_units.is_empty() { sunshine::Stream::Unknown } else { streams.check(user.uid) };
        let game_open = found.streamed_apps.iter().any(|a| a.id.starts_with("steam:"));
        let anything_open = !found.streamed_apps.is_empty();
        // What's on the stream: the focused window when Sway can tell us, otherwise whatever is running
        let streamed_apps = config
            .streaming_sway_socket
            .as_ref()
            .and_then(|pattern| sway::focused_window(Path::new(&pattern.replace("{uid}", &user.uid.to_string()))))
            .map(|window| vec![sway::app_for(&window, |appid| scanner.steam_name(user.uid, &user.home, appid, ""))])
            .unwrap_or(found.streamed_apps);
        let (state, apps) = match stream {
            _ if state.counts() => (state, found.apps),
            // Anything being streamed counts, even just the Steam menus
            sunshine::Stream::Connected => (UserState::Streaming, streamed_apps),
            // Something open over there, but nobody's watching
            sunshine::Stream::Disconnected if anything_open => (UserState::StreamIdle, streamed_apps),
            // Encoder can't be checked: count games rather than miss real play
            sunshine::Stream::Unknown if game_open => (UserState::Streaming, streamed_apps),
            _ => (state, found.apps),
        };
        samples.push(UserSample { user: user.name.clone(), state, apps });
    }
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    Sample { seq, at, elapsed_secs, users: samples }
}

fn lookup_user(name: &str) -> Result<TrackedUser> {
    let passwd = std::fs::read_to_string("/etc/passwd")?;
    passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 6 && f[0] == name)
        .map(|f| TrackedUser { name: name.to_string(), uid: f[2].parse().unwrap_or(u32::MAX), home: Path::new(f[5]).to_path_buf() })
        .with_context(|| format!("user {name} not found in /etc/passwd"))
}
