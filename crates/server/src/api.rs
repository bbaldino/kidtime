//! JSON handlers for rules, blackouts, the app catalogue and the would-have log.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{Local, NaiveDateTime};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::rules::{self, CategoryId, DayRule, Invalid};

pub enum ApiError {
    NotFound,
    Invalid(Invalid),
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => StatusCode::NOT_FOUND.into_response(),
            ApiError::Invalid(i) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": i.message, "field": i.field })),
            )
                .into_response(),
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("api: {e:#}");
        ApiError::Internal
    }
}

impl From<Invalid> for ApiError {
    fn from(i: Invalid) -> Self {
        ApiError::Invalid(i)
    }
}

fn invalid(field: &'static str, message: &str) -> ApiError {
    ApiError::Invalid(Invalid {
        field,
        message: message.into(),
    })
}

type Api<T> = Result<T, ApiError>;

fn check_weekday(weekday: u8) -> Api<()> {
    if weekday > 6 {
        Err(invalid(
            "weekday",
            "weekday must be 0 (Monday) to 6 (Sunday)",
        ))
    } else {
        Ok(())
    }
}

pub async fn get_rules(
    State(state): State<Arc<AppState>>,
    Path(user): Path<String>,
) -> Api<Json<serde_json::Value>> {
    let db = state.db.lock().unwrap();
    if !db.is_account(&user)? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(json!({ "user": user, "days": db.week_rules(&user)? })))
}

