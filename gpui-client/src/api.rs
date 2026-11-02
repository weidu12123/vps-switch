//! 守护进程自动拉起与 HTTP 客户端。

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub(crate) fn deserialize_null_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

pub(crate) fn deserialize_null_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<bool>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(false))
}

pub(crate) fn deserialize_null_u16<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<u16>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(0))
}

pub(crate) fn deserialize_null_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<u64>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(0))
}

pub(crate) fn deserialize_null_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<i64>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(0))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostLink {
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub url: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub username: String,
    pub kind: String,
    pub auth_type: String,
    pub has_password: bool,
    pub has_private_key: bool,
    pub has_passphrase: bool,
    pub jump_host_id: Option<String>,
    pub jump_host_name: Option<String>,
    pub status: String,
    pub os: Option<String>,
    pub pretty_name: Option<String>,
    pub distro_id: Option<String>,
    pub kernel: Option<String>,
    pub arch: Option<String>,
    pub cpu_cores: Option<u32>,
    pub cpu_model: Option<String>,
    pub pinned: bool,
    pub favorited: bool,
    pub archived: bool,
    pub mcp_exposed: bool,
    pub sort_order: i64,
    pub role: Option<String>,
    pub tags: Vec<String>,
    pub links: Vec<HostLink>,
    pub latency_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Inspection {
    pub cpu_cores: Option<u32>,
    pub cpu_usage_pct: f64,
    pub cpu_iowait_pct: f64,
    pub cpu_steal_pct: f64,
    pub cpu_model: Option<String>,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub mem_total_bytes: f64,
    pub mem_used_bytes: f64,
    pub mem_cached_bytes: f64,
    pub swap_total_bytes: f64,
    pub swap_used_bytes: f64,
    pub disk_mount: Option<String>,
    pub disk_total_bytes: f64,
    pub disk_used_bytes: f64,
    pub disks: Vec<DiskUsage>,
    pub net_iface: Option<String>,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    pub net_rx_bytes_total: f64,
    pub net_tx_bytes_total: f64,
    pub uptime_seconds: f64,
    pub tcp_cc: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DiskUsage {
    pub mount: String,
    pub total_bytes: f64,
    pub used_bytes: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub path: String,
    #[serde(default, deserialize_with = "deserialize_null_bool")]
    pub is_dir: bool,
    #[serde(default, deserialize_with = "deserialize_null_u64")]
    pub size: u64,
    #[serde(default, deserialize_with = "deserialize_null_i64")]
    pub mtime: i64,
    #[serde(default)]
    pub permissions: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferTask {
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub host_id: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub host_name: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub direction: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub filename: String,
    #[serde(default)]
    pub local_path: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub remote_path: String,
    #[serde(default, deserialize_with = "deserialize_null_u64")]
    pub total_bytes: u64,
    #[serde(default, deserialize_with = "deserialize_null_u64")]
    pub transferred_bytes: u64,
    #[serde(default, deserialize_with = "deserialize_null_u64")]
    pub speed_bps: u64,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_u64")]
    pub resumed_from: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub verified: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallRecord {
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub ts: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub tool: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub host: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub args: String,
    #[serde(default, deserialize_with = "deserialize_null_bool")]
    pub ok: bool,
    #[serde(default, deserialize_with = "deserialize_null_i64")]
    pub duration_ms: i64,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub summary: String,
}

pub const ALL_MCP_TOOL_NAMES: [&str; 7] = [
    "vps_list",
    "vps_inspect",
    "vps_exec",
    "vps_terminal",
    "vps_fs",
    "vps_transfer",
    "vps_task",
];

pub fn default_mcp_tool_names() -> Vec<String> {
    ALL_MCP_TOOL_NAMES.iter().map(|s| (*s).to_owned()).collect()
}

#[derive(Clone, Debug, Default)]
pub struct SettingsBundle {
    pub mcp_enabled: bool,
    pub mcp_require_api_key: bool,
    pub mcp_api_key: String,
    pub mcp_enabled_tools: Vec<String>,
    pub auth_enabled: bool,
    pub admin_username: String,
    pub has_admin_password: bool,
    pub autostart: bool,
    pub close_action: String,
    pub proxy_mode: String,
    pub proxy_port: u16,
    pub claude_code_synced: bool,
    pub codex_synced: bool,
    pub exe_path: String,
    pub port: u16,
    pub version: String,
    pub guard_path: String,
    pub guard_content: String,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateCheckOutcome {
    pub ok: bool,
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_notes: Option<String>,
    pub release_url: Option<String>,
    pub error: Option<String>,
}

pub type NetHistory = HashMap<String, Vec<(f64, f64)>>;

pub enum ApiEvent {
    Hosts {
        hosts: Vec<Host>,
        inspections: HashMap<String, Inspection>,
    },
    HostsFailed(String),
    Inspection {
        host_id: String,
        inspection: Inspection,
        host: Option<Host>,
    },
    InspectionFailed {
        host_id: String,
        error: String,
    },
    CheckedAll {
        hosts: Vec<Host>,
        inspections: HashMap<String, Inspection>,
    },
    HostChecked {
        host_id: String,
        host: Option<Host>,
        inspection: Option<Inspection>,
        error: Option<String>,
    },
    HostSaved {
        host: Host,
        is_edit: bool,
    },
    HostSaveFailed(String),
    HostDeleted {
        host_id: String,
        host_name: String,
    },
    HostDeleteFailed(String),
    HostUpdated(Host),
    FilesListed {
        host_id: String,
        requested_path: String,
        path: String,
        parent: Option<String>,
        items: Vec<FileEntry>,
        prefetch_only: bool,
    },
    FilesListFailed {
        host_id: String,
        error: String,
    },
    FileRead {
        host_id: String,
        path: String,
        name: String,
        content: String,
    },
    FileReadFailed(String),
    FileWritten {
        host_id: String,
        path: String,
        name: String,
        from_editor: bool,
    },
    FileWriteFailed(String),
    FileActionDone {
        host_id: String,
        dir_to_reload: String,
        highlight_path: Option<String>,
        toast: String,
    },
    FileActionFailed {
        host_id: String,
        error: String,
    },
    TransfersListed {
        host_id: String,
        tasks: Vec<TransferTask>,
    },
    TransferStarted {
        host_id: String,
        dir_to_reload: Option<String>,
        toast: String,
    },
    SettingsLoaded(SettingsBundle),
    AutostartSaved {
        autostart: bool,
        close_action: String,
    },
    AutostartSaveFailed {
        prev_autostart: bool,
        prev_close_action: String,
        error: String,
    },
    ProxySaved {
        proxy_mode: String,
        proxy_port: u16,
    },
    McpSaved {
        mcp_enabled: bool,
        mcp_require_api_key: bool,
        mcp_api_key: String,
        mcp_enabled_tools: Vec<String>,
        regenerated: bool,
    },
    McpSaveFailed {
        prev_enabled: bool,
        prev_require_key: bool,
        error: String,
    },
    AuthSaved {
        auth_enabled: bool,
        admin_username: String,
        has_admin_password: bool,
    },
    CliSynced {
        claude_code_synced: bool,
        codex_synced: bool,
    },
    GuardSaved,
    McpCallsLoaded {
        calls: Vec<McpCallRecord>,
        hosts: Vec<String>,
        tools: Vec<String>,
    },
    McpCallDetailLoaded {
        call_id: String,
        args: String,
        result: String,
    },
    UpdateChecked(UpdateCheckOutcome),
    RevealTerminal {
        host_id: String,
        pane: crate::terminal::PaneMeta,
        reason: Option<String>,
    },
    ShowMainWindow,
    OpenStandaloneTerminal {
        host_id: String,
        pane_id: Option<String>,
    },
    HostsUpdated,
    Failed(String),
    Json(serde_json::Value),
}

pub enum ApiCommand {
    LoadHosts,
    Inspect(String),
    CheckAll,
    CheckHost(String),
    CreateHost(serde_json::Value),
    UpdateHost {
        host_id: String,
        payload: serde_json::Value,
    },
    DeleteHost {
        host_id: String,
        host_name: String,
    },
    PatchMeta {
        host_id: String,
        patch: serde_json::Value,
    },
    SetMcpExposedBatch {
        exposed: bool,
        host_ids: Option<Vec<String>>,
    },
    Reorder {
        ordered_ids: Vec<String>,
    },
    PrewarmFiles(String),
    ListFiles {
        host_id: String,
        path: String,
        force_refresh: bool,
        prefetch_only: bool,
    },
    ReadFile {
        host_id: String,
        path: String,
        name: String,
    },
    WriteFile {
        host_id: String,
        path: String,
        name: String,
        content: String,
        from_editor: bool,
        dir_to_reload: String,
    },
    Mkdir {
        host_id: String,
        dir_to_reload: String,
        target_path: String,
        folder_name: String,
    },
    RenameFile {
        host_id: String,
        dir_to_reload: String,
        old_path: String,
        new_path: String,
        new_name: String,
    },
    RemoveFile {
        host_id: String,
        dir_to_reload: String,
        path: String,
        name: String,
        is_dir: bool,
    },
    CompressFile {
        host_id: String,
        dir_to_reload: String,
        path: String,
    },
    ExtractFile {
        host_id: String,
        dir_to_reload: String,
        path: String,
    },
    StartTransfer {
        host_id: String,
        direction: String,
        local_path: String,
        remote_path: String,
        dir_to_reload: Option<String>,
    },
    ListTransfers {
        host_id: String,
    },
    ClearTransfers {
        host_id: String,
    },
    LoadSettings,
    SaveAutostart {
        autostart: bool,
        close_action: String,
        prev_autostart: bool,
        prev_close_action: String,
    },
    SaveProxy {
        proxy_mode: String,
        proxy_port: u16,
    },
    SaveMcp {
        mcp_enabled: bool,
        mcp_require_api_key: bool,
        regenerate_key: bool,
        prev_enabled: bool,
        prev_require_key: bool,
    },
    SaveMcpTools {
        mcp_enabled_tools: Vec<String>,
    },
    SaveAuth {
        auth_enabled: bool,
        username: String,
        password: Option<String>,
    },
    SyncCli,
    SaveGuard {
        content: String,
    },
    LoadMcpCalls {
        from: String,
        to: String,
        host: String,
        tool: String,
        ok: String,
    },
    LoadMcpCallDetail {
        call_id: String,
    },
    CheckUpdate,
    DeletePane(String),
    Post {
        path: String,
        body: serde_json::Value,
    },
}

pub struct Api {
    commands: async_channel::Sender<ApiCommand>,
    events: mpsc::Receiver<ApiEvent>,
}

pub fn port() -> String {
    std::env::var("VPS_SWITCH_PORT").unwrap_or_else(|_| "18800".into())
}

pub fn base_url() -> String {
    format!("http://127.0.0.1:{}", port())
}

pub fn ensure_daemon_running() {
    let port_num: u16 = port().parse().unwrap_or(18800);
    let addr = SocketAddr::from(([127, 0, 0, 1], port_num));
    if TcpStream::connect_timeout(&addr, Duration::from_millis(180)).is_ok() {
        return;
    }
    let Some(daemon_bin) = find_daemon_binary() else {
        return;
    };
    let mut cmd = Command::new(daemon_bin);
    cmd.arg("--headless");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if cmd.spawn().is_ok() {
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(120));
            if TcpStream::connect_timeout(&addr, Duration::from_millis(120)).is_ok() {
                break;
            }
        }
    }
}

fn find_daemon_binary() -> Option<PathBuf> {
    let bin_name = if cfg!(windows) {
        "vps-switch.exe"
    } else {
        "vps-switch"
    };
    let current_exe = std::env::current_exe().ok()?;
    let exe_dir = current_exe.parent()?;
    let sibling = exe_dir.join(bin_name);
    if sibling.is_file() {
        return Some(sibling);
    }
    let release_candidate = exe_dir.parent()?.join("release").join(bin_name);
    if release_candidate.is_file() {
        return Some(release_candidate);
    }
    None
}

impl Api {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = async_channel::unbounded::<ApiCommand>();
        let (event_tx, event_rx) = mpsc::channel::<ApiEvent>();
        std::thread::Builder::new()
            .name("vps-switch-api".into())
            .spawn(move || {
                ensure_daemon_running();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(async move {
                    let client = reqwest::Client::new();
                    let sse_client = client.clone();
                    let sse_tx = event_tx.clone();
                    tokio::spawn(async move {
                        sse_event_loop(sse_client, sse_tx).await;
                    });
                    while let Ok(command) = command_rx.recv().await {
                        let req_client = client.clone();
                        let req_tx = event_tx.clone();
                        tokio::spawn(async move {
                            if let Some(event) = handle(&req_client, command).await {
                                let _ = req_tx.send(event);
                            }
                        });
                    }
                });
            })
            .expect("api thread");
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn disconnected() -> Self {
        let (command_tx, _command_rx) = async_channel::unbounded::<ApiCommand>();
        let (_event_tx, event_rx) = mpsc::channel::<ApiEvent>();
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn send(&self, command: ApiCommand) {
        let _ = self.commands.send_blocking(command);
    }

    pub fn try_recv(&self) -> Vec<ApiEvent> {
        self.events.try_iter().collect()
    }
}

async fn handle(client: &reqwest::Client, command: ApiCommand) -> Option<ApiEvent> {
    match command {
        ApiCommand::LoadHosts => match get_json(client, "/api/hosts").await {
            Ok(body) => Some(ApiEvent::Hosts {
                hosts: hosts_from_json(&body),
                inspections: inspections_map_from_json(&body),
            }),
            Err(error) => Some(ApiEvent::HostsFailed(error)),
        },
        ApiCommand::Inspect(host_id) => {
            let path = format!("/api/hosts/{host_id}/inspect");
            match get_json(client, &path).await {
                Ok(body) => {
                    let inspection = body
                        .get("inspection")
                        .map(inspection_from_json)
                        .unwrap_or_default();
                    let host = body.get("host").and_then(host_from_json);
                    Some(ApiEvent::Inspection {
                        host_id,
                        inspection,
                        host,
                    })
                }
                Err(error) => Some(ApiEvent::InspectionFailed { host_id, error }),
            }
        }
        ApiCommand::CheckAll => match post_json(client, "/api/hosts/check-all", None).await {
            Ok(body) => Some(ApiEvent::CheckedAll {
                hosts: hosts_from_json(&body),
                inspections: inspections_map_from_json(&body),
            }),
            Err(error) => Some(ApiEvent::Failed(error)),
        },
        ApiCommand::CheckHost(host_id) => {
            let path = format!("/api/hosts/{host_id}/check");
            match post_json(client, &path, None).await {
                Ok(body) => Some(ApiEvent::HostChecked {
                    host_id,
                    host: body.get("host").and_then(host_from_json),
                    inspection: body.get("inspection").map(inspection_from_json),
                    error: None,
                }),
                Err(error) => Some(ApiEvent::HostChecked {
                    host_id,
                    host: None,
                    inspection: None,
                    error: Some(error),
                }),
            }
        }
        ApiCommand::CreateHost(payload) => {
            match post_json(client, "/api/hosts", Some(&payload)).await {
                Ok(body) => match body.get("host").and_then(host_from_json) {
                    Some(host) => Some(ApiEvent::HostSaved {
                        host,
                        is_edit: false,
                    }),
                    None => Some(ApiEvent::HostSaveFailed("响应缺少 host 字段".into())),
                },
                Err(error) => Some(ApiEvent::HostSaveFailed(error)),
            }
        }
        ApiCommand::UpdateHost { host_id, payload } => {
            let path = format!("/api/hosts/{host_id}");
            match put_json(client, &path, &payload).await {
                Ok(body) => match body.get("host").and_then(host_from_json) {
                    Some(host) => Some(ApiEvent::HostSaved {
                        host,
                        is_edit: true,
                    }),
                    None => Some(ApiEvent::HostSaveFailed("响应缺少 host 字段".into())),
                },
                Err(error) => Some(ApiEvent::HostSaveFailed(error)),
            }
        }
        ApiCommand::DeleteHost { host_id, host_name } => {
            let path = format!("/api/hosts/{host_id}");
            match delete_json(client, &path).await {
                Ok(_) => Some(ApiEvent::HostDeleted { host_id, host_name }),
                Err(error) => Some(ApiEvent::HostDeleteFailed(error)),
            }
        }
        ApiCommand::PatchMeta { host_id, patch } => {
            let path = format!("/api/hosts/{host_id}/meta");
            match patch_json(client, &path, &patch).await {
                Ok(body) => match body.get("host").and_then(host_from_json) {
                    Some(host) => Some(ApiEvent::HostUpdated(host)),
                    None => Some(ApiEvent::Json(body)),
                },
                Err(error) => Some(ApiEvent::Failed(error)),
            }
        }
        ApiCommand::SetMcpExposedBatch { exposed, host_ids } => {
            match put_json(
                client,
                "/api/hosts/mcp-expose",
                &serde_json::json!({
                    "exposed": exposed,
                    "hostIds": host_ids,
                }),
            )
            .await
            {
                Ok(body) => Some(ApiEvent::Hosts {
                    hosts: hosts_from_json(&body),
                    inspections: inspections_map_from_json(&body),
                }),
                Err(error) => Some(ApiEvent::Failed(error)),
            }
        }
        ApiCommand::Reorder { ordered_ids } => {
            match put_json(
                client,
                "/api/hosts/reorder",
                &serde_json::json!({ "orderedIds": ordered_ids }),
            )
            .await
            {
                Ok(body) => Some(ApiEvent::Hosts {
                    hosts: hosts_from_json(&body),
                    inspections: inspections_map_from_json(&body),
                }),
                Err(error) => Some(ApiEvent::Failed(error)),
            }
        }
        ApiCommand::PrewarmFiles(host_id) => {
            let _ = post_json(
                client,
                "/api/files/prewarm",
                Some(&serde_json::json!({ "hostId": host_id })),
            )
            .await;
            None
        }
        ApiCommand::ListFiles {
            host_id,
            path,
            force_refresh,
            prefetch_only,
        } => {
            let mut req = client
                .get(format!("{}/api/files/list", base_url()))
                .query(&[("hostId", host_id.as_str()), ("path", path.as_str())]);
            if force_refresh {
                req = req.query(&[("refresh", "true")]);
            }
            match req.send().await {
                Ok(resp) => match read_json(resp).await {
                    Ok(body) => {
                        let actual_path = body
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&path)
                            .to_owned();
                        let parent = body
                            .get("parent")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                        let items: Vec<FileEntry> = body
                            .get("items")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|item| {
                                        serde_json::from_value::<FileEntry>(item.clone()).ok()
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(ApiEvent::FilesListed {
                            host_id,
                            requested_path: path,
                            path: actual_path,
                            parent,
                            items,
                            prefetch_only,
                        })
                    }
                    Err(error) => {
                        if prefetch_only {
                            None
                        } else {
                            Some(ApiEvent::FilesListFailed { host_id, error })
                        }
                    }
                },
                Err(err) => {
                    if prefetch_only {
                        None
                    } else {
                        Some(ApiEvent::FilesListFailed {
                            host_id,
                            error: format!("连接守护进程失败：{err}"),
                        })
                    }
                }
            }
        }
        ApiCommand::ReadFile {
            host_id,
            path,
            name,
        } => {
            let resp = client
                .get(format!("{}/api/files/read", base_url()))
                .query(&[("hostId", host_id.as_str()), ("path", path.as_str())])
                .send()
                .await;
            match resp {
                Ok(r) => match read_json(r).await {
                    Ok(body) => {
                        let content = body
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned();
                        Some(ApiEvent::FileRead {
                            host_id,
                            path,
                            name,
                            content,
                        })
                    }
                    Err(err) => Some(ApiEvent::FileReadFailed(err)),
                },
                Err(err) => Some(ApiEvent::FileReadFailed(format!("读取失败: {err}"))),
            }
        }
        ApiCommand::WriteFile {
            host_id,
            path,
            name,
            content,
            from_editor,
            dir_to_reload,
        } => {
            let res = post_json(
                client,
                "/api/files/write",
                Some(&serde_json::json!({
                    "hostId": host_id,
                    "path": path,
                    "content": content,
                })),
            )
            .await;
            match res {
                Ok(_) => {
                    if from_editor {
                        Some(ApiEvent::FileWritten {
                            host_id,
                            path,
                            name,
                            from_editor: true,
                        })
                    } else {
                        Some(ApiEvent::FileActionDone {
                            host_id,
                            dir_to_reload,
                            highlight_path: Some(path),
                            toast: format!("已创建文件 {name}"),
                        })
                    }
                }
                Err(err) => {
                    if from_editor {
                        Some(ApiEvent::FileWriteFailed(err))
                    } else {
                        Some(ApiEvent::FileActionFailed {
                            host_id,
                            error: err,
                        })
                    }
                }
            }
        }
        ApiCommand::Mkdir {
            host_id,
            dir_to_reload,
            target_path,
            folder_name,
        } => {
            let res = post_json(
                client,
                "/api/files/mkdir",
                Some(&serde_json::json!({
                    "hostId": host_id,
                    "path": target_path,
                })),
            )
            .await;
            match res {
                Ok(_) => Some(ApiEvent::FileActionDone {
                    host_id,
                    dir_to_reload,
                    highlight_path: Some(target_path),
                    toast: format!("已创建目录 {folder_name}"),
                }),
                Err(error) => Some(ApiEvent::FileActionFailed { host_id, error }),
            }
        }
        ApiCommand::RenameFile {
            host_id,
            dir_to_reload,
            old_path,
            new_path,
            new_name,
        } => {
            let res = post_json(
                client,
                "/api/files/rename",
                Some(&serde_json::json!({
                    "hostId": host_id,
                    "oldPath": old_path,
                    "newPath": new_path,
                })),
            )
            .await;
            match res {
                Ok(_) => Some(ApiEvent::FileActionDone {
                    host_id,
                    dir_to_reload,
                    highlight_path: Some(new_path),
                    toast: format!("已重命名为 {new_name}"),
                }),
                Err(error) => Some(ApiEvent::FileActionFailed { host_id, error }),
            }
        }
        ApiCommand::RemoveFile {
            host_id,
            dir_to_reload,
            path,
            name,
            is_dir,
        } => {
            let res = post_json(
                client,
                "/api/files/remove",
                Some(&serde_json::json!({
                    "hostId": host_id,
                    "path": path,
                    "isDir": is_dir,
                })),
            )
            .await;
            match res {
                Ok(_) => Some(ApiEvent::FileActionDone {
                    host_id,
                    dir_to_reload,
                    highlight_path: None,
                    toast: format!("已删除 {name}"),
                }),
                Err(error) => Some(ApiEvent::FileActionFailed { host_id, error }),
            }
        }
        ApiCommand::CompressFile {
            host_id,
            dir_to_reload,
            path,
        } => {
            let res = post_json(
                client,
                "/api/files/compress",
                Some(&serde_json::json!({
                    "hostId": host_id,
                    "path": path,
                })),
            )
            .await;
            match res {
                Ok(body) => {
                    let archive = body
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    let short = archive.rsplit('/').next().unwrap_or(&archive);
                    Some(ApiEvent::FileActionDone {
                        host_id,
                        dir_to_reload,
                        highlight_path: Some(archive.clone()),
                        toast: format!("已压缩生成 {short}"),
                    })
                }
                Err(error) => Some(ApiEvent::FileActionFailed { host_id, error }),
            }
        }
        ApiCommand::ExtractFile {
            host_id,
            dir_to_reload,
            path,
        } => {
            let res = post_json(
                client,
                "/api/files/extract",
                Some(&serde_json::json!({
                    "hostId": host_id,
                    "path": path,
                })),
            )
            .await;
            match res {
                Ok(body) => {
                    let out_dir = body
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    let short = out_dir.rsplit('/').next().unwrap_or(&out_dir);
                    Some(ApiEvent::FileActionDone {
                        host_id,
                        dir_to_reload,
                        highlight_path: Some(out_dir.clone()),
                        toast: format!("已解压至 {short}"),
                    })
                }
                Err(error) => Some(ApiEvent::FileActionFailed { host_id, error }),
            }
        }
        ApiCommand::StartTransfer {
            host_id,
            direction,
            local_path,
            remote_path,
            dir_to_reload,
        } => {
            let res = post_json(
                client,
                "/api/transfers",
                Some(&serde_json::json!({
                    "hostId": host_id,
                    "direction": direction,
                    "localPath": local_path,
                    "remotePath": remote_path,
                })),
            )
            .await;
            match res {
                Ok(body) => {
                    let target_local = body
                        .get("localPath")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&local_path);
                    let toast = if direction == "download" {
                        format!("已发起下载至 {target_local}")
                    } else {
                        format!("已发起上传至 {remote_path}")
                    };
                    Some(ApiEvent::TransferStarted {
                        host_id,
                        dir_to_reload,
                        toast,
                    })
                }
                Err(error) => Some(ApiEvent::FileActionFailed { host_id, error }),
            }
        }
        ApiCommand::ListTransfers { host_id } => {
            let resp = client
                .get(format!("{}/api/transfers", base_url()))
                .query(&[("hostId", host_id.as_str())])
                .send()
                .await;
            if let Ok(r) = resp
                && let Ok(body) = read_json(r).await
            {
                let tasks: Vec<TransferTask> = body
                    .get("tasks")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                serde_json::from_value::<TransferTask>(item.clone()).ok()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Some(ApiEvent::TransfersListed { host_id, tasks });
            }
            None
        }
        ApiCommand::ClearTransfers { host_id } => {
            let _ = delete_json(client, "/api/transfers").await;
            let resp = client
                .get(format!("{}/api/transfers", base_url()))
                .query(&[("hostId", host_id.as_str())])
                .send()
                .await;
            if let Ok(r) = resp
                && let Ok(body) = read_json(r).await
            {
                let tasks: Vec<TransferTask> = body
                    .get("tasks")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                serde_json::from_value::<TransferTask>(item.clone()).ok()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Some(ApiEvent::TransfersListed { host_id, tasks });
            }
            None
        }
        ApiCommand::LoadSettings => {
            let mut bundle = SettingsBundle {
                mcp_enabled_tools: default_mcp_tool_names(),
                admin_username: "admin".into(),
                close_action: "tray".into(),
                proxy_mode: "per_host".into(),
                proxy_port: 7890,
                port: port().parse().unwrap_or(18800),
                version: concat!("v", env!("CARGO_PKG_VERSION")).into(),
                ..SettingsBundle::default()
            };
            if let Ok(body) = get_json(client, "/api/settings").await {
                if let Some(s) = body.get("settings") {
                    bundle.mcp_enabled = s
                        .get("mcpEnabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    bundle.mcp_require_api_key = s
                        .get("mcpRequireApiKey")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    bundle.mcp_api_key = s
                        .get("mcpApiKey")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(arr) = s.get("mcpEnabledTools").and_then(|v| v.as_array()) {
                        bundle.mcp_enabled_tools = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect();
                    }
                    bundle.auth_enabled = s
                        .get("authEnabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    bundle.admin_username = s
                        .get("adminUsername")
                        .and_then(|v| v.as_str())
                        .unwrap_or("admin")
                        .to_owned();
                    bundle.has_admin_password = s
                        .get("hasAdminPassword")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
                if let Some(cli) = body.get("cliSync") {
                    bundle.claude_code_synced = cli
                        .get("claudeCodeSynced")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    bundle.codex_synced = cli
                        .get("codexSynced")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if let Some(exe) = cli.get("exePath").and_then(|v| v.as_str()) {
                        bundle.exe_path = exe.to_owned();
                    }
                }
                if let Some(auto) = body.get("autostart").and_then(|v| v.as_bool()) {
                    bundle.autostart = auto;
                }
                if let Some(ca) = body.get("closeAction").and_then(|v| v.as_str()) {
                    bundle.close_action = ca.to_owned();
                }
                if let Some(pm) = body.get("proxyMode").and_then(|v| v.as_str()) {
                    bundle.proxy_mode = pm.to_owned();
                }
                if let Some(pp) = body
                    .get("proxyPort")
                    .and_then(|v| v.as_u64())
                    .filter(|p| *p >= 1 && *p <= 65535)
                {
                    bundle.proxy_port = pp as u16;
                }
            }
            if let Ok(about) = get_json(client, "/api/system/about").await {
                if let Some(v) = about.get("version").and_then(|v| v.as_str()) {
                    bundle.version = v.to_owned();
                }
                if let Some(p) = about.get("port").and_then(|v| v.as_u64()) {
                    bundle.port = p as u16;
                }
                if let Some(exe) = about.get("exePath").and_then(|v| v.as_str()) {
                    bundle.exe_path = exe.to_owned();
                }
            }
            if let Ok(guard) = get_json(client, "/api/guard").await {
                bundle.guard_path = guard
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("data/deny-list.txt")
                    .to_owned();
                bundle.guard_content = guard
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
            }
            Some(ApiEvent::SettingsLoaded(bundle))
        }
        ApiCommand::SaveAutostart {
            autostart,
            close_action,
            prev_autostart,
            prev_close_action,
        } => {
            let res = post_json(
                client,
                "/api/system/autostart",
                Some(&serde_json::json!({
                    "autostart": autostart,
                    "closeAction": close_action,
                })),
            )
            .await;
            match res {
                Ok(body) => Some(ApiEvent::AutostartSaved {
                    autostart: body
                        .get("autostart")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(autostart),
                    close_action: body
                        .get("closeAction")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&close_action)
                        .to_owned(),
                }),
                Err(error) => Some(ApiEvent::AutostartSaveFailed {
                    prev_autostart,
                    prev_close_action,
                    error,
                }),
            }
        }
        ApiCommand::SaveProxy {
            proxy_mode,
            proxy_port,
        } => {
            let res = post_json(
                client,
                "/api/system/autostart",
                Some(&serde_json::json!({
                    "proxyMode": proxy_mode,
                    "proxyPort": proxy_port,
                })),
            )
            .await;
            match res {
                Ok(body) => Some(ApiEvent::ProxySaved {
                    proxy_mode: body
                        .get("proxyMode")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&proxy_mode)
                        .to_owned(),
                    proxy_port: body
                        .get("proxyPort")
                        .and_then(|v| v.as_u64())
                        .map(|p| p as u16)
                        .unwrap_or(proxy_port),
                }),
                Err(error) => Some(ApiEvent::Failed(format!("保存本机代理设置失败: {error}"))),
            }
        }
        ApiCommand::SaveMcp {
            mcp_enabled,
            mcp_require_api_key,
            regenerate_key,
            prev_enabled,
            prev_require_key,
        } => {
            let res = put_json(
                client,
                "/api/settings/mcp",
                &serde_json::json!({
                    "mcpEnabled": mcp_enabled,
                    "mcpRequireApiKey": mcp_require_api_key,
                    "regenerateKey": regenerate_key,
                }),
            )
            .await;
            match res {
                Ok(body) => {
                    let s = body.get("settings");
                    Some(ApiEvent::McpSaved {
                        mcp_enabled: s
                            .and_then(|v| v.get("mcpEnabled"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(mcp_enabled),
                        mcp_require_api_key: s
                            .and_then(|v| v.get("mcpRequireApiKey"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(mcp_require_api_key),
                        mcp_api_key: s
                            .and_then(|v| v.get("mcpApiKey"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                        mcp_enabled_tools: s
                            .and_then(|v| v.get("mcpEnabledTools"))
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(str::to_owned))
                                    .collect()
                            })
                            .unwrap_or_else(default_mcp_tool_names),
                        regenerated: regenerate_key,
                    })
                }
                Err(error) => Some(ApiEvent::McpSaveFailed {
                    prev_enabled,
                    prev_require_key,
                    error,
                }),
            }
        }
        ApiCommand::SaveMcpTools { mcp_enabled_tools } => {
            let res = put_json(
                client,
                "/api/settings/mcp",
                &serde_json::json!({
                    "mcpEnabledTools": mcp_enabled_tools,
                }),
            )
            .await;
            match res {
                Ok(body) => {
                    let s = body.get("settings");
                    Some(ApiEvent::McpSaved {
                        mcp_enabled: s
                            .and_then(|v| v.get("mcpEnabled"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true),
                        mcp_require_api_key: s
                            .and_then(|v| v.get("mcpRequireApiKey"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true),
                        mcp_api_key: s
                            .and_then(|v| v.get("mcpApiKey"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                        mcp_enabled_tools: s
                            .and_then(|v| v.get("mcpEnabledTools"))
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(str::to_owned))
                                    .collect()
                            })
                            .unwrap_or(mcp_enabled_tools),
                        regenerated: false,
                    })
                }
                Err(error) => Some(ApiEvent::Failed(format!("保存 MCP 工具开放范围失败: {error}"))),
            }
        }
        ApiCommand::SaveAuth {
            auth_enabled,
            username,
            password,
        } => {
            let mut payload = serde_json::json!({
                "authEnabled": auth_enabled,
                "username": username,
            });
            if let Some(pwd) = password
                && !pwd.is_empty()
            {
                payload["password"] = serde_json::Value::String(pwd);
            }
            match put_json(client, "/api/settings/auth", &payload).await {
                Ok(body) => {
                    let s = body.get("settings");
                    Some(ApiEvent::AuthSaved {
                        auth_enabled: s
                            .and_then(|v| v.get("authEnabled"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(auth_enabled),
                        admin_username: s
                            .and_then(|v| v.get("adminUsername"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(&username)
                            .to_owned(),
                        has_admin_password: s
                            .and_then(|v| v.get("hasAdminPassword"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true),
                    })
                }
                Err(error) => Some(ApiEvent::Failed(error)),
            }
        }
        ApiCommand::SyncCli => match post_json(client, "/api/mcp/status", None).await {
            Ok(body) => {
                let st = body.get("status");
                Some(ApiEvent::CliSynced {
                    claude_code_synced: st
                        .and_then(|v| v.get("claudeCodeSynced"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    codex_synced: st
                        .and_then(|v| v.get("codexSynced"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                })
            }
            Err(error) => Some(ApiEvent::Failed(error)),
        },
        ApiCommand::SaveGuard { content } => {
            match put_json(
                client,
                "/api/guard",
                &serde_json::json!({ "content": content }),
            )
            .await
            {
                Ok(_) => Some(ApiEvent::GuardSaved),
                Err(error) => Some(ApiEvent::Failed(error)),
            }
        }
        ApiCommand::LoadMcpCalls {
            from,
            to,
            host,
            tool,
            ok,
        } => {
            let mut req = client
                .get(format!("{}/api/mcp-calls", base_url()))
                .query(&[("limit", "200")]);
            if !from.is_empty() {
                req = req.query(&[("from", format!("{from}T00:00:00Z"))]);
            }
            if !to.is_empty() {
                req = req.query(&[("to", format!("{to}T23:59:59Z"))]);
            }
            if !host.is_empty() {
                req = req.query(&[("host", host)]);
            }
            if !tool.is_empty() {
                req = req.query(&[("tool", tool)]);
            }
            if !ok.is_empty() {
                req = req.query(&[("ok", ok)]);
            }
            if let Ok(resp) = req.send().await
                && let Ok(body) = read_json(resp).await
            {
                let calls: Vec<McpCallRecord> = body
                    .get("calls")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                serde_json::from_value::<McpCallRecord>(item.clone()).ok()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let hosts: Vec<String> = body
                    .get("hosts")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let tools: Vec<String> = body
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                return Some(ApiEvent::McpCallsLoaded {
                    calls,
                    hosts,
                    tools,
                });
            }
            None
        }
        ApiCommand::LoadMcpCallDetail { call_id } => {
            let path = format!("/api/mcp-calls/{call_id}");
            if let Ok(body) = get_json(client, &path).await
                && let Some(call) = body.get("call")
            {
                let args = call
                    .get("args")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let result = call
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                return Some(ApiEvent::McpCallDetailLoaded {
                    call_id,
                    args,
                    result,
                });
            }
            None
        }
        ApiCommand::CheckUpdate => {
            let resp = client
                .post(format!("{}/api/system/check-update", base_url()))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    let ok = body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let current_version = body
                        .get("currentVersion")
                        .and_then(|v| v.as_str())
                        .unwrap_or(concat!("v", env!("CARGO_PKG_VERSION")))
                        .to_owned();
                    let latest_version = body
                        .get("latestVersion")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&current_version)
                        .to_owned();
                    if ok {
                        Some(ApiEvent::UpdateChecked(UpdateCheckOutcome {
                            ok: true,
                            has_update: body
                                .get("hasUpdate")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            current_version,
                            latest_version,
                            release_notes: body
                                .get("releaseNotes")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                            release_url: body
                                .get("releaseUrl")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                            error: None,
                        }))
                    } else {
                        let err = body
                            .get("error")
                            .or_else(|| body.get("stderr"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("检查更新失败")
                            .to_owned();
                        Some(ApiEvent::UpdateChecked(UpdateCheckOutcome {
                            ok: false,
                            has_update: false,
                            current_version,
                            latest_version,
                            release_notes: None,
                            release_url: None,
                            error: Some(err),
                        }))
                    }
                }
                Err(err) => Some(ApiEvent::UpdateChecked(UpdateCheckOutcome {
                    ok: false,
                    has_update: false,
                    current_version: concat!("v", env!("CARGO_PKG_VERSION")).into(),
                    latest_version: concat!("v", env!("CARGO_PKG_VERSION")).into(),
                    release_notes: None,
                    release_url: None,
                    error: Some(err.to_string()),
                })),
            }
        }
        ApiCommand::DeletePane(pane_id) => {
            let _ = client
                .delete(format!("{}/api/terminals/{pane_id}", base_url()))
                .send()
                .await;
            None
        }
        ApiCommand::Post { path, body } => match post_json(client, &path, Some(&body)).await {
            Ok(body) => Some(ApiEvent::Json(body)),
            Err(error) => Some(ApiEvent::Failed(error)),
        },
    }
}

async fn get_json(client: &reqwest::Client, path: &str) -> Result<serde_json::Value, String> {
    let response = client
        .get(format!("{}{path}", base_url()))
        .send()
        .await
        .map_err(|error| format!("连接守护进程失败：{error}"))?;
    read_json(response).await
}

async fn post_json(
    client: &reqwest::Client,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut request = client.post(format!("{}{path}", base_url()));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("连接守护进程失败：{error}"))?;
    read_json(response).await
}

async fn patch_json(
    client: &reqwest::Client,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let response = client
        .patch(format!("{}{path}", base_url()))
        .json(body)
        .send()
        .await
        .map_err(|error| format!("连接守护进程失败：{error}"))?;
    read_json(response).await
}

async fn put_json(
    client: &reqwest::Client,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let response = client
        .put(format!("{}{path}", base_url()))
        .json(body)
        .send()
        .await
        .map_err(|error| format!("连接守护进程失败：{error}"))?;
    read_json(response).await
}

async fn delete_json(client: &reqwest::Client, path: &str) -> Result<serde_json::Value, String> {
    let response = client
        .delete(format!("{}{path}", base_url()))
        .send()
        .await
        .map_err(|error| format!("连接守护进程失败：{error}"))?;
    read_json(response).await
}

async fn read_json(response: reqwest::Response) -> Result<serde_json::Value, String> {
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("解析响应失败：{error}"))?;
    let ok_flag = body.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
    if !status.is_success() || !ok_flag {
        let reason = body
            .get("error")
            .and_then(|error| error.as_str())
            .unwrap_or("请求失败");
        return Err(reason.to_owned());
    }
    Ok(body)
}

pub fn hosts_from_json(body: &serde_json::Value) -> Vec<Host> {
    body.get("hosts")
        .and_then(|hosts| hosts.as_array())
        .map(|items| items.iter().filter_map(host_from_json).collect())
        .unwrap_or_default()
}

pub fn inspections_map_from_json(body: &serde_json::Value) -> HashMap<String, Inspection> {
    let mut map = HashMap::new();
    if let Some(obj) = body.get("inspections").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            map.insert(k.clone(), inspection_from_json(v));
        }
    }
    map
}

pub fn host_from_json(value: &serde_json::Value) -> Option<Host> {
    let field = |name: &str| value.get(name).and_then(|item| item.as_str());
    let os_info = value.get("osInfo");
    let os_field = |name: &str| {
        os_info
            .and_then(|info| info.get(name))
            .and_then(|item| item.as_str())
    };
    let links: Vec<HostLink> = value
        .get("links")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| serde_json::from_value::<HostLink>(item.clone()).ok())
                .filter(|l| !l.name.is_empty() && !l.url.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(Host {
        id: field("id")?.to_owned(),
        name: field("name")?.to_owned(),
        addr: field("addr").unwrap_or_default().to_owned(),
        port: value.get("port").and_then(|v| v.as_u64()).unwrap_or(22) as u16,
        username: field("username").unwrap_or_default().to_owned(),
        kind: field("kind").unwrap_or("ssh").to_owned(),
        auth_type: field("authType").unwrap_or("password").to_owned(),
        has_password: value
            .get("hasPassword")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        has_private_key: value
            .get("hasPrivateKey")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        has_passphrase: value
            .get("hasPassphrase")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        jump_host_id: field("jumpHostId").map(str::to_owned),
        jump_host_name: field("jumpHostName").map(str::to_owned),
        status: field("status").unwrap_or("unknown").to_owned(),
        os: os_field("os").map(str::to_owned),
        pretty_name: os_field("prettyName").map(str::to_owned),
        distro_id: os_field("distroId").map(str::to_owned),
        kernel: os_field("kernel").map(str::to_owned),
        arch: os_field("arch").map(str::to_owned),
        cpu_cores: os_info
            .and_then(|info| info.get("cpuCores"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        cpu_model: os_field("cpuModel").map(str::to_owned),
        pinned: value.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false),
        favorited: value
            .get("favorited")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        archived: value
            .get("archived")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        mcp_exposed: value
            .get("mcpExposed")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        sort_order: value
            .get("sortOrder")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        role: field("role").map(str::to_owned),
        tags: value
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        links,
        latency_ms: value.get("latencyMs").and_then(|v| v.as_i64()),
        last_error: field("lastError").map(str::to_owned),
    })
}

pub fn inspection_from_json(value: &serde_json::Value) -> Inspection {
    let num = |name: &str| value.get(name).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let text = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    Inspection {
        cpu_cores: value
            .get("cpuCores")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        cpu_usage_pct: num("cpuUsagePct"),
        cpu_iowait_pct: num("cpuIowaitPct"),
        cpu_steal_pct: num("cpuStealPct"),
        cpu_model: text("cpuModel"),
        load1: num("load1"),
        load5: num("load5"),
        load15: num("load15"),
        mem_total_bytes: num("memTotalBytes"),
        mem_used_bytes: num("memUsedBytes"),
        mem_cached_bytes: num("memCachedBytes"),
        swap_total_bytes: num("swapTotalBytes"),
        swap_used_bytes: num("swapUsedBytes"),
        disk_mount: text("diskMount"),
        disk_total_bytes: num("diskTotalBytes"),
        disk_used_bytes: num("diskUsedBytes"),
        disks: value
            .get("disks")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(DiskUsage {
                            mount: item.get("mount")?.as_str()?.to_owned(),
                            total_bytes: item.get("totalBytes")?.as_f64().unwrap_or(0.0),
                            used_bytes: item.get("usedBytes")?.as_f64().unwrap_or(0.0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        net_iface: text("netIface"),
        net_rx_bps: num("netRxBps"),
        net_tx_bps: num("netTxBps"),
        net_rx_bytes_total: num("netRxBytesTotal"),
        net_tx_bytes_total: num("netTxBytesTotal"),
        uptime_seconds: num("uptimeSeconds"),
        tcp_cc: text("tcpCc"),
    }
}

/// 解析 `/api/events` SSE 推送的单条 `data:` JSON 负载（兼容 camelCase 与 snake_case 字段名）。
pub fn parse_sse_ui_event(raw_json: &str) -> Option<ApiEvent> {
    let val: serde_json::Value = serde_json::from_str(raw_json.trim()).ok()?;
    let ev_type = val.get("type")?.as_str()?;
    match ev_type {
        "revealTerminal" => {
            let host_id = val
                .get("hostId")
                .or_else(|| val.get("host_id"))
                .and_then(|v| v.as_str())?
                .to_owned();
            let pane_val = val.get("pane")?;
            let mut pane: crate::terminal::PaneMeta =
                serde_json::from_value(pane_val.clone()).ok()?;
            if pane.host_id.is_empty() {
                pane.host_id = host_id.clone();
            }
            let reason = val
                .get("reason")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            Some(ApiEvent::RevealTerminal {
                host_id,
                pane,
                reason,
            })
        }
        "showMainWindow" => Some(ApiEvent::ShowMainWindow),
        "hostsUpdated" => Some(ApiEvent::HostsUpdated),
        "openStandaloneTerminal" => {
            let host_id = val
                .get("hostId")
                .or_else(|| val.get("host_id"))
                .and_then(|v| v.as_str())?
                .to_owned();
            let pane_id = val
                .get("paneId")
                .or_else(|| val.get("pane_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            Some(ApiEvent::OpenStandaloneTerminal { host_id, pane_id })
        }
        _ => None,
    }
}

async fn sse_event_loop(client: reqwest::Client, event_tx: mpsc::Sender<ApiEvent>) {
    loop {
        let url = format!("{}/api/events", base_url());
        let resp = client
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await;

        if let Ok(mut response) = resp
            && response.status().is_success()
        {
            let mut buffer = String::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(newline_idx) = buffer.find('\n') {
                    let line = buffer[..newline_idx].trim_end_matches('\r').to_owned();
                    buffer.drain(..=newline_idx);
                    if let Some(data) = line.strip_prefix("data:") {
                        if let Some(ev) = parse_sse_ui_event(data)
                            && event_tx.send(ev).is_err()
                        {
                            return;
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse_reveal_terminal_both_camel_and_snake_case() {
        let snake = r#"{"type":"revealTerminal","host_id":"vip-hk","pane":{"id":"p-101","hostId":"vip-hk","cols":100,"rows":30,"cwd":"/var/log","running":true},"reason":"请协助输入 sudo 密码"}"#;
        let ev = parse_sse_ui_event(snake).expect("should parse snake_case host_id");
        match ev {
            ApiEvent::RevealTerminal {
                host_id,
                pane,
                reason,
            } => {
                assert_eq!(host_id, "vip-hk");
                assert_eq!(pane.id, "p-101");
                assert_eq!(pane.cwd.as_deref(), Some("/var/log"));
                assert!(pane.running);
                assert_eq!(reason.as_deref(), Some("请协助输入 sudo 密码"));
            }
            _ => panic!("expected RevealTerminal"),
        }

        let camel = r#"{"type":"revealTerminal","hostId":"us-node","pane":{"id":"p-202","cols":80,"rows":24}}"#;
        let ev2 = parse_sse_ui_event(camel).expect("should parse camelCase hostId");
        match ev2 {
            ApiEvent::RevealTerminal {
                host_id,
                pane,
                reason,
            } => {
                assert_eq!(host_id, "us-node");
                assert_eq!(pane.id, "p-202");
                assert_eq!(pane.host_id, "us-node");
                assert!(reason.is_none());
            }
            _ => panic!("expected RevealTerminal"),
        }

        let hosts_updated = r#"{"type":"hostsUpdated"}"#;
        assert!(matches!(
            parse_sse_ui_event(hosts_updated),
            Some(ApiEvent::HostsUpdated)
        ));
    }

    #[test]
    fn parses_mcp_call_records_with_null_host_and_optional_fields() {
        let raw = serde_json::json!([
            {
                "id": "c-1",
                "ts": "2026-10-18T07:23:38Z",
                "tool": "vps_exec",
                "host": "node-a",
                "args": "{\"command\":\"ls\"}",
                "ok": true,
                "durationMs": 611,
                "summary": "ok=true"
            },
            {
                "id": "c-2",
                "ts": "2026-10-18T07:20:00Z",
                "tool": "vps_list",
                "host": null,
                "args": "{}",
                "ok": true,
                "durationMs": 4,
                "summary": null
            }
        ]);
        let records: Vec<McpCallRecord> =
            serde_json::from_value(raw).expect("records with null host must deserialize");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].host, "node-a");
        assert_eq!(records[1].tool, "vps_list");
        assert_eq!(records[1].host, "");
        assert_eq!(records[1].summary, "");
    }
}

