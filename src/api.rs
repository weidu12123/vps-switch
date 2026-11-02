use crate::{
    db::Database,
    files::{self, join_remote_path, TransferManager},
    inspector::InspectorEngine,
    mcp::{handle_mcp_request, sync_cli_mcp_configs, ExecTaskManager, JsonRpcRequest},
    models::{
        BatchMcpExposePayload, HostMetaPatch, HostUpsertPayload, ReorderPayload, LOCAL_HOST_ID,
    },
    ssh::SshPool,
    terminal::{PaneBroadcast, PaneCommand, PaneHandle, TerminalManager},
};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path, Query, Request, State,
    },
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use futures::{
    sink::SinkExt,
    stream::{self, StreamExt},
    TryStreamExt,
};
use serde::Deserialize;
use serde_json::json;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::io::StreamReader;
use uuid::Uuid;

pub struct AppState {
    pub db: Arc<Database>,
    pub guard: Arc<crate::guard::Guard>,
    pub ssh_pool: Arc<SshPool>,
    pub terminals: Arc<TerminalManager>,
    pub transfers: Arc<TransferManager>,
    pub inspector: Arc<InspectorEngine>,
    pub exec_tasks: Arc<ExecTaskManager>,
    /// 已签发的 Web 会话令牌 → 签发时刻。用 map 而不是 set 是为了能按 TTL 淘汰：
    /// 之前只增不删，而且每次保存鉴权设置都会再塞一个，长驻进程里会一直涨。
    pub sessions: Mutex<HashMap<String, Instant>>,
    pub port: u16,
    /// 服务是否绑在本机回环。绑回环时 MCP 端点免 API Key（本机优先，对齐 1Shell）；
    /// 只有真正对外部署（HOST=0.0.0.0）时才强制校验 Key。
    pub bind_is_loopback: bool,
}

pub fn build_api_router(state: Arc<AppState>) -> Router {
    let protected_api = Router::new()
        .route("/api/system/about", get(api_get_about))
        .route(
            "/api/system/autostart",
            get(api_get_autostart).post(api_set_autostart),
        )
        .route("/api/system/check-update", post(api_check_update))
        .route("/api/mcp/status", get(api_mcp_status).post(api_mcp_sync))
        .route("/api/settings", get(api_get_settings))
        .route("/api/settings/auth", put(api_update_auth_settings))
        .route("/api/settings/mcp", put(api_update_mcp_settings))
        .route("/api/guard", get(api_get_guard).put(api_update_guard))
        .route("/api/mcp-calls", get(api_list_mcp_calls))
        .route("/api/mcp-calls/:id", get(api_get_mcp_call))
        .route("/api/events", get(api_sse_events))
        .route("/api/hosts", get(list_hosts).post(create_host))
        .route("/api/hosts/reorder", put(reorder_hosts))
        .route("/api/hosts/mcp-expose", put(batch_set_mcp_expose))
        .route("/api/hosts/import-1shell", post(import_from_1shell))
        .route("/api/hosts/check-all", post(check_all_hosts))
        .route("/api/hosts/:id", put(update_host).delete(delete_host))
        .route("/api/hosts/:id/meta", patch(patch_host_meta))
        .route("/api/hosts/:id/check", post(check_single_host))
        .route("/api/hosts/:id/inspect", get(inspect_single_host))
        .route("/api/terminals", get(list_terminals).post(create_terminal))
        .route("/api/terminals/:pane_id", delete(close_terminal))
        .route("/api/ws/terminal/:pane_id", get(ws_terminal_handler))
        .route("/api/files/list", get(api_list_dir))
        .route("/api/files/prewarm", post(api_prewarm_files))
        .route("/api/files/read", get(api_read_file))
        .route("/api/files/write", post(api_write_file))
        .route("/api/files/mkdir", post(api_mkdir))
        .route("/api/files/rename", post(api_rename_item))
        .route("/api/files/remove", post(api_remove_item))
        .route("/api/files/compress", post(api_compress_item))
        .route("/api/files/extract", post(api_extract_item))
        .route(
            "/api/files/upload",
            post(api_stream_upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/files/download", get(api_stream_download))
        .route(
            "/api/transfers",
            get(api_list_transfers)
                .post(api_create_local_remote_transfer)
                .delete(api_clear_transfers),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_web_auth,
        ));

    Router::new()
        .route("/mcp", post(api_mcp_rpc))
        .route("/api/mcp", post(api_mcp_rpc))
        .route("/api/window/show", post(api_show_window))
        .route("/api/window/open-terminal", post(api_open_terminal_window))
        .route("/api/auth/status", get(api_auth_status))
        .route("/api/auth/login", post(api_auth_login))
        .route("/api/auth/logout", post(api_auth_logout))
        .merge(protected_api)
        .with_state(state)
}

#[derive(Deserialize)]
struct OpenTerminalWindowPayload {
    #[serde(alias = "hostId")]
    host_id: String,
    #[serde(alias = "paneId")]
    pane_id: Option<String>,
}

async fn api_open_terminal_window(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<OpenTerminalWindowPayload>,
) -> impl IntoResponse {
    state
        .terminals
        .notify_open_standalone_terminal(&payload.host_id, payload.pane_id.as_deref());
    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn api_show_window(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.terminals.notify_show_main_window();
    if state.inspector.try_claim_wave(Duration::from_secs(10)) {
        let bg = state.clone();
        tokio::spawn(async move {
            inspect_fleet_wave(&bg, true).await;
        });
    }
    (StatusCode::OK, Json(json!({ "ok": true })))
}

fn err_response(status: StatusCode, msg: impl ToString) -> impl IntoResponse {
    (
        status,
        Json(json!({
            "ok": false,
            "error": msg.to_string(),
        })),
    )
}

async fn list_hosts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.db.list_public_hosts() {
        Ok(hosts) => {
            let has_non_online = hosts
                .iter()
                .any(|h| !h.archived && h.status != "online");
            if has_non_online && state.inspector.try_claim_wave(Duration::from_secs(15)) {
                let bg = state.clone();
                tokio::spawn(async move {
                    inspect_fleet_wave(&bg, true).await;
                });
            }
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "hosts": hosts,
                    "inspections": state.inspector.get_all_cached(),
                })),
            )
                .into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 后台异步触发单台主机的状态检测与硬件快照采集，完成后通过 SSE 广播 `hostsUpdated`。
