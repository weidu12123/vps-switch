#![cfg_attr(
    all(windows, not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod api;
mod crypto;
mod db;
#[cfg(windows)]
mod desktop;
mod files;
mod guard;
mod inspector;
mod mcp;
mod models;
mod ssh;
mod terminal;

use anyhow::Result;
use api::{build_api_router, AppState};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use crypto::MasterKey;
use db::Database;
use files::TransferManager;
use guard::Guard;
use inspector::InspectorEngine;
use mcp::{run_stdio_bridge, sync_cli_mcp_configs, ExecTaskManager};
use rust_embed::RustEmbed;
use ssh::SshPool;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use terminal::TerminalManager;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

#[derive(RustEmbed)]
#[folder = "frontend/dist/"]
struct EmbeddedFrontend;

/// 数据目录固定落在 `%APPDATA%\vps-switch\data`，与 cwd、exe 位置都无关。
/// 之前按 `cwd/data → exe同级/data → %APPDATA%` 的顺序找，导致同一个 exe 换个 cwd
/// 就用另一套库（机器上真出现过两套互不相干的库 + 不同密钥，API Key 因此对不上、
/// stdio 桥接 401）。统一到一处，杜绝分叉。
fn resolve_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("vps-switch").join("data");
    }
    // Linux / 容器环境下默认落在当前目录下的 data
    PathBuf::from("data")
}

/// 静态资源相对路径白名单校验：拒绝 `..`、空段、反斜杠与盘符，杜绝跳出 frontend/dist 读本机任意文件
fn is_safe_asset_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\\')
        && !rel.contains(':')
        && rel
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// 只接受本机回环 Host。桌面 WebView、Vite 开发代理、stdio 桥接全部走 127.0.0.1；
/// 借此挡掉 DNS rebinding（恶意网页把自己的域名解析到 127.0.0.1 后即被浏览器视为"同源"）。
fn host_header_is_loopback(headers: &HeaderMap) -> bool {
    let Some(raw) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = raw.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

async fn require_loopback_host(State(enforce): State<bool>, req: Request, next: Next) -> Response {
    if enforce && !host_header_is_loopback(req.headers()) {
        return (StatusCode::FORBIDDEN, "仅允许通过本机回环地址访问").into_response();
    }
    next.run(req).await
}

/// 静态前端资源服务：优先读本机 frontend/dist（方便开发热更新），不存在时直接走二进制内嵌的 EmbeddedFrontend（单文件打包免外部依赖）
async fn serve_frontend_asset(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');
    let rel_path = if raw_path.is_empty() {
        "index.html"
    } else {
        raw_path
    };

    if !is_safe_asset_path(rel_path) {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let disk_candidate = PathBuf::from("frontend/dist").join(rel_path);
    if let Ok(bytes) = tokio::fs::read(&disk_candidate).await {
        let mime = mime_guess::from_path(rel_path).first_or_octet_stream();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(bytes))
            .unwrap();
    }

    if let Some(content) = EmbeddedFrontend::get(rel_path) {
        let mime = mime_guess::from_path(rel_path).first_or_octet_stream();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(content.data.into_owned()))
            .unwrap();
    }

    if let Some(index) = EmbeddedFrontend::get("index.html") {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(index.data.into_owned()))
            .unwrap();
    }

    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

