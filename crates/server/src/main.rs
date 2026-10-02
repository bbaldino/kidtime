//! kidtime-server: collects agent reports and serves the dashboard.

mod db;
mod rules;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Days, Local};
use protocol::{Report, UserSample, UserState};
use serde::{Deserialize, Serialize};

const HISTORY_DAYS: u64 = 7;

/// Settings come from an optional TOML file, then `KIDTIME_*` environment
/// variables override them (the usual way to configure the container).
#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_db")]
    db: PathBuf,
    /// Shared secret agents must send as a bearer token.
    #[serde(default)]
    agent_token: String,
}

fn default_listen() -> String {
    "0.0.0.0:8470".into()
}

fn default_db() -> PathBuf {
    "/var/lib/kidtime/kidtime.db".into()
}

struct AppState {
    db: Mutex<db::Db>,
    agent_token: String,
    /// Latest sample from each host, for the live view.
    live: Mutex<HashMap<String, LiveHost>>,
}

struct LiveHost {
    received_at: i64,
    interval_secs: u32,
    users: Vec<UserSample>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(PathBuf::from(args.next().context("--config needs a path")?))
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    let config = load_config(config_path)?;

    let state = Arc::new(AppState {
        db: Mutex::new(
            db::Db::open(&config.db).with_context(|| format!("opening {}", config.db.display()))?,
        ),
        agent_token: config.agent_token,
        live: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/report", post(report))
        .route("/api/status", get(status))
        .route("/", get(|| asset("index.html")))
        .route(
            "/{file}",
            get(|axum::extract::Path(file): axum::extract::Path<String>| asset_owned(file)),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!("listening on {}", config.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn load_config(path: Option<PathBuf>) -> Result<Config> {
    // The default path is optional; an explicitly given one must exist
    let (path, required) = match path {
        Some(p) => (p, true),
        None => (PathBuf::from("/etc/kidtime/server.toml"), false),
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut config: Config =
        toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;

    if let Ok(v) = std::env::var("KIDTIME_LISTEN") {
        config.listen = v;
    }
    if let Ok(v) = std::env::var("KIDTIME_DB") {
        config.db = v.into();
    }
    if let Ok(v) = std::env::var("KIDTIME_AGENT_TOKEN") {
        config.agent_token = v;
    }
    if config.agent_token.is_empty() {
        bail!(
            "no agent token: set agent_token in {} or KIDTIME_AGENT_TOKEN",
            path.display()
        );
    }
    Ok(config)
}

/// Ctrl+C, or SIGTERM from systemd / `docker stop`.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

fn now() -> i64 {
    Local::now().timestamp()
}

async fn report(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(report): Json<Report>,
) -> Response {
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| token == state.agent_token);
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if let Err(e) = state.db.lock().unwrap().record(&report) {
        tracing::error!("recording report from {}: {e:#}", report.host);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some(latest) = report.samples.last() {
        state.live.lock().unwrap().insert(
            report.host.clone(),
            LiveHost {
                received_at: now(),
                interval_secs: report.interval_secs,
                users: latest.users.clone(),
            },
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Serialize)]
struct Status {
    generated_at: i64,
    users: Vec<UserStatus>,
}

#[derive(Serialize)]
struct UserStatus {
    user: String,
    /// The most "present" state across hosts.
    state: UserState,
    /// Host behind `state`, if any.
    host: Option<String>,
    sessions: Vec<HostSession>,
    today_secs: i64,
    week_secs: i64,
    apps_today: Vec<NamedSecs>,
    hosts_today: Vec<NamedSecs>,
    /// Oldest first, ending today.
    days: Vec<NamedSecs>,
}

#[derive(Serialize)]
struct HostSession {
    host: String,
    state: UserState,
    apps: Vec<String>,
}

#[derive(Serialize)]
struct NamedSecs {
    name: String,
    secs: i64,
}

/// Higher is more "present"; picks which host's state headlines a user.
fn presence(state: UserState) -> u8 {
    match state {
        UserState::Active => 5,
        UserState::Streaming => 4,
        UserState::StreamIdle => 3,
        UserState::Idle => 3,
        UserState::Locked => 2,
        UserState::Background => 1,
        UserState::Offline => 0,
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Result<Json<Status>, StatusCode> {
    let today = Local::now().date_naive();
    let first_day = today - Days::new(HISTORY_DAYS - 1);
    let now = now();

    // Live sessions per user, ignoring hosts that have gone quiet
    let mut sessions: HashMap<String, Vec<HostSession>> = HashMap::new();
    for (host, live) in state.live.lock().unwrap().iter() {
        let stale = now - live.received_at > i64::from(live.interval_secs) * 3;
        for u in &live.users {
            let (state, apps) = if stale {
                (UserState::Offline, Vec::new())
            } else {
                (u.state, u.apps.iter().map(|a| a.name.clone()).collect())
            };
            sessions
                .entry(u.user.clone())
                .or_default()
                .push(HostSession {
                    host: host.clone(),
                    state,
                    apps,
                });
        }
    }

    let db = state.db.lock().unwrap();
    let internal = |e: anyhow::Error| {
        tracing::error!("status query: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    };
    let mut names: Vec<String> = db.users_since(first_day).map_err(internal)?;
    names.extend(sessions.keys().cloned());
    names.sort();
    names.dedup();

    let mut users = Vec::new();
    for name in names {
        let mut user_sessions = sessions.remove(&name).unwrap_or_default();
        user_sessions.sort_by(|a, b| {
            presence(b.state)
                .cmp(&presence(a.state))
                .then(a.host.cmp(&b.host))
        });
        let (state, host) = user_sessions
            .first()
            .filter(|s| s.state != UserState::Offline)
            .map_or((UserState::Offline, None), |s| {
                (s.state, Some(s.host.clone()))
            });

        let days: Vec<NamedSecs> = db
            .daily_totals(&name, first_day, today)
            .map_err(internal)?
            .into_iter()
            .map(|(day, secs)| NamedSecs {
                name: day.to_string(),
                secs,
            })
            .collect();

        users.push(UserStatus {
            state,
            host,
            today_secs: days.last().map_or(0, |d| d.secs),
            week_secs: days.iter().map(|d| d.secs).sum(),
            apps_today: db
                .app_totals(&name, today)
                .map_err(internal)?
                .into_iter()
                .map(|a| NamedSecs {
                    name: a.name,
                    secs: a.secs,
                })
                .collect(),
            hosts_today: db
                .host_totals(&name, today)
                .map_err(internal)?
                .into_iter()
                .map(|(name, secs)| NamedSecs { name, secs })
                .collect(),
            days,
            sessions: user_sessions,
            user: name,
        });
    }
    Ok(Json(Status {
        generated_at: now,
        users,
    }))
}

async fn asset_owned(file: String) -> Response {
    asset(&file).await
}

async fn asset(file: &str) -> Response {
    let (body, content_type): (&'static [u8], &str) = match file {
        "index.html" => (
            include_bytes!("../static/index.html"),
            "text/html; charset=utf-8",
        ),
        "app.js" => (
            include_bytes!("../static/app.js"),
            "text/javascript; charset=utf-8",
        ),
        "style.css" => (
            include_bytes!("../static/style.css"),
            "text/css; charset=utf-8",
        ),
        "sw.js" => (
            include_bytes!("../static/sw.js"),
            "text/javascript; charset=utf-8",
        ),
        "manifest.webmanifest" => (
            include_bytes!("../static/manifest.webmanifest"),
            "application/manifest+json",
        ),
        "icon.svg" => (include_bytes!("../static/icon.svg"), "image/svg+xml"),
        "icon-192.png" => (include_bytes!("../static/icon-192.png"), "image/png"),
        "icon-512.png" => (include_bytes!("../static/icon-512.png"), "image/png"),
        "apple-touch-icon.png" => (
            include_bytes!("../static/apple-touch-icon.png"),
            "image/png",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // Revalidate everything; the service worker handles offline use
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