fn spawn_single_host_inspect(state: &Arc<AppState>, host_id: &str) {
    let st = state.clone();
    let hid = host_id.to_string();
    tokio::spawn(async move {
        let _ = st
            .inspector
            .inspect_host(&st.ssh_pool, &st.db, &hid)
            .await;
        st.terminals.notify_hosts_updated();
    });
}

async fn create_host(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<HostUpsertPayload>,
) -> impl IntoResponse {
    if payload.jump_host_id.as_deref() == Some(crate::models::LOCAL_PROXY_JUMP_ID)
        && state.db.get_proxy_settings().0 == "off"
    {
        let _ = state.db.set_setting("proxy_mode", "per_host");
    }
    match state.db.create_host(payload) {
        Ok(host) => {
            spawn_single_host_inspect(&state, &host.id);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "host": host,
                })),
            )
                .into_response()
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn update_host(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<HostUpsertPayload>,
) -> impl IntoResponse {
    if payload.jump_host_id.as_deref() == Some(crate::models::LOCAL_PROXY_JUMP_ID)
        && state.db.get_proxy_settings().0 == "off"
    {
        let _ = state.db.set_setting("proxy_mode", "per_host");
    }
    state.ssh_pool.invalidate_host(&id).await;
    match state.db.update_host(&id, payload) {
        Ok(host) => {
            spawn_single_host_inspect(&state, &host.id);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "host": host,
                })),
            )
                .into_response()
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn patch_host_meta(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(patch): Json<HostMetaPatch>,
) -> impl IntoResponse {
    match state.db.patch_meta(&id, patch) {
        Ok(host) => {
            let hosts = state.db.list_public_hosts().unwrap_or_default();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "host": host,
                    "hosts": hosts,
                })),
            )
                .into_response()
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn reorder_hosts(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ReorderPayload>,
) -> impl IntoResponse {
    match state.db.reorder(&payload.ordered_ids) {
        Ok(()) => match state.db.list_public_hosts() {
            Ok(hosts) => (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "hosts": hosts,
                })),
            )
                .into_response(),
            Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn batch_set_mcp_expose(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<BatchMcpExposePayload>,
) -> impl IntoResponse {
    match state
        .db
        .set_mcp_exposed_batch(payload.exposed, payload.host_ids.as_deref())
    {
        Ok(()) => match state.db.list_public_hosts() {
            Ok(hosts) => (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "hosts": hosts,
                })),
            )
                .into_response(),
            Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn delete_host(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    state.ssh_pool.invalidate_host(&id).await;
    match state.db.delete_host(&id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn import_from_1shell(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.db.import_from_1shell() {
        Ok(imported) => {
            if imported > 0 {
                let bg = state.clone();
                tokio::spawn(async move {
                    inspect_fleet_wave(&bg, false).await;
                });
            }
            match state.db.list_public_hosts() {
                Ok(hosts) => (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "imported": imported,
                        "hosts": hosts,
                    })),
                )
                    .into_response(),
                Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn check_single_host(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let inspection = state
        .inspector
        .inspect_host(&state.ssh_pool, &state.db, &id)
        .await
        .ok();
    state.terminals.notify_hosts_updated();
    match state.db.get_public_host(&id) {
        Ok(Some(host)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "host": host,
                "inspection": inspection,
            })),
        )
            .into_response(),
        Ok(None) => err_response(StatusCode::NOT_FOUND, "主机不存在").into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn inspect_single_host(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state
        .inspector
        .inspect_host(&state.ssh_pool, &state.db, &id)
        .await
    {
        Ok(inspection) => {
            let host = state.db.get_public_host(&id).ok().flatten();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "inspection": inspection,
                    "host": host,
                })),
            )
                .into_response()
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// 并发巡检一轮未归档主机（`only_non_online = true` 时仅补检非 `online` 状态的主机），
/// 并在巡检过程中按节流（800ms）与巡检结束时通过 SSE 广播 `hostsUpdated` 通知前端刷新列表。
pub async fn inspect_fleet_wave(state: &Arc<AppState>, only_non_online: bool) {
    let Ok(records) = state.db.list_records() else {
        return;
    };
    let targets: Vec<_> = records
        .into_iter()
        .filter(|r| !r.archived && (!only_non_online || r.status != "online"))
        .collect();
    if targets.is_empty() {
        return;
    }

    state.inspector.mark_wave_now();
    let last_notify = Arc::new(Mutex::new(Instant::now()));
    stream::iter(targets)
        .for_each_concurrent(8, |record| {
            let st = state.clone();
            let last_notify = last_notify.clone();
            async move {
                let _ = st
                    .inspector
                    .inspect_host(&st.ssh_pool, &st.db, &record.id)
                    .await;
                let should_notify = {
                    let mut g = last_notify.lock().unwrap();
                    if g.elapsed() >= Duration::from_millis(800) {
                        *g = Instant::now();
                        true
                    } else {
                        false
                    }
                };
                if should_notify {
                    st.terminals.notify_hosts_updated();
                }
            }
        })
        .await;

    state.inspector.mark_wave_now();
    state.terminals.notify_hosts_updated();
}

async fn check_all_hosts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Err(e) = state.db.list_records() {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    inspect_fleet_wave(&state, false).await;

    match state.db.list_public_hosts() {
        Ok(hosts) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "hosts": hosts,
                "inspections": state.inspector.get_all_cached(),
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ==================== 板块二：终端 API 与二进制 WebSocket ====================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListTerminalsQuery {
    host_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTerminalPayload {
    host_id: String,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
    cwd: Option<String>,
}

fn default_cols() -> u16 {
    100
}
fn default_rows() -> u16 {
    30
}

async fn list_terminals(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListTerminalsQuery>,
) -> impl IntoResponse {
    let panes = state.terminals.list_panes(query.host_id.as_deref(), false);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "panes": panes,
        })),
    )
}