pub async fn put_rule(
    State(state): State<Arc<AppState>>,
    Path((user, weekday)): Path<(String, u8)>,
    Json(rule): Json<DayRule>,
) -> Api<StatusCode> {
    check_weekday(weekday)?;
    rules::validate_day(&rule)?;
    let mut db = state.db.lock().unwrap();
    if !db.is_account(&user)? {
        return Err(ApiError::NotFound);
    }
    let known: Vec<CategoryId> = db.categories()?.into_iter().map(|(id, _)| id).collect();
    if rule.budgets.keys().any(|id| !known.contains(id)) {
        return Err(invalid("budgets", "unknown category"));
    }
    db.set_day_rule(&user, weekday, &rule)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct CopyRule {
    from: u8,
    to: Vec<u8>,
}

pub async fn copy_rule(
    State(state): State<Arc<AppState>>,
    Path(user): Path<String>,
    Json(copy): Json<CopyRule>,
) -> Api<StatusCode> {
    if copy.from > 6 {
        return Err(invalid("from", "weekday must be 0 (Monday) to 6 (Sunday)"));
    }
    if copy.to.iter().any(|&d| d > 6) {
        return Err(invalid("to", "weekday must be 0 (Monday) to 6 (Sunday)"));
    }
    let mut db = state.db.lock().unwrap();
    if !db.is_account(&user)? {
        return Err(ApiError::NotFound);
    }
    let rule = db.day_rule(&user, copy.from)?;
    for weekday in copy.to {
        db.set_day_rule(&user, weekday, &rule)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_blackouts(
    State(state): State<Arc<AppState>>,
) -> Api<Json<Vec<crate::db::Blackout>>> {
    Ok(Json(
        state
            .db
            .lock()
            .unwrap()
            .blackouts(Local::now().naive_local())?,
    ))
}

#[derive(Deserialize)]
pub struct NewBlackout {
    user: Option<String>,
    start: String,
    end: String,
    #[serde(default)]
    note: String,
}

/// What `<input type="datetime-local">` sends, with or without seconds.
fn parse_local(field: &'static str, text: &str) -> Api<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .map_err(|_| invalid(field, "not a date and time"))
}

pub async fn post_blackout(
    State(state): State<Arc<AppState>>,
    Json(new): Json<NewBlackout>,
) -> Api<Response> {
    let start = parse_local("start", &new.start)?;
    let end = parse_local("end", &new.end)?;
    rules::validate_blackout(start, end)?;
    // One that has already ended would never be listed, so it could never be deleted either
    if end <= Local::now().naive_local() {
        return Err(invalid("end", "a blackout must end in the future"));
    }
    let mut db = state.db.lock().unwrap();
    if let Some(user) = &new.user
        && !db.is_account(user)?
    {
        return Err(invalid("user", "unknown account"));
    }
    let id = db.add_blackout(new.user.as_deref(), start, end, new.note.trim())?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

pub async fn delete_blackout(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Api<StatusCode> {
    if state.db.lock().unwrap().delete_blackout(id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

pub async fn get_apps(State(state): State<Arc<AppState>>) -> Api<Json<Vec<crate::db::AppEntry>>> {
    Ok(Json(state.db.lock().unwrap().apps()?))
}

#[derive(Deserialize)]
pub struct SetCategory {
    category_id: Option<CategoryId>,
}

pub async fn put_app(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(set): Json<SetCategory>,
) -> Api<StatusCode> {
    let mut db = state.db.lock().unwrap();
    if let Some(id) = set.category_id
        && !db.categories()?.iter().any(|(known, _)| *known == id)
    {
        return Err(invalid("category_id", "unknown category"));
    }
    if db.set_app_category(&app_id, set.category_id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

pub async fn get_categories(State(state): State<Arc<AppState>>) -> Api<Json<serde_json::Value>> {
    let categories = state.db.lock().unwrap().categories()?;
    Ok(Json(
        categories
            .into_iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct EventQuery {
    user: Option<String>,
}

pub async fn get_events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventQuery>,
) -> Api<Json<Vec<crate::db::Event>>> {
    Ok(Json(
        state.db.lock().unwrap().events(query.user.as_deref(), 50)?,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::{AppState, db, router};

    struct TestApp {
        state: Arc<AppState>,
        path: std::path::PathBuf,
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn app(name: &str) -> TestApp {
        let path =
            std::env::temp_dir().join(format!("kidtime-api-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let state = Arc::new(AppState {
            db: Mutex::new(db::Db::open(&path).unwrap()),
            agent_token: "token".into(),
            live: Mutex::new(HashMap::new()),
            auth: None,
        });
        TestApp { state, path }
    }

    async fn call(
        app: &TestApp,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = router(app.state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Makes `kid1` a known account with one app in the catalogue.
    async fn report(app: &TestApp, app_id: &str) {
        report_as(app, app_id, "active").await;
    }

    async fn report_as(app: &TestApp, app_id: &str, state: &str) {
        let at = chrono::Local::now().timestamp();
        let body = json!({ "host": "host-a", "agent_id": "a", "interval_secs": 15, "samples": [{
            "seq": 1, "at": at, "elapsed_secs": 15,
            "users": [{ "user": "kid1", "state": state, "apps": [{ "id": app_id, "name": "Some App" }] }],
        }]});
        let request = Request::builder()
            .method("POST")
            .uri("/api/report")
            .header("content-type", "application/json")
            .header("authorization", "Bearer token")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(app.state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn rules_round_trip_and_copy() {
        let app = app("rules");
        report(&app, "steam:1").await;
        let rule = json!({ "restricted": true, "stretches": [{ "start_min": 375, "end_min": 1200 }], "budgets": { "1": 60 } });
        assert_eq!(
            call(&app, "PUT", "/api/rules/kid1/0", Some(rule.clone()))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let copy = json!({ "from": 0, "to": [1, 4] });
        assert_eq!(
            call(&app, "POST", "/api/rules/kid1/copy", Some(copy))
                .await
                .0,
            StatusCode::NO_CONTENT
        );

        let (status, body) = call(&app, "GET", "/api/rules/kid1", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["days"].as_array().unwrap().len(), 7);
        assert_eq!(body["days"][0], rule);
        assert_eq!(body["days"][4], rule);
        assert_eq!(body["days"][2]["restricted"], false);
    }

    #[tokio::test]
    async fn rules_reject_bad_input() {
        let app = app("rules-bad");
        report(&app, "steam:1").await;
        let backwards =
            json!({ "restricted": true, "stretches": [{ "start_min": 700, "end_min": 600 }] });
        let (status, body) = call(&app, "PUT", "/api/rules/kid1/0", Some(backwards)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["field"], "stretches");
        assert!(body["error"].as_str().unwrap().contains("end after"));

        let ok = json!({ "restricted": false });
        assert_eq!(
            call(&app, "PUT", "/api/rules/kid1/7", Some(ok.clone()))
                .await
                .1["field"],
            "weekday"
        );
        assert_eq!(
            call(&app, "PUT", "/api/rules/nobody/0", Some(ok.clone()))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let unknown_category = json!({ "restricted": false, "budgets": { "99": 10 } });
        assert_eq!(
            call(&app, "PUT", "/api/rules/kid1/0", Some(unknown_category))
                .await
                .1["field"],
            "budgets"
        );
        let bad_copy = json!({ "from": 0, "to": [9] });
        assert_eq!(
            call(&app, "POST", "/api/rules/kid1/copy", Some(bad_copy))
                .await
                .1["field"],
            "to"
        );
    }

    #[tokio::test]
    async fn blackouts_add_list_delete() {
        let app = app("blackouts");
        report(&app, "steam:1").await;
        let body = json!({ "user": "kid1", "start": "2099-01-01T17:00", "end": "2099-01-01T19:00", "note": "dinner" });
        let (status, created) = call(&app, "POST", "/api/blackouts", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = created["id"].as_i64().unwrap();

        let (_, list) = call(&app, "GET", "/api/blackouts", None).await;
        assert_eq!(list[0]["note"], "dinner");
        assert_eq!(list[0]["start"], "2099-01-01T17:00:00");

        let backwards = json!({ "user": null, "start": "2099-01-01T19:00", "end": "2099-01-01T17:00", "note": "" });
        assert_eq!(
            call(&app, "POST", "/api/blackouts", Some(backwards))
                .await
                .1["field"],
            "end"
        );
        let unknown = json!({ "user": "nobody", "start": "2099-01-01T17:00", "end": "2099-01-01T19:00", "note": "" });
        assert_eq!(
            call(&app, "POST", "/api/blackouts", Some(unknown)).await.1["field"],
            "user"
        );
        let garbled =
            json!({ "user": null, "start": "tomorrow", "end": "2099-01-01T19:00", "note": "" });
        assert_eq!(
            call(&app, "POST", "/api/blackouts", Some(garbled)).await.1["field"],
            "start"
        );

        assert_eq!(
            call(&app, "DELETE", &format!("/api/blackouts/{id}"), None)
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, "DELETE", &format!("/api/blackouts/{id}"), None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn apps_can_be_categorised_whatever_their_id() {
        let app = app("apps");
        let id = "window:A/B: Game? 100%";
        report(&app, id).await;
        let (_, list) = call(&app, "GET", "/api/apps", None).await;
        assert_eq!(list[0]["app_id"], id);
        assert_eq!(list[0]["reviewed"], false);

        // Percent-encoded the way encodeURIComponent does it
        let uri = "/api/apps/window%3AA%2FB%3A%20Game%3F%20100%25";
        assert_eq!(
            call(&app, "PUT", uri, Some(json!({ "category_id": 1 })))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let (_, list) = call(&app, "GET", "/api/apps", None).await;
        assert_eq!(list[0]["category_id"], 1);
        assert_eq!(list[0]["reviewed"], true);

        assert_eq!(
            call(
                &app,
                "PUT",
                "/api/apps/nope",
                Some(json!({ "category_id": null }))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, "PUT", uri, Some(json!({ "category_id": 99 })))
                .await
                .1["field"],
            "category_id"
        );
        let (_, categories) = call(&app, "GET", "/api/categories", None).await;
        assert_eq!(categories, json!([{ "id": 1, "name": "Games" }]));
    }

    #[tokio::test]
    async fn status_carries_the_decision_and_reports_log_events() {
        let app = app("status");
        report(&app, "steam:1").await;
        let (_, status) = call(&app, "GET", "/api/status", None).await;
        assert_eq!(status["users"][0]["restricted"], false);
        assert_eq!(
            status["users"][0]["decision"]["computer"]["state"],
            "allowed"
        );

        // No stretches on any day: never allowed
        for weekday in 0..7 {
            let rule = json!({ "restricted": true, "stretches": [] });
            call(
                &app,
                "PUT",
                &format!("/api/rules/kid1/{weekday}"),
                Some(rule),
            )
            .await;
        }
        let (_, status) = call(&app, "GET", "/api/status", None).await;
        assert_eq!(status["users"][0]["restricted"], true);
        assert_eq!(
            status["users"][0]["decision"]["computer"]["state"],
            "outside_schedule"
        );

        // The next report notices the change
        report(&app, "steam:1").await;
        let (_, events) = call(&app, "GET", "/api/events?user=kid1", None).await;
        assert_eq!(events[0]["kind"], "locked");
        assert_eq!(events[0]["detail"], "outside schedule");
        assert_eq!(
            call(&app, "GET", "/api/events", None)
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn events_are_logged_only_for_accounts_in_use() {
        let app = app("in-use");
        // The first report makes the account known, so rules can be set for it
        report_as(&app, "steam:1", "offline").await;
        for weekday in 0..7 {
            let rule = json!({ "restricted": true, "stretches": [] });
            let uri = format!("/api/rules/kid1/{weekday}");
            assert_eq!(
                call(&app, "PUT", &uri, Some(rule)).await.0,
                StatusCode::NO_CONTENT
            );
        }

        // Nobody is at the computer: there is nothing to lock
        report_as(&app, "steam:1", "offline").await;
        report_as(&app, "steam:1", "background").await;
        let (_, events) = call(&app, "GET", "/api/events", None).await;
        assert_eq!(events, json!([]));

        report_as(&app, "steam:1", "active").await;
        let (_, events) = call(&app, "GET", "/api/events", None).await;
        assert_eq!(events.as_array().unwrap().len(), 1);
        assert_eq!(events[0]["kind"], "locked");
        assert_eq!(events[0]["user"], "kid1");
    }

    #[tokio::test]
    async fn a_blackout_that_has_already_ended_is_rejected() {
        let app = app("blackout-past");
        report(&app, "steam:1").await;
        let past = json!({ "user": "kid1", "start": "2001-01-01T17:00", "end": "2001-01-01T19:00", "note": "" });
        let (status, body) = call(&app, "POST", "/api/blackouts", Some(past)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["field"], "end");
        assert_eq!(body["error"], "a blackout must end in the future");
    }

    #[tokio::test]
    async fn with_the_login_check_on_only_report_and_health_are_open() {
        use crate::auth::{AccessConfig, Auth};
        let mut app = app("login");
        let keys = serde_json::from_value(json!({ "keys": [] })).unwrap();
        let auth = Auth::with_keys(
            AccessConfig {
                team: "https://team.example.com".into(),
                aud: "test-aud".into(),
            },
            keys,
        );
        Arc::get_mut(&mut app.state).unwrap().auth = Some(Arc::new(auth));

        for (method, uri) in [
            ("GET", "/"),
            ("GET", "/app.js"),
            ("GET", "/manage.js"),
            ("GET", "/manifest.webmanifest"),
            ("GET", "/api/status"),
            ("GET", "/api/rules/kid1"),
            ("PUT", "/api/rules/kid1/0"),
            ("POST", "/api/rules/kid1/copy"),
            ("GET", "/api/blackouts"),
            ("POST", "/api/blackouts"),
            ("DELETE", "/api/blackouts/1"),
            ("GET", "/api/apps"),
            ("PUT", "/api/apps/x"),
            ("GET", "/api/categories"),
            ("GET", "/api/events"),
            // Unmatched paths and wrong methods must not reveal anything either
            ("GET", "/api/nope"),
            ("GET", "/a/b/c"),
            ("POST", "/api/status"),
            ("DELETE", "/"),
            ("HEAD", "/api/status"),
            ("OPTIONS", "/api/status"),
        ] {
            let (status, body) = call(&app, method, uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
            assert_eq!(body, Value::Null, "{method} {uri} must have an empty body");
        }
        assert_eq!(call(&app, "GET", "/healthz", None).await.0, StatusCode::OK);
        // The open paths answer other methods with 405 and an empty body
        for (method, uri) in [("GET", "/api/report"), ("POST", "/healthz")] {
            let (status, body) = call(&app, method, uri, None).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {uri}");
            assert_eq!(body, Value::Null, "{method} {uri} must have an empty body");
        }
        report(&app, "steam:1").await;
    }
}