fn try_wakeup_existing_instance(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(120)) else {
        return false;
    };
    let req = format!(
        "POST /api/window/show HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    let _ = stream.read(&mut buf);
    true
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let is_mcp_stdio = args.iter().any(|a| a == "--mcp-stdio" || a == "mcp");
    let mut is_headless = args.iter().any(|a| a == "--headless" || a == "--server");
    let start_minimized = args.iter().any(|a| a == "--minimized" || a == "--hidden");

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(18800);

    // 桌面模式下若已有实例在运行，直接唤醒已有主窗口并退出
    #[cfg(windows)]
    if !is_mcp_stdio && !is_headless && try_wakeup_existing_instance(port) {
        return Ok(());
    }

    // 若同级目录存在 GPUI 原生客户端 vps-switch-gpui.exe，则拉起它并自身转为无头守护进程运行
    #[cfg(windows)]
    if !is_mcp_stdio && !is_headless {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let gpui_bin = dir.join("vps-switch-gpui.exe");
                if gpui_bin.is_file() {
                    let mut cmd = std::process::Command::new(gpui_bin);
                    if start_minimized {
                        cmd.arg("--minimized");
                    }
                    let _ = cmd.spawn();
                    is_headless = true;
                }
            }
        }
    }

    if !is_mcp_stdio {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "vps_switch=info,tower_http=warn".into()),
            )
            .init();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let data_dir = resolve_data_dir();
    let master_key = MasterKey::load_or_create(&data_dir)?;
    let db = Arc::new(Database::open(&data_dir, master_key)?);

    if db.count_ssh_hosts().unwrap_or(0) == 0 {
        match db.import_from_1shell() {
            Ok(n) if n > 0 && !is_mcp_stdio => info!("已自动从 1Shell 导入 {n} 台 VPS 主机"),
            _ => {}
        }
    }

    let exec_tasks = Arc::new(ExecTaskManager::new());
    // 绑定地址在构造 AppState 前先算出来：MCP 端点是否免 Key 取决于它（绑回环 = 本机 = 免校验）
    let bind_ip: std::net::IpAddr = std::env::var("HOST")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));
    let state = Arc::new(AppState {
        db,
        guard: Arc::new(Guard::load_or_create(&data_dir)?),
        ssh_pool: Arc::new(SshPool::new()),
        terminals: Arc::new(TerminalManager::new()),
        transfers: Arc::new(TransferManager::new()),
        inspector: Arc::new(InspectorEngine::new()),
        exec_tasks: exec_tasks.clone(),
        sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
        port,
        bind_is_loopback: bind_ip.is_loopback(),
    });

    if is_mcp_stdio {
        return rt.block_on(run_stdio_bridge(state, exec_tasks, port));
    }

    let sync_status = sync_cli_mcp_configs(port);
    info!(
        "MCP 自动配置同步: Claude Code={}, Codex={}",
        sync_status.claude_code_synced, sync_status.codex_synced
    );

    // 启动后台机队状态巡检：
    // 1. 启动错峰 2s 后执行首轮全机队并发检测（避开开机自启瞬间的网卡未拨号窗口）；
    // 2. 启动后 15s 执行一次快速补检（仅针对未在线主机，覆盖刚开机稍晚启动本机代理的场景）；
    // 3. 常驻循环：每 45s 补检一次非在线主机，每 4 轮（180s / 3分钟）执行一次全机队状态与探针刷新。
    {
        let bg_state = state.clone();
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            api::inspect_fleet_wave(&bg_state, false).await;

            tokio::time::sleep(Duration::from_secs(15)).await;
            api::inspect_fleet_wave(&bg_state, true).await;

            let mut tick_count: u64 = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(45)).await;
                tick_count = tick_count.wrapping_add(1);
                let only_non_online = tick_count % 4 != 0;
                api::inspect_fleet_wave(&bg_state, only_non_online).await;
            }
        });
    }

    let bind_ip: std::net::IpAddr = std::env::var("HOST")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));
    let addr = SocketAddr::new(bind_ip, port);

    // 跨源只放行本机回环来源（桌面 WebView 与 Vite 开发端口），不再对任意网站开放读取响应
    let mut allowed_origins: Vec<HeaderValue> = Vec::new();
    for origin in [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        "http://127.0.0.1:5173".to_string(),
        "http://localhost:5173".to_string(),
    ] {
        if let Ok(value) = HeaderValue::from_str(&origin) {
            allowed_origins.push(value);
        }
    }
    let cors = CorsLayer::new()
        .allow_origin(allowed_origins)
        .allow_methods(Any)
        .allow_headers(Any);

    // 绑回环时才强制校验 Host；显式设 HOST=0.0.0.0 对外提供服务时交给登录鉴权把关
    let enforce_loopback_host = bind_ip.is_loopback();
    let app = build_api_router(state.clone())
        .fallback(serve_frontend_asset)
        .layer(cors)
        .layer(middleware::from_fn_with_state(
            enforce_loopback_host,
            require_loopback_host,
        ));

    #[cfg(windows)]
    if !is_headless {
        let rt_handle = rt.handle().clone();
        rt.spawn(async move {
            if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
                info!("vps-switch desktop server listening on http://{addr}");
                let _ = axum::serve(listener, app).await;
            }
        });
        return desktop::run_desktop_window(state, rt_handle, port);
    }

    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!("vps-switch headless server listening on http://{addr}");
        if let Ok(settings) = state.db.get_app_settings() {
            if settings.auth_enabled {
                info!(
                    "Web 登录保护已启用：账号「{}」，初次部署初始密码为「vps-switch」",
                    settings.admin_username
                );
            }
        }
        axum::serve(listener, app).await?;
        Ok::<(), anyhow::Error>(())
    })
}