async fn create_terminal(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateTerminalPayload>,
) -> impl IntoResponse {
    let host_id = payload.host_id.clone();
    match state
        .terminals
        .create_pane(
            &state.ssh_pool,
            &state.db,
            &payload.host_id,
            payload.cols,
            payload.rows,
            payload.cwd,
            false,
        )
        .await
    {
        Ok(pane) => {
            if host_id != LOCAL_HOST_ID
                && state
                    .db
                    .get_record(&host_id)
                    .ok()
                    .flatten()
                    .is_some_and(|r| r.os_info.is_none() || r.latency_ms.is_none())
            {
                spawn_single_host_inspect(&state, &host_id);
            }
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "pane": pane,
                })),
            )
                .into_response()
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

async fn close_terminal(
    State(state): State<Arc<AppState>>,
    Path(pane_id): Path<String>,
) -> impl IntoResponse {
    let closed = state.terminals.close_pane(&state.ssh_pool, &pane_id).await;
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "closed": closed,
        })),
    )
}

async fn ws_terminal_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(pane_id): Path<String>,
) -> impl IntoResponse {
    let Some(pane) = state.terminals.get_pane(&pane_id) else {
        return err_response(StatusCode::NOT_FOUND, "终端 Pane 不存在或已关闭").into_response();
    };
    ws.on_upgrade(move |socket| handle_terminal_ws(socket, pane))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WsClientControl {
    Resize { cols: u16, rows: u16 },
    Paste { text: String },
}

async fn handle_terminal_ws(socket: WebSocket, pane: Arc<PaneHandle>) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut bcast_rx = pane.broadcast_tx.subscribe();

    let replay = pane.replay_bytes();
    if !replay.is_empty() {
        let mut frame = Vec::with_capacity(1 + replay.len());
        frame.push(0x02);
        frame.extend_from_slice(&replay);
        if ws_tx.send(Message::Binary(frame)).await.is_err() {
            return;
        }
    }

    let meta_msg = json!({
        "type": "meta",
        "pane": pane.snapshot_meta(),
    })
    .to_string();
    if ws_tx.send(Message::Text(meta_msg)).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            bcast = bcast_rx.recv() => {
                match bcast {
                    Ok(PaneBroadcast::Output(chunk)) => {
                        let mut frame = Vec::with_capacity(1 + chunk.len());
                        frame.push(0x00);
                        frame.extend_from_slice(&chunk);
                        if ws_tx.send(Message::Binary(frame)).await.is_err() {
                            break;
                        }
                    }
                    Ok(PaneBroadcast::Meta(meta)) => {
                        let text = json!({
                            "type": "meta",
                            "pane": meta,
                        }).to_string();
                        if ws_tx.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let snapshot = pane.replay_bytes();
                        let mut frame = Vec::with_capacity(1 + snapshot.len());
                        frame.push(0x02);
                        frame.extend_from_slice(&snapshot);
                        if ws_tx.send(Message::Binary(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        if data.first() == Some(&0x01) && data.len() > 1 {
                            let _ = pane.cmd_tx.send(PaneCommand::Input { bytes: data[1..].to_vec(), sent: None });
                        } else if !data.is_empty() {
                            let _ = pane.cmd_tx.send(PaneCommand::Input { bytes: data, sent: None });
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(ctrl) = serde_json::from_str::<WsClientControl>(&text) {
                            match ctrl {
                                WsClientControl::Resize { cols, rows } => {
                                    let _ = pane.cmd_tx.send(PaneCommand::Resize(cols, rows));
                                }
                                WsClientControl::Paste { text } => {
                                    let _ = pane.cmd_tx.send(PaneCommand::Paste(text));
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        }
    }
}

// ==================== 板块三 & 四：文件浏览与满速流式传输 API ====================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileQuery {
    host_id: String,
    path: Option<String>,
    #[serde(default)]
    refresh: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrewarmPayload {
    host_id: String,
    cwd: Option<String>,
}

async fn api_prewarm_files(
    State(state): State<Arc<AppState>>,
    Json(p): Json<PrewarmPayload>,
) -> impl IntoResponse {
    let st = state.clone();
    let host_id = p.host_id.clone();
    let cwd = p.cwd.clone();
    tokio::spawn(async move {
        let prev_status = st
            .db
            .get_record(&host_id)
            .ok()
            .flatten()
            .map(|r| r.status);
        match files::list_dir(&st.ssh_pool, &st.db, &host_id, cwd.as_deref(), false).await {
            Ok(listing) => {
                if prev_status.as_deref() != Some("online") {
                    st.terminals.notify_hosts_updated();
                }
                if let Some(ref parent) = listing.parent {
                    let _ = files::list_dir(&st.ssh_pool, &st.db, &host_id, Some(parent), false)
                        .await;
                }
            }
            Err(_) => {
                st.terminals.notify_hosts_updated();
            }
        }
    });
    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn api_list_dir(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> impl IntoResponse {
    let prev_record = state.db.get_record(&q.host_id).ok().flatten();
    match files::list_dir(
        &state.ssh_pool,
        &state.db,
        &q.host_id,
        q.path.as_deref(),
        q.refresh,
    )
    .await
    {
        Ok(listing) => {
            if q.host_id != LOCAL_HOST_ID {
                if prev_record.as_ref().map(|r| r.status.as_str()) != Some("online") {
                    state.terminals.notify_hosts_updated();
                }
                if prev_record
                    .as_ref()
                    .is_some_and(|r| r.os_info.is_none() || r.latency_ms.is_none())
                {
                    spawn_single_host_inspect(&state, &q.host_id);
                }
            }
            // 后台自动预热上级目录与前 6 个子目录，让后续点击子目录或返回上级 0ms 瞬开
            if q.host_id != LOCAL_HOST_ID {
                let pool = state.ssh_pool.clone();
                let db = state.db.clone();
                let host_id = q.host_id.clone();
                let parent = listing.parent.clone();
                let subdirs: Vec<String> = listing
                    .items
                    .iter()
                    .filter(|i| i.is_dir && !i.name.starts_with('.'))
                    .take(6)
                    .map(|i| i.path.clone())
                    .collect();
                tokio::spawn(async move {
                    if let Some(p) = parent {
                        let _ = files::list_dir(&pool, &db, &host_id, Some(&p), false).await;
                    }
                    for sub in subdirs {
                        let _ = files::list_dir(&pool, &db, &host_id, Some(&sub), false).await;
                    }
                });
            }

            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "path": listing.path,
                    "parent": listing.parent,
                    "items": listing.items,
                })),
            )
                .into_response()
        }
        Err(e) => {
            if q.host_id != LOCAL_HOST_ID {
                state.terminals.notify_hosts_updated();
            }
            err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response()
        }
    }
}

async fn api_read_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> impl IntoResponse {
    let Some(path) = q.path.as_deref().filter(|s| !s.trim().is_empty()) else {
        return err_response(StatusCode::BAD_REQUEST, "缺少文件路径").into_response();
    };
    match files::read_text_file(&state.ssh_pool, &state.db, &q.host_id, path).await {
        Ok(res) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "path": res.path,
                "content": res.content,
                "size": res.size,
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteFilePayload {
    host_id: String,
    path: String,
    content: String,
}

async fn api_write_file(
    State(state): State<Arc<AppState>>,
    Json(p): Json<WriteFilePayload>,
) -> impl IntoResponse {
    match files::write_text_file(&state.ssh_pool, &state.db, &p.host_id, &p.path, &p.content).await
    {
        Ok(size) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "path": p.path,
                "size": size,
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MkdirPayload {
    host_id: String,
    path: String,
}

async fn api_mkdir(
    State(state): State<Arc<AppState>>,
    Json(p): Json<MkdirPayload>,
) -> impl IntoResponse {
    match files::create_dir(&state.ssh_pool, &state.db, &p.host_id, &p.path).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenamePayload {
    host_id: String,
    old_path: String,
    new_path: String,
}

async fn api_rename_item(
    State(state): State<Arc<AppState>>,
    Json(p): Json<RenamePayload>,
) -> impl IntoResponse {
    match files::rename_item(
        &state.ssh_pool,
        &state.db,
        &p.host_id,
        &p.old_path,
        &p.new_path,
    )
    .await
    {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemovePayload {
    host_id: String,
    path: String,
    #[serde(default)]
    is_dir: bool,
}

async fn api_remove_item(
    State(state): State<Arc<AppState>>,
    Json(p): Json<RemovePayload>,
) -> impl IntoResponse {
    match files::remove_item(&state.ssh_pool, &state.db, &p.host_id, &p.path, p.is_dir).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveActionPayload {
    host_id: String,
    path: String,
}

async fn api_compress_item(
    State(state): State<Arc<AppState>>,
    Json(p): Json<ArchiveActionPayload>,
) -> impl IntoResponse {
    match files::compress_item(&state.ssh_pool, &state.db, &p.host_id, &p.path).await {
        Ok(archive_path) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "path": archive_path,
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

async fn api_extract_item(
    State(state): State<Arc<AppState>>,
    Json(p): Json<ArchiveActionPayload>,
) -> impl IntoResponse {
    match files::extract_item(&state.ssh_pool, &state.db, &p.host_id, &p.path).await {
        Ok(out_dir) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "path": out_dir,
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadQuery {
    host_id: String,
    dir_path: String,
    filename: String,
    #[serde(default)]
    size: u64,
}

/// 浏览器直传：纯二进制 Body 流直写 SFTP 64 路并发流水线（零 multipart 解析损耗）
async fn api_stream_upload(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> impl IntoResponse {
    let host_name = state
        .db
        .get_record(&q.host_id)
        .ok()
        .flatten()
        .map(|h| h.name)
        .unwrap_or_else(|| q.host_id.clone());

    let remote_target = join_remote_path(&q.dir_path, &q.filename);
    let task_id = state.transfers.create_task(
        &q.host_id,
        &host_name,
        "upload",
        &q.filename,
        None,
        &remote_target,
        q.size,
    );

    let byte_stream = body
        .into_data_stream()
        .map_err(|err| std::io::Error::other(err.to_string()));
    let reader = StreamReader::new(byte_stream);

    match files::stream_upload_to_host(
        &state.ssh_pool,
        &state.db,
        &state.transfers,
        &task_id,
        &q.host_id,
        &q.dir_path,
        &q.filename,
        q.size,
        reader,
    )
    .await
    {
        Ok(final_path) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "taskId": task_id,
                "path": final_path,
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

struct TemporaryDownload(std::path::PathBuf);

impl Drop for TemporaryDownload {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 浏览器下载沿用传输引擎，完成校验后通过 HTTP 交付，临时文件随响应关闭而清理。
async fn api_stream_download(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> Response {
    let Some(file_path) = q.path.filter(|s| !s.trim().is_empty()) else {
        return err_response(StatusCode::BAD_REQUEST, "缺少下载路径").into_response();
    };

    let filename = file_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("download.bin")
        .to_string();

    if q.host_id == LOCAL_HOST_ID {
        let meta = match tokio::fs::metadata(&file_path).await {
            Ok(m) => m,
            Err(e) => return err_response(StatusCode::NOT_FOUND, e).into_response(),
        };
        let file = match tokio::fs::File::open(&file_path).await {
            Ok(f) => f,
            Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
        let stream = tokio_util::io::ReaderStream::with_capacity(file, 512 * 1024);
        return build_download_response(&filename, meta.len(), Body::from_stream(stream));
    }

    let temp_dir =
        std::env::temp_dir().join(format!("vps-switch-browser-download-{}", Uuid::new_v4()));
    if let Err(e) = tokio::fs::create_dir(&temp_dir).await {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let cleanup = TemporaryDownload(temp_dir);
    let local_path = cleanup.0.join("file");
    let task = match files::transfer_local_remote_file(
        &state.ssh_pool,
        &state.db,
        &state.transfers,
        &q.host_id,
        "download",
        &local_path.to_string_lossy(),
        &file_path,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    };
    let file = match tokio::fs::File::open(&local_path).await {
        Ok(f) => f,
        Err(e) => return err_response(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    };
    let reader = tokio_util::io::ReaderStream::with_capacity(file, 512 * 1024);
    let stream = futures::stream::unfold((reader, cleanup), |(mut reader, cleanup)| async move {
        reader.next().await.map(|bytes| (bytes, (reader, cleanup)))
    });
    build_download_response(&filename, task.transferred_bytes, Body::from_stream(stream))
}

fn build_download_response(filename: &str, size: u64, body: Body) -> Response {
    let safe_ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let disposition = format!("attachment; filename=\"{safe_ascii}\"; filename*=UTF-8''{encoded}");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size.to_string())
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(body)
        .unwrap()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListTransfersQuery {
    host_id: Option<String>,
}

async fn api_list_transfers(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListTransfersQuery>,
) -> impl IntoResponse {
    let tasks = state.transfers.list_tasks(q.host_id.as_deref());
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "tasks": tasks,
        })),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalRemoteTransferPayload {
    host_id: String,
    direction: String, // "upload" | "download"
    local_path: String,
    remote_path: String,
}

/// UI 未指定本机落点时的默认路径：系统下载目录（Windows: %USERPROFILE%\Downloads），
/// 取不到就退回当前目录。文件名沿用远端文件名。
fn default_download_path(remote_path: &str) -> String {
    let filename = remote_path
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin");
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(std::path::PathBuf::from)
        .map(|home| home.join("Downloads"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join(filename).to_string_lossy().to_string()
}

async fn api_create_local_remote_transfer(
    State(state): State<Arc<AppState>>,
    Json(p): Json<LocalRemoteTransferPayload>,
) -> impl IntoResponse {
    if p.direction != "upload" && p.direction != "download" {
        return err_response(
            StatusCode::BAD_REQUEST,
            "direction 只能是 upload 或 download",
        )
        .into_response();
    }
    if state.db.get_record(&p.host_id).ok().flatten().is_none() {
        return err_response(StatusCode::BAD_REQUEST, "主机不存在").into_response();
    }
    let local_path = if p.local_path.trim().is_empty() {
        if p.direction == "download" {
            default_download_path(&p.remote_path)
        } else {
            return err_response(StatusCode::BAD_REQUEST, "上传需要指定本机文件路径")
                .into_response();
        }
    } else {
        p.local_path.trim().to_string()
    };

    // 后台跑：大文件不能把这条 HTTP 请求挂住。进度、速率、续传与校验状态都走
    // /api/transfers 队列查询（前端会轮询）。这里立刻返回本机落点供界面提示。
    let st = state.clone();
    let host_id = p.host_id.clone();
    let direction = p.direction.clone();
    let remote_path = p.remote_path.clone();
    let local_for_task = local_path.clone();
    tokio::spawn(async move {
        let _ = files::transfer_local_remote_file(
            &st.ssh_pool,
            &st.db,
            &st.transfers,
            &host_id,
            &direction,
            &local_for_task,
            &remote_path,
        )
        .await;
    });

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "started": true,
            "localPath": local_path,
        })),
    )
        .into_response()
}

async fn api_clear_transfers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.transfers.clear_finished();
    (StatusCode::OK, Json(json!({ "ok": true })))
}

// ==================== 登录鉴权、设置页与 MCP JSON-RPC 2.0 端点 ====================

const SESSION_COOKIE_NAME: &str = "vps_switch_session";
/// Web 会话有效期，与登录 Cookie 的 Max-Age 对齐（30 天）。
const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// 登记新令牌，顺手淘汰过期会话——所有签发入口都走这里，保证会话表不会无界增长。
fn register_session(state: &AppState, token: String) {
    let mut sessions = state.sessions.lock().unwrap();
    sessions.retain(|_, issued| issued.elapsed() < SESSION_TTL);
    sessions.insert(token, Instant::now());
}

fn session_is_valid(state: &AppState, token: &str) -> bool {
    state
        .sessions
        .lock()
        .unwrap()
        .get(token)
        .map(|issued| issued.elapsed() < SESSION_TTL)
        .unwrap_or(false)
}

fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(tok) = auth.strip_prefix("Bearer ") {
            if !tok.trim().is_empty() {
                return Some(tok.trim().to_string());
            }
        }
    }
    if let Some(cookie_hdr) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for part in cookie_hdr.split(';') {
            let trimmed = part.trim();
            if let Some(val) = trimmed.strip_prefix(&format!("{SESSION_COOKIE_NAME}=")) {
                if !val.trim().is_empty() {
                    return Some(val.trim().to_string());
                }
            }
        }
    }
    None
}

fn is_request_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    let auth_enabled = state
        .db
        .get_app_settings()
        .map(|s| s.auth_enabled)
        .unwrap_or(false);
    if !auth_enabled {
        return true;
    }
    let Some(token) = extract_session_token(headers) else {
        return false;
    };
    session_is_valid(state, &token)
}

async fn require_web_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if !is_request_authenticated(&state, req.headers()) {
        return err_response(StatusCode::UNAUTHORIZED, "请先登录").into_response();
    }
    next.run(req).await
}

async fn api_auth_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let settings = state.db.get_app_settings().ok();
    let auth_enabled = settings.as_ref().map(|s| s.auth_enabled).unwrap_or(false);
    let username = settings
        .as_ref()
        .map(|s| s.admin_username.clone())
        .unwrap_or_else(|| "admin".to_string());
    let authenticated = is_request_authenticated(&state, &headers);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "authEnabled": auth_enabled,
            "authenticated": authenticated,
            "username": username,
        })),
    )
}

#[derive(Debug, Deserialize)]
struct LoginPayload {
    username: String,
    password: String,
}

async fn api_auth_login(
    State(state): State<Arc<AppState>>,
    Json(p): Json<LoginPayload>,
) -> Response {
    match state.db.verify_admin_credentials(&p.username, &p.password) {
        Ok(true) => {
            let token = format!(
                "sess_{}{}",
                Uuid::new_v4().simple(),
                Uuid::new_v4().simple()
            );
            register_session(&state, token.clone());
            let cookie = format!(
                "{SESSION_COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"
            );
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::SET_COOKIE, cookie)
                .body(Body::from(
                    json!({
                        "ok": true,
                        "token": token,
                        "username": p.username.trim(),
                    })
                    .to_string(),
                ))
                .unwrap()
        }
        Ok(false) => err_response(StatusCode::UNAUTHORIZED, "账号或密码错误").into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn api_auth_logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(tok) = extract_session_token(&headers) {
        state.sessions.lock().unwrap().remove(&tok);
    }
    let clear_cookie = format!("{SESSION_COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::SET_COOKIE, clear_cookie)
        .body(Body::from(json!({ "ok": true }).to_string()))
        .unwrap()
}

const CREATE_NO_WINDOW: u32 = 0x08000000;

fn check_autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("reg")
            .args([
                "query",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                "/v",
                "VPSSwitch",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    false
}

fn set_autostart_enabled(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        if enabled {
            let exe = std::env::current_exe()?;
            let target_exe = exe
                .parent()
                .map(|dir| dir.join("vps-switch-gpui.exe"))
                .filter(|p| p.is_file())
                .unwrap_or(exe);
            let exe_str = target_exe.to_string_lossy();
            let cmd_val = format!("\"{}\" --minimized", exe_str);
            let status = std::process::Command::new("reg")
                .args([
                    "add",
                    "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                    "/v",
                    "VPSSwitch",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &cmd_val,
                    "/f",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .status()?;
            if !status.success() {
                return Err(anyhow::anyhow!("写入 Windows 注册表开机自启失败"));
            }
        } else {
            let _ = std::process::Command::new("reg")
                .args([
                    "delete",
                    "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                    "/v",
                    "VPSSwitch",
                    "/f",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
        Ok(())
    }
    #[cfg(not(windows))]
    Ok(())
}

async fn api_get_autostart(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let enabled = check_autostart_enabled();
    let close_action = state
        .db
        .get_setting("close_action")
        .ok()
        .flatten()
        .unwrap_or_else(|| "tray".to_string());
    let (proxy_mode, proxy_port) = state.db.get_proxy_settings();
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "autostart": enabled,
            "closeAction": close_action,
            "proxyMode": proxy_mode,
            "proxyPort": proxy_port,
        })),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetAutostartPayload {
    autostart: Option<bool>,
    close_action: Option<String>,
    proxy_mode: Option<String>,
    proxy_port: Option<u16>,
}

async fn api_set_autostart(
    State(state): State<Arc<AppState>>,
    Json(p): Json<SetAutostartPayload>,
) -> impl IntoResponse {
    if let Some(enable) = p.autostart {
        if let Err(e) = set_autostart_enabled(enable) {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(ref action) = p.close_action {
        let valid_action = if action == "exit" { "exit" } else { "tray" };
        let _ = state.db.set_setting("close_action", valid_action);
    }
    let mut proxy_changed = false;
    if let Some(ref mode) = p.proxy_mode {
        let valid_mode = match mode.as_str() {
            "off" => "off",
            "global" => "global",
            _ => "per_host",
        };
        let _ = state.db.set_setting("proxy_mode", valid_mode);
        proxy_changed = true;
    }
    if let Some(port) = p.proxy_port.filter(|port| *port >= 1) {
        let _ = state.db.set_setting("proxy_port", &port.to_string());
        proxy_changed = true;
    }
    if proxy_changed {
        state.ssh_pool.invalidate_all().await;
        let bg = state.clone();
        tokio::spawn(async move {
            inspect_fleet_wave(&bg, true).await;
        });
    }
    let enabled = check_autostart_enabled();
    let close_action = state
        .db
        .get_setting("close_action")
        .ok()
        .flatten()
        .unwrap_or_else(|| "tray".to_string());
    let (proxy_mode, proxy_port) = state.db.get_proxy_settings();
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "autostart": enabled,
            "closeAction": close_action,
            "proxyMode": proxy_mode,
            "proxyPort": proxy_port,
        })),
    )
        .into_response()
}

async fn api_get_about(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let exe_path = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "version": format!("v{}", env!("CARGO_PKG_VERSION")),
            "description": "轻量级 VPS 管理工具与 MCP Server",
            "exePath": exe_path,
            "port": state.port,
            "githubUrl": "https://github.com/weidu12123/vps-switch",
        })),
    )
}

async fn api_check_update(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let current_version = env!("CARGO_PKG_VERSION");
    let started = Instant::now();
    let (proxy_mode, proxy_port) = state.db.get_proxy_settings();
    let mut command = tokio::process::Command::new("gh");
    command.args([
        "release",
        "view",
        "--repo",
        "weidu12123/vps-switch",
        "--json",
        "tagName,name,publishedAt,body,url",
    ]);
    if proxy_mode != "off" && proxy_port > 0 {
        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        command.env("HTTPS_PROXY", &proxy_url);
        command.env("HTTP_PROXY", &proxy_url);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = None;
    let error = match command.output().await {
        Ok(out) => {
            stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            exit_code = out.status.code();
            if out.status.success() {
                match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    Ok(info)
                        if info["tagName"]
                            .as_str()
                            .is_some_and(|s| !s.trim().is_empty()) =>
                    {
                        let tag = info["tagName"].as_str().unwrap().trim_start_matches('v');
                        let has_update = !tag.is_empty() && tag != current_version;
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "ok": true,
                                "currentVersion": format!("v{current_version}"),
                                "latestVersion": info["tagName"].as_str().unwrap_or(&format!("v{current_version}")),
                                "hasUpdate": has_update,
                                "publishedAt": info["publishedAt"].as_str(),
                                "releaseNotes": info["body"].as_str(),
                                "releaseUrl": info["url"].as_str(),
                                "stdout": stdout,
                                "stderr": stderr,
                                "exitCode": exit_code,
                                "durationMs": started.elapsed().as_millis() as u64,
                            })),
                        );
                    }
                    Ok(_) => "更新响应缺少版本号".to_string(),
                    Err(e) => format!("无法解析更新响应: {e}"),
                }
            } else {
                format!("更新查询失败（退出码 {:?}）: {}", exit_code, stderr.trim())
            }
        }
        Err(e) => format!("无法运行 gh 查询更新: {e}"),
    };

    (
        StatusCode::BAD_GATEWAY,
        Json(json!({
            "ok": false,
            "currentVersion": format!("v{current_version}"),
            "error": error,
            "stdout": stdout,
            "stderr": stderr,
            "exitCode": exit_code,
            "durationMs": started.elapsed().as_millis() as u64,
        })),
    )
}

async fn api_get_settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.db.get_app_settings() {
        Ok(settings) => {
            let cli_sync = sync_cli_mcp_configs(state.port);
            let autostart = check_autostart_enabled();
            let close_action = state
                .db
                .get_setting("close_action")
                .ok()
                .flatten()
                .unwrap_or_else(|| "tray".to_string());
            let (proxy_mode, proxy_port) = state.db.get_proxy_settings();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "settings": settings,
                    "cliSync": cli_sync,
                    "autostart": autostart,
                    "closeAction": close_action,
                    "proxyMode": proxy_mode,
                    "proxyPort": proxy_port,
                })),
            )
                .into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAuthSettingsPayload {
    auth_enabled: bool,
    username: String,
    password: Option<String>,
}

async fn api_update_auth_settings(
    State(state): State<Arc<AppState>>,
    Json(p): Json<UpdateAuthSettingsPayload>,
) -> Response {
    match state
        .db
        .update_auth_settings(p.auth_enabled, &p.username, p.password.as_deref())
    {
        Ok(settings) => {
            // 若刚开启了登录保护，自动为当前管理员签发会话 Cookie，避免改完设置立刻被踢出
            let token = format!(
                "sess_{}{}",
                Uuid::new_v4().simple(),
                Uuid::new_v4().simple()
            );
            register_session(&state, token.clone());
            let cookie = format!(
                "{SESSION_COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"
            );
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::SET_COOKIE, cookie)
                .body(Body::from(
                    json!({
                        "ok": true,
                        "settings": settings,
                    })
                    .to_string(),
                ))
                .unwrap()
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateMcpSettingsPayload {
    mcp_enabled: Option<bool>,
    mcp_require_api_key: Option<bool>,
    #[serde(default)]
    regenerate_key: bool,
    mcp_enabled_tools: Option<Vec<String>>,
}

async fn api_update_mcp_settings(
    State(state): State<Arc<AppState>>,
    Json(p): Json<UpdateMcpSettingsPayload>,
) -> impl IntoResponse {
    match state.db.update_mcp_settings(
        p.mcp_enabled,
        p.mcp_require_api_key,
        p.regenerate_key,
        p.mcp_enabled_tools.as_deref(),
    ) {
        Ok(settings) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "settings": settings,
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn api_get_guard(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "path": state.guard.path().to_string_lossy(),
            "content": state.guard.read_list(),
        })),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GuardUpdatePayload {
    content: String,
}

async fn api_update_guard(
    State(state): State<Arc<AppState>>,
    Json(p): Json<GuardUpdatePayload>,
) -> impl IntoResponse {
    match state.guard.write_list(&p.content) {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "path": state.guard.path().to_string_lossy(),
            })),
        )
            .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

fn extract_mcp_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(k) = auth.strip_prefix("Bearer ") {
            if !k.trim().is_empty() {
                return Some(k.trim().to_string());
            }
        }
    }
    if let Some(k) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !k.trim().is_empty() {
            return Some(k.trim().to_string());
        }
    }
    None
}

