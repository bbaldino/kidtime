//! kidtime-server: collects agent reports and serves the dashboard.

mod api;
mod auth;
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
    /// Login check: the identity provider's base URL. Set together with `access_aud`.
    #[serde(default)]
    access_team: Option<String>,
    /// Login check: the application's audience tag.
    #[serde(default)]
    access_aud: Option<String>,
}

fn default_listen() -> String {
    "0.0.0.0:8470".into()
}

fn default_db() -> PathBuf {
    "/var/lib/kidtime/kidtime.db".into()
}

pub(crate) struct AppState {
    pub(crate) db: Mutex<db::Db>,
    pub(crate) agent_token: String,
    /// Latest sample from each host, for the live view.
    pub(crate) live: Mutex<HashMap<String, LiveHost>>,
    /// None when the login check is off.
    pub(crate) auth: Option<Arc<auth::Auth>>,
}

pub(crate) struct LiveHost {
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
    let access = auth::access_config(config.access_team.clone(), config.access_aud.clone())?;
    if access.is_none() {
        tracing::warn!(
            "login check is OFF: anyone who can reach this server can read and change everything"
        );
    }
    let auth = access.map(|c| Arc::new(auth::Auth::new(c)));
    if let Some(auth) = &auth {
        // Start-up continues either way: without keys the login check answers 503, never lets anyone in
        auth.warm().await;
    }

    let state = Arc::new(AppState {
        db: Mutex::new(
            db::Db::open(&config.db).with_context(|| format!("opening {}", config.db.display()))?,
        ),
        agent_token: config.agent_token,
        live: Mutex::new(HashMap::new()),
        auth,
    });

    let pruner = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(86400));
        loop {
            tick.tick().await;
            if let Err(e) = pruner.db.lock().unwrap().prune(now()) {
                tracing::error!("pruning old rows: {e:#}");
            }
        }
    });

    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!("listening on {}", config.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Everything except the agents' endpoint and the health check sits behind the login check.
pub(crate) fn router(state: Arc<AppState>) -> Router {
    use axum::routing::{delete, put};

    let behind_login = Router::new()
        .route("/api/status", get(status))
        .route("/api/rules/{user}", get(api::get_rules))
        .route("/api/rules/{user}/copy", post(api::copy_rule))
        .route("/api/rules/{user}/{weekday}", put(api::put_rule))
        .route(
            "/api/blackouts",
            get(api::get_blackouts).post(api::post_blackout),
        )
        .route("/api/blackouts/{id}", delete(api::delete_blackout))
        .route("/api/apps", get(api::get_apps))
        .route("/api/apps/{id}", put(api::put_app))
        .route("/api/categories", get(api::get_categories))
        .route("/api/events", get(api::get_events))
        .route("/", get(|| asset("index.html")))
        .route(
            "/{file}",
            get(|axum::extract::Path(file): axum::extract::Path<String>| asset_owned(file)),
        )
        // The login check runs before any handler's extractors, so a bad body can't answer before it
        .layer(axum::middleware::from_fn_with_state(
            state.auth.clone(),
            auth::require_login,
        ));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/report", post(report))
        // Unmatched paths rely on the protected sub-router's layered fallback surviving this merge;
        // the login test in api.rs pins that
        .merge(behind_login)
        .with_state(state)
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
    if let Ok(v) = std::env::var("KIDTIME_ACCESS_TEAM") {
        config.access_team = Some(v);
    }
    if let Ok(v) = std::env::var("KIDTIME_ACCESS_AUD") {
        config.access_aud = Some(v);
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
    Json(mut report): Json<Report>,
) -> Response {
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| token == state.agent_token);
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    {
        let mut db = state.db.lock().unwrap();
        // From here on only the report as recorded is used, with its names clipped
        report = match db.record(&report) {
            Ok(recorded) => recorded,
            Err(e) => {
                tracing::error!("recording report from {}: {e:#}", report.host);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        let now_local = Local::now().naive_local();
        // Agents report every tracked account in every sample. Only the ones at a computer right
        // now have anything to lock or close.
        let mut users: Vec<&str> = report
            .samples
            .last()
            .into_iter()
            .flat_map(|s| s.users.iter().filter(|u| u.state.counts()))
            .map(|u| u.user.as_str())
            .collect();
        users.sort_unstable();
        users.dedup();
        for user in users {
            // A failure here must not make the agent resend a report that was already recorded
            let logged = db
                .decision(user, now_local)
                .and_then(|d| db.log_decision(user, now(), &d));
            if let Err(e) = logged {
                tracing::error!("logging the decision for {user}: {e:#}");
            }
        }
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
    /// Whether the account has any rule.
    restricted: bool,
    /// None if it couldn't be worked out; the other accounts are still returned.
    decision: Option<rules::Decision>,
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

        let now_local = Local::now().naive_local();
        let decision = db
            .decision(&name, now_local)
            .inspect_err(|e| tracing::error!("decision for {name}: {e:#}"))
            .ok();
        let restricted = db.is_restricted(&name).map_err(internal)?;

        users.push(UserStatus {
            restricted,
            decision,
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

const APP_JS: &[u8] = include_bytes!("../static/app.js");
const MANAGE_JS: &[u8] = include_bytes!("../static/manage.js");
const STYLE_CSS: &[u8] = include_bytes!("../static/style.css");

/// The page and the service worker with `__ASSETS__` replaced by a stamp of the scripts' and
/// stylesheet's content.
///
/// A browser or a proxy in front of the server may keep `/app.js` for longer than `no-cache` asks.
/// Naming the files `/app.js?v=<stamp>` means a page from a new release can never be paired with a
/// script from an old one.
struct Stamped {
    index_html: Vec<u8>,
    sw_js: Vec<u8>,
}

fn stamped() -> &'static Stamped {
    static STAMPED: std::sync::OnceLock<Stamped> = std::sync::OnceLock::new();
    STAMPED.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        // Not for security: it only has to change when a file changes
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (APP_JS, MANAGE_JS, STYLE_CSS).hash(&mut hasher);
        let stamp = format!("{:016x}", hasher.finish());
        let fill = |text: &str| text.replace("__ASSETS__", &stamp).into_bytes();
        Stamped {
            index_html: fill(include_str!("../static/index.html")),
            sw_js: fill(include_str!("../static/sw.js")),
        }
    })
}

async fn asset(file: &str) -> Response {
    let (body, content_type): (&'static [u8], &str) = match file {
        "index.html" => (&stamped().index_html, "text/html; charset=utf-8"),
        "app.js" => (APP_JS, "text/javascript; charset=utf-8"),
        "manage.js" => (MANAGE_JS, "text/javascript; charset=utf-8"),
        "style.css" => (STYLE_CSS, "text/css; charset=utf-8"),
        "sw.js" => (&stamped().sw_js, "text/javascript; charset=utf-8"),
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
