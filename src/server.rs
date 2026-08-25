use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use axum::{
    extract::{ConnectInfo, Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use include_dir::{include_dir, Dir};
use serde_json::json;
use tokio::net::TcpListener;

use crate::{
    agent_telemetry::{query_agent_usage, AgentUsageQuery},
    app::{AppQueries, GroupsQuery, ProcessesQuery, SeriesQuery, SharedApp},
    browser_api::{self, ClientConfigInput, EventBatch, InterventionInput, PairInput},
    charge_control::ChargeTypeInput,
    context_api::{
        self, AliasInput, DeletionInput, EntitySplitInput, ExportInput, ManualLabelInput,
        MergeInput, OverrideInput, PolicyInput, RangeQuery, RelabelInput, RelationInput, RuleInput,
    },
};

static PUBLIC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/public");
static CONTEXT_DB_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
static BROWSER_DB_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) async fn serve(shared: SharedApp) -> Result<()> {
    let (host, port) = {
        let app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        (app.cfg.host.clone(), app.cfg.port)
    };
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let router = Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/assets/*path", get(static_asset))
        .route("/api/status", get(api_status_handler))
        .route("/api/series", get(api_series_handler))
        .route("/api/groups", get(api_groups_handler))
        .route("/api/processes", get(api_processes_handler))
        .route(
            "/api/battery/charge-type",
            get(api_charge_type_handler).post(api_set_charge_type_handler),
        )
        .route("/api/context/latest", get(api_context_latest_handler))
        .route("/api/activity/summary", get(api_activity_summary_handler))
        .route("/api/activity/timeline", get(api_activity_timeline_handler))
        .route(
            "/api/activity/project-breaks",
            get(api_project_breaks_handler),
        )
        .route("/api/activity/episodes", get(api_activity_episodes_handler))
        .route("/api/context/unknown", get(api_context_unknown_handler))
        .route("/api/context/entities", get(api_context_entities_handler))
        .route(
            "/api/context/rules",
            get(api_context_rules_handler).post(api_context_create_rule_handler),
        )
        .route(
            "/api/context/rules/preview",
            axum::routing::post(api_context_rule_preview_handler),
        )
        .route(
            "/api/context/rules/:id",
            axum::routing::delete(api_context_delete_rule_handler),
        )
        .route(
            "/api/context/manual-labels",
            get(api_context_manual_labels_handler).post(api_context_manual_label_handler),
        )
        .route(
            "/api/context/manual-labels/:id",
            axum::routing::delete(api_context_delete_manual_label_handler),
        )
        .route(
            "/api/context/relabel",
            get(api_context_relabel_status_handler).post(api_context_relabel_handler),
        )
        .route(
            "/api/context/entities/aliases",
            axum::routing::post(api_context_alias_handler),
        )
        .route(
            "/api/context/entities/:id/aliases",
            axum::routing::post(api_context_alias_path_handler),
        )
        .route(
            "/api/context/entities/relations",
            axum::routing::post(api_context_relation_handler),
        )
        .route(
            "/api/context/entities/merge",
            axum::routing::post(api_context_merge_handler),
        )
        .route(
            "/api/context/entities/:id/split",
            axum::routing::post(api_context_split_handler),
        )
        .route(
            "/api/context/entities/:id/archive",
            axum::routing::post(api_context_archive_handler),
        )
        .route(
            "/api/context/entities/:id",
            axum::routing::delete(api_context_delete_entity_handler),
        )
        .route(
            "/api/context/export",
            get(api_context_export_handler).post(api_context_export_post_handler),
        )
        .route(
            "/api/context/deletions",
            axum::routing::post(api_context_deletion_handler),
        )
        .route("/api/context/storage", get(api_context_storage_handler))
        .route("/api/context/source-health", get(api_source_health_handler))
        .route("/api/context/health", get(api_source_health_handler))
        .route(
            "/api/attention/policies",
            get(api_policies_handler).post(api_create_policy_handler),
        )
        .route(
            "/api/attention/policies/:id",
            axum::routing::put(api_update_policy_handler),
        )
        .route(
            "/api/attention/policies/:id/actions",
            axum::routing::post(api_policy_action_handler),
        )
        .route("/api/interventions", get(api_interventions_handler))
        .route(
            "/api/attention/overrides",
            axum::routing::post(api_create_override_handler),
        )
        .route(
            "/api/integrations/browser/v1/pair-code",
            axum::routing::post(api_browser_pair_code_handler),
        )
        .route(
            "/api/integrations/browser/v1/pair",
            axum::routing::post(api_browser_pair_handler).options(api_browser_options_handler),
        )
        .route(
            "/api/integrations/browser/v1/events:batch",
            axum::routing::post(api_browser_events_handler).options(api_browser_options_handler),
        )
        .route(
            "/api/integrations/browser/v1/config",
            get(api_browser_config_handler)
                .post(api_browser_config_update_handler)
                .options(api_browser_options_handler),
        )
        .route(
            "/api/integrations/browser/v1/policy-state",
            get(api_browser_policy_state_handler).options(api_browser_options_handler),
        )
        .route(
            "/api/integrations/browser/v1/interventions",
            axum::routing::post(api_browser_intervention_handler)
                .options(api_browser_options_handler),
        )
        .route(
            "/api/integrations/browser/v1/client",
            axum::routing::delete(api_browser_revoke_handler).options(api_browser_options_handler),
        )
        .route(
            "/api/integrations/browser/v1/clients",
            get(api_browser_clients_handler),
        )
        .route("/api/agents/usage", get(api_agent_usage_handler))
        .layer(axum::extract::DefaultBodyLimit::max(128 * 1024))
        .with_state(shared);
    let listener = TcpListener::bind(addr).await?;
    println!("[bms-watchdog] UI: http://{}", listener.local_addr()?);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

async fn root() -> impl IntoResponse {
    Html(
        PUBLIC_DIR
            .get_file("index.html")
            .and_then(|f| f.contents_utf8())
            .unwrap_or("not found")
            .to_string(),
    )
}

async fn healthz() -> impl IntoResponse {
    "ok\n"
}

async fn static_asset(AxumPath(path): AxumPath<String>) -> Response {
    let Some(file) = PUBLIC_DIR.get_file(format!("assets/{path}")) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    let mut resp = file.contents().to_vec().into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

async fn api_status_handler(State(shared): State<SharedApp>) -> Response {
    with_dashboard_app(shared, |app| app.api_status()).await
}
async fn api_series_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<SeriesQuery>,
) -> Response {
    with_dashboard_app(shared, move |app| app.api_series(q)).await
}
async fn api_groups_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<GroupsQuery>,
) -> Response {
    with_dashboard_app(shared, move |app| app.api_groups(q)).await
}
async fn api_processes_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<ProcessesQuery>,
) -> Response {
    with_dashboard_app(shared, move |app| app.api_processes(q)).await
}
async fn api_context_latest_handler(State(shared): State<SharedApp>) -> Response {
    with_context_db(shared, |db| {
        crate::context::latest_context_json(db)
            .map(|latest| json!({"trackingEnabled":true,"latest":latest}))
    })
    .await
}

async fn api_charge_type_handler(State(shared): State<SharedApp>) -> Response {
    let cfg = match shared.lock() {
        Ok(app) => app.cfg.clone(),
        Err(_) => return state_lock_error(),
    };
    match tokio::task::spawn_blocking(move || crate::charge_control::status_json(&cfg)).await {
        Ok(value) => Json(value).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"charge-type task failed"})),
        )
            .into_response(),
    }
}

async fn api_set_charge_type_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<ChargeTypeInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    let cfg = match shared.lock() {
        Ok(app) => app.cfg.clone(),
        Err(_) => return state_lock_error(),
    };
    match tokio::task::spawn_blocking(move || crate::charge_control::apply(&cfg, &input.value)).await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"charge-type task failed"})),
        )
            .into_response(),
    }
}

fn state_lock_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"state lock poisoned"})),
    )
        .into_response()
}

async fn with_context_db<F>(shared: SharedApp, operation: F) -> Response
where
    F: FnOnce(&mut rusqlite::Connection) -> Result<serde_json::Value> + Send + 'static,
{
    let path = match shared.lock() {
        Ok(app) => app.db_path.clone(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"state lock poisoned"})),
            )
                .into_response()
        }
    };
    match tokio::task::spawn_blocking(move || {
        let _gate = CONTEXT_DB_GATE
            .lock()
            .map_err(|_| anyhow!("context database gate poisoned"))?;
        let mut db = crate::db::open_request_connection(&path, false)?;
        operation(&mut db)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"database task failed"})),
        )
            .into_response(),
    }
}

async fn with_dashboard_app<F>(shared: SharedApp, operation: F) -> Response
where
    F: FnOnce(&AppQueries) -> Result<serde_json::Value> + Send + 'static,
{
    let (cfg, path) = match shared.lock() {
        Ok(app) => (app.cfg.clone(), app.db_path.clone()),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"state lock poisoned"})),
            )
                .into_response()
        }
    };
    match tokio::task::spawn_blocking(move || {
        let queries = AppQueries::open(cfg, path)?;
        operation(&queries)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"database task failed"})),
        )
            .into_response(),
    }
}

async fn api_activity_summary_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<RangeQuery>,
) -> Response {
    with_context_db(shared, move |db| context_api::activity_summary(db, &q)).await
}
async fn api_activity_timeline_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<RangeQuery>,
) -> Response {
    with_context_db(shared, move |db| context_api::activity_timeline(db, &q)).await
}
async fn api_project_breaks_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<RangeQuery>,
) -> Response {
    with_context_db(shared, move |db| context_api::project_breaks(db, &q)).await
}
async fn api_activity_episodes_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<RangeQuery>,
) -> Response {
    with_context_db(shared, move |db| context_api::activity_episodes(db, &q)).await
}
async fn api_context_unknown_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<RangeQuery>,
) -> Response {
    with_context_db(shared, move |db| context_api::unknown_context(db, &q)).await
}
#[derive(serde::Deserialize)]
struct EntityQuery {
    kind: Option<String>,
}
async fn api_context_entities_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<EntityQuery>,
) -> Response {
    with_context_db(shared, move |db| {
        context_api::entities(db, q.kind.as_deref())
    })
    .await
}
async fn api_context_rules_handler(State(shared): State<SharedApp>) -> Response {
    with_context_db(shared, context_api::rules).await
}
async fn api_context_relabel_status_handler(State(shared): State<SharedApp>) -> Response {
    with_context_db(shared, |db| context_api::relabel_status(db)).await
}
async fn api_context_storage_handler(State(shared): State<SharedApp>) -> Response {
    with_context_db(shared, context_api::storage_metrics).await
}
async fn api_source_health_handler(State(shared): State<SharedApp>) -> Response {
    with_context_db(shared, |db| context_api::source_health(db)).await
}
async fn api_context_export_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<RangeQuery>,
) -> Response {
    let input = ExportInput {
        kind: q.kind.unwrap_or_else(|| "all".into()),
        format: q.format.unwrap_or_else(|| "json".into()),
        from: q.from,
        to: q.to,
    };
    with_context_db(shared, move |db| context_api::export_context(db, &input)).await
}
async fn api_policies_handler(State(shared): State<SharedApp>) -> Response {
    with_context_db(shared, |db| context_api::policies(db)).await
}

fn reject_untrusted_write(shared: &SharedApp, headers: &HeaderMap) -> Option<Response> {
    let configured_port = shared.lock().ok().map(|app| app.cfg.port);
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let csrf = headers
        .get("x-watchdog-csrf")
        .and_then(|value| value.to_str().ok())
        == Some("1");
    let expected_origin = format!("http://{host}");
    let port_matches = configured_port.is_some_and(|port| {
        host.rsplit_once(':')
            .and_then(|(_, value)| value.parse::<u16>().ok())
            == Some(port)
    });
    if !csrf || host.is_empty() || origin != expected_origin || !port_matches {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"same-origin dashboard request with CSRF header required"})),
            )
                .into_response(),
        )
    } else {
        None
    }
}

async fn api_context_rule_preview_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<RuleInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::preview_rule(db, &input)).await
}
async fn api_context_create_rule_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<RuleInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::create_rule(db, input)).await
}
async fn api_context_delete_rule_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::delete_rule(db, id)).await
}
#[derive(serde::Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
}
async fn api_context_manual_labels_handler(
    State(shared): State<SharedApp>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    with_context_db(shared, move |db| {
        context_api::manual_label_history(db, query.limit.unwrap_or(100))
    })
    .await
}
async fn api_context_manual_label_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<ManualLabelInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| {
        context_api::create_manual_label(db, input)
    })
    .await
}
async fn api_context_delete_manual_label_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::delete_manual_label(db, id)).await
}
async fn api_context_relabel_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<RelabelInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::create_relabel(db, input)).await
}
async fn api_context_alias_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<AliasInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::create_alias(db, input)).await
}
#[derive(serde::Deserialize)]
struct AliasBody {
    #[serde(rename = "aliasKind")]
    alias_kind: String,
    #[serde(rename = "aliasValue")]
    alias_value: String,
    confidence: Option<f64>,
}
async fn api_context_alias_path_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<AliasBody>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| {
        context_api::create_alias(
            db,
            AliasInput {
                entity_id: id,
                alias_kind: input.alias_kind,
                alias_value: input.alias_value,
                confidence: input.confidence,
            },
        )
    })
    .await
}
async fn api_context_relation_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<RelationInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::create_relation(db, input)).await
}
async fn api_context_merge_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<MergeInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::merge_entity(db, input)).await
}
async fn api_context_split_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<EntitySplitInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::split_entity(db, id, input)).await
}
async fn api_context_archive_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::archive_entity(db, id)).await
}
async fn api_context_delete_entity_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::delete_entity(db, id)).await
}
async fn api_create_policy_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<PolicyInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::create_policy(db, input)).await
}
async fn api_update_policy_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<PolicyInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::update_policy(db, id, input)).await
}
#[derive(serde::Deserialize)]
struct PolicyAction {
    action: String,
    seconds: i64,
}
async fn api_policy_action_handler(
    AxumPath(id): AxumPath<i64>,
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<PolicyAction>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| {
        context_api::create_override(
            db,
            OverrideInput {
                policy_id: id,
                client_id: None,
                action: input.action,
                duration_seconds: input.seconds,
            },
        )
    })
    .await
}
async fn api_interventions_handler(
    State(shared): State<SharedApp>,
    Query(query): Query<RangeQuery>,
) -> Response {
    with_context_db(shared, move |db| context_api::interventions(db, &query)).await
}
async fn api_context_export_post_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<ExportInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::export_context(db, &input)).await
}
async fn api_context_deletion_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<DeletionInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| {
        context_api::delete_context_data(db, input)
    })
    .await
}
async fn api_create_override_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
    Json(input): Json<OverrideInput>,
) -> Response {
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    with_context_db(shared, move |db| context_api::create_override(db, input)).await
}

enum BrowserTaskError {
    Request(anyhow::Error),
    Internal(anyhow::Error),
}

async fn with_browser_db<F>(
    shared: SharedApp,
    operation: F,
) -> std::result::Result<serde_json::Value, BrowserTaskError>
where
    F: FnOnce(&mut rusqlite::Connection) -> Result<serde_json::Value> + Send + 'static,
{
    let path = {
        let app = shared
            .lock()
            .map_err(|_| BrowserTaskError::Internal(anyhow!("state lock poisoned")))?;
        if !app.cfg.browser_integration {
            return Err(BrowserTaskError::Request(anyhow!(
                "browser integration disabled"
            )));
        }
        app.db_path.clone()
    };
    match tokio::task::spawn_blocking(move || {
        let _gate = BROWSER_DB_GATE
            .lock()
            .map_err(|_| BrowserTaskError::Internal(anyhow!("browser database gate poisoned")))?;
        let mut db =
            crate::db::open_request_connection(&path, false).map_err(BrowserTaskError::Internal)?;
        operation(&mut db).map_err(BrowserTaskError::Request)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(BrowserTaskError::Internal(anyhow!(
            "browser database task failed"
        ))),
    }
}
fn request_origin(headers: &HeaderMap) -> &str {
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}
fn bearer(headers: &HeaderMap) -> Result<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| v.len() <= 128)
        .ok_or_else(|| anyhow!("bearer token required"))
}
trait BrowserResponseResult {
    fn into_response_result(self) -> std::result::Result<serde_json::Value, (StatusCode, String)>;
}

impl BrowserResponseResult for Result<serde_json::Value> {
    fn into_response_result(self) -> std::result::Result<serde_json::Value, (StatusCode, String)> {
        self.map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))
    }
}

impl BrowserResponseResult for std::result::Result<serde_json::Value, BrowserTaskError> {
    fn into_response_result(self) -> std::result::Result<serde_json::Value, (StatusCode, String)> {
        self.map_err(|error| match error {
            BrowserTaskError::Request(error) => (StatusCode::BAD_REQUEST, error.to_string()),
            BrowserTaskError::Internal(error) => {
                (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        })
    }
}

fn browser_response(result: impl BrowserResponseResult, origin: &str) -> Response {
    let mut response = match result.into_response_result() {
        Ok(value) => Json(value).into_response(),
        Err((status, error)) => (status, Json(json!({"error":error}))).into_response(),
    };
    if browser_api::extension_origin(origin) {
        if let Ok(value) = HeaderValue::from_str(origin) {
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
            response
                .headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Origin"));
        }
    }
    response
}
async fn api_browser_options_handler(headers: HeaderMap) -> Response {
    let origin = request_origin(&headers);
    if !browser_api::extension_origin(origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(origin).unwrap(),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET,POST,DELETE,OPTIONS"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization,content-type"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}
async fn api_browser_pair_code_handler(
    State(shared): State<SharedApp>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = reject_untrusted_write(&shared, &headers) {
        return r;
    }
    browser_response(
        with_browser_db(shared, |db| browser_api::create_pairing_code(db)).await,
        "",
    )
}
async fn api_browser_pair_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<serde_json::Value>,
) -> Response {
    let origin = request_origin(&headers);
    if input["action"].as_str() == Some("create_code") {
        if let Some(r) = reject_untrusted_write(&shared, &headers) {
            return r;
        }
        return browser_response(
            with_browser_db(shared, |db| browser_api::create_pairing_code(db)).await,
            "",
        );
    }
    let input = match serde_json::from_value::<PairInput>(input) {
        Ok(input) => input,
        Err(_) => return browser_response(Err(anyhow!("invalid pair request")), origin),
    };
    let origin = origin.to_string();
    let response_origin = origin.clone();
    browser_response(
        with_browser_db(shared, move |db| {
            browser_api::pair(db, input, &origin, peer.ip())
        })
        .await,
        &response_origin,
    )
}
async fn api_browser_events_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<EventBatch>,
) -> Response {
    let origin = request_origin(&headers);
    let token = match bearer(&headers) {
        Ok(v) => v,
        Err(err) => return browser_response(Err(err), origin),
    };
    let origin = origin.to_string();
    let response_origin = origin.clone();
    let token = token.to_string();
    browser_response(
        with_browser_db(shared, move |db| {
            browser_api::ingest(db, &token, &origin, peer.ip(), input)
        })
        .await,
        &response_origin,
    )
}
async fn api_browser_config_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let token = match bearer(&headers) {
        Ok(v) => v,
        Err(err) => return browser_response(Err(err), origin),
    };
    let origin = origin.to_string();
    let response_origin = origin.clone();
    let token = token.to_string();
    browser_response(
        with_browser_db(shared, move |db| {
            browser_api::client_config(db, &token, &origin, peer.ip())
        })
        .await,
        &response_origin,
    )
}
async fn api_browser_config_update_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<ClientConfigInput>,
) -> Response {
    let origin = request_origin(&headers);
    let token = match bearer(&headers) {
        Ok(value) => value,
        Err(err) => return browser_response(Err(err), origin),
    };
    let origin = origin.to_string();
    let response_origin = origin.clone();
    let token = token.to_string();
    browser_response(
        with_browser_db(shared, move |db| {
            browser_api::update_client_config(db, &token, &origin, peer.ip(), input)
        })
        .await,
        &response_origin,
    )
}
#[derive(serde::Deserialize)]
struct BrowserPolicyQuery {
    domain: Option<String>,
}
async fn api_browser_policy_state_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<BrowserPolicyQuery>,
) -> Response {
    let origin = request_origin(&headers);
    let token = match bearer(&headers) {
        Ok(v) => v,
        Err(err) => return browser_response(Err(err), origin),
    };
    let origin = origin.to_string();
    let response_origin = origin.clone();
    let token = token.to_string();
    browser_response(
        with_browser_db(shared, move |db| {
            browser_api::policy_state(db, &token, &origin, peer.ip(), query.domain.as_deref())
        })
        .await,
        &response_origin,
    )
}
async fn api_browser_revoke_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    input: Option<Json<serde_json::Value>>,
) -> Response {
    let origin = request_origin(&headers);
    if let Some(Json(input)) = input {
        if let Some(client_id) = input["clientId"].as_i64() {
            if let Some(r) = reject_untrusted_write(&shared, &headers) {
                return r;
            }
            return browser_response(
                with_browser_db(shared, move |db| {
                    db.execute(
                        "UPDATE browser_clients SET revoked_ts=? WHERE id=?",
                        rusqlite::params![crate::util::now_ms(), client_id],
                    )
                    .map(|changed| json!({"revoked":changed>0}))
                    .map_err(Into::into)
                })
                .await,
                "",
            );
        }
    }
    let token = match bearer(&headers) {
        Ok(v) => v,
        Err(err) => return browser_response(Err(err), origin),
    };
    let origin = origin.to_string();
    let response_origin = origin.clone();
    let token = token.to_string();
    browser_response(
        with_browser_db(shared, move |db| {
            browser_api::revoke_client(db, &token, &origin, peer.ip())
        })
        .await,
        &response_origin,
    )
}
async fn api_browser_clients_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !peer.ip().is_loopback() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(response) = reject_untrusted_write(&shared, &headers) {
        return response;
    }
    browser_response(
        with_browser_db(shared, |db| browser_api::clients(db)).await,
        "",
    )
}
async fn api_browser_intervention_handler(
    State(shared): State<SharedApp>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<InterventionInput>,
) -> Response {
    let origin = request_origin(&headers);
    let token = match bearer(&headers) {
        Ok(value) => value,
        Err(err) => return browser_response(Err(err), origin),
    };
    let origin = origin.to_string();
    let response_origin = origin.clone();
    let token = token.to_string();
    browser_response(
        with_browser_db(shared, move |db| {
            browser_api::record_intervention(db, &token, &origin, peer.ip(), input)
        })
        .await,
        &response_origin,
    )
}

async fn api_agent_usage_handler(
    State(shared): State<SharedApp>,
    Query(q): Query<AgentUsageQuery>,
) -> Response {
    with_dashboard_app(shared, move |app| query_agent_usage(&app.db, q)).await
}

#[cfg(test)]
mod browser_error_tests {
    use super::*;

    #[test]
    fn browser_infrastructure_errors_return_500() {
        let response =
            browser_response(Err(BrowserTaskError::Internal(anyhow!("join failed"))), "");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let response = browser_response(Err(anyhow!("invalid request")), "");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