async fn api_mcp_rpc(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    let Ok(settings) = state.db.get_app_settings() else {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, "读取设置失败").into_response();
    };

    if !settings.mcp_enabled {
        return err_response(StatusCode::FORBIDDEN, "MCP Server 当前已在设置中关闭")
            .into_response();
    }

    // API Key 只在**非回环绑定**（真的对外部署）时才校验。绑在本机回环时，端点只有本机
    // 进程够得着，和 1Shell 一样免校验——既符合"本机优先"的定位，也消除了"桥接进程与
    // 守护进程解析到不同数据目录、key 对不上 → 401 → 永远握不上手"这类纯本机自伤。
    if settings.mcp_require_api_key && !state.bind_is_loopback {
        let provided = extract_mcp_api_key(&headers);
        if provided.as_deref() != Some(settings.mcp_api_key.as_str()) {
            return err_response(
                StatusCode::UNAUTHORIZED,
                "MCP API Key 无效或缺失（请在 Authorization: Bearer <API_KEY> 或 X-API-Key 请求头中提供）",
            )
            .into_response();
        }
    }

    match handle_mcp_request(&state, &state.exec_tasks, req).await {
        Some(val) => (StatusCode::OK, Json(val)).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpCallsQuery {
    limit: Option<i64>,
    tool: Option<String>,
    host: Option<String>,
    ok: Option<bool>,
    /// RFC3339 时间下界（含）/ 上界（不含），由前端按本地时区换算好再发过来
    from: Option<String>,
    to: Option<String>,
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 读取 MCP 调用记录（全程留痕的查看口）：支持按时间范围、主机、工具、成败筛选。
async fn api_list_mcp_calls(
    State(state): State<Arc<AppState>>,
    Query(q): Query<McpCallsQuery>,
) -> impl IntoResponse {
    let filter = crate::db::McpCallFilter {
        limit: q.limit.unwrap_or(200),
        tool: non_empty(q.tool),
        host: non_empty(q.host),
        ok: q.ok,
        from: non_empty(q.from),
        to: non_empty(q.to),
    };
    match state.db.list_mcp_calls(&filter) {
        Ok(calls) => {
            let (hosts, tools) = state.db.mcp_call_facets().unwrap_or_default();
            (StatusCode::OK, Json(json!({ "ok": true, "calls": calls, "hosts": hosts, "tools": tools })))
                .into_response()
        }
        Err(e) => {
            err_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")).into_response()
        }
    }
}

/// 单条 MCP 调用记录的完整正文（参数与返回结果原文）。
async fn api_get_mcp_call(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.db.get_mcp_call(&id) {
        Ok(Some(call)) => (StatusCode::OK, Json(json!({ "ok": true, "call": call }))).into_response(),
        Ok(None) => err_response(StatusCode::NOT_FOUND, "调用记录不存在").into_response(),
        Err(e) => {
            err_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")).into_response()
        }
    }
}

async fn api_mcp_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let status = sync_cli_mcp_configs(state.port);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "status": status,
        })),
    )
}

async fn api_mcp_sync(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let status = sync_cli_mcp_configs(state.port);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "status": status,
        })),
    )
}

async fn api_sse_events(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rx = state.terminals.subscribe_ui_events();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let payload = serde_json::to_string(&ev).unwrap_or_default();
                    return Some((
                        Ok::<SseEvent, std::convert::Infallible>(SseEvent::default().data(payload)),
                        rx,
                    ));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}
