use crate::{
    crypto::{decrypt_1shell_payload, MasterKey, OneShellCipherPayload},
    models::{
        HostLink, HostMetaPatch, HostRecord, HostUpsertPayload, OsInfo, PublicHost, LOCAL_HOST_ID,
    },
};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
use uuid::Uuid;

/// 把字符串截断到最多 `max` 个字符（按 char 边界），超出补省略号。
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// MCP 调用留痕里「参数」与「结果」正文各自的上限（按 char）。超出即截断并在正文里
/// 写明原始长度——留痕是给人看的，宁可占点磁盘，也不要让人对着半截内容猜。
const MCP_CALL_TEXT_LIMIT: usize = 256 * 1024;

/// 与 `clip` 相同，但把截断这件事写进正文本身，读取方不必额外记标志位。
fn clip_with_note(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!(
        "\n\n…（已截断：原始 {total} 字符，此处保留前 {max} 字符）"
    ));
    out
}

/// MCP 调用记录的查询条件。所有字段都是可选的，为空即不参与过滤。
#[derive(Debug, Default, Clone)]
pub struct McpCallFilter {
    pub limit: i64,
    pub tool: Option<String>,
    pub host: Option<String>,
    pub ok: Option<bool>,
    /// RFC3339 时间下界（含）
    pub from: Option<String>,
    /// RFC3339 时间上界（不含）
    pub to: Option<String>,
}

pub struct Database {
    conn: Mutex<Connection>,
    master_key: MasterKey,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyOneShellHost {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    auth_type: String,
    #[serde(default)]
    proxy_host_id: Option<String>,
    #[serde(default)]
    links: Vec<HostLink>,
    #[serde(default)]
    os_info: Option<OsInfo>,
    #[serde(default)]
    encrypted_password: Option<OneShellCipherPayload>,
    #[serde(default)]
    encrypted_private_key: Option<OneShellCipherPayload>,
    #[serde(default)]
    encrypted_passphrase: Option<OneShellCipherPayload>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

struct LegacyPref {
    repository_order: i64,
    console_order: i64,
    pinned: bool,
    role: Option<String>,
    tags: Vec<String>,
    archived: bool,
}

impl Database {
    pub fn open(data_dir: &Path, master_key: MasterKey) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("vps-switch.db");
        let conn = Connection::open(&db_path).context("打开 vps-switch.db 失败")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS hosts (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 kind TEXT NOT NULL DEFAULT 'ssh',
                 addr TEXT NOT NULL DEFAULT '',
                 port INTEGER NOT NULL DEFAULT 22,
                 username TEXT NOT NULL DEFAULT 'root',
                 auth_type TEXT NOT NULL DEFAULT 'password',
                 encrypted_password TEXT,
                 encrypted_private_key TEXT,
                 encrypted_passphrase TEXT,
                 jump_host_id TEXT,
                 role TEXT,
                 tags_json TEXT NOT NULL DEFAULT '[]',
                 links_json TEXT NOT NULL DEFAULT '[]',
                 os_json TEXT,
                 pinned INTEGER NOT NULL DEFAULT 0,
                 favorited INTEGER NOT NULL DEFAULT 0,
                 archived INTEGER NOT NULL DEFAULT 0,
                 mcp_exposed INTEGER NOT NULL DEFAULT 1,
                 sort_order INTEGER NOT NULL DEFAULT 0,
                 status TEXT NOT NULL DEFAULT 'unknown',
                 latency_ms INTEGER,
                 last_connected_at TEXT,
                 last_error TEXT,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mcp_calls (
                 id TEXT PRIMARY KEY,
                 ts TEXT NOT NULL,
                 tool TEXT NOT NULL,
                 host TEXT,
                 args TEXT NOT NULL DEFAULT '',
                 ok INTEGER NOT NULL DEFAULT 0,
                 duration_ms INTEGER NOT NULL DEFAULT 0,
                 summary TEXT NOT NULL DEFAULT '',
                 result TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_mcp_calls_ts ON mcp_calls(ts);",
        )?;
        // 兼容升级已有数据库：安全添加 favorited 与 mcp_exposed 列
        let _ = conn.execute(
            "ALTER TABLE hosts ADD COLUMN favorited INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE hosts ADD COLUMN mcp_exposed INTEGER NOT NULL DEFAULT 1",
            [],
        );
        // 兼容升级：MCP 调用留痕补上「返回给 agent 的完整结果」一列
        let _ = conn.execute(
            "ALTER TABLE mcp_calls ADD COLUMN result TEXT NOT NULL DEFAULT ''",
            [],
        );

        let db = Self {
            conn: Mutex::new(conn),
            master_key,
        };
        db.ensure_local_host()?;
        db.ensure_default_settings()?;
        Ok(db)
    }

    pub fn master_key(&self) -> &MasterKey {
        &self.master_key
    }

    fn ensure_local_host(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let exists: Option<String> = conn
            .query_row(
                "SELECT id FROM hosts WHERE id = ?1",
                params![LOCAL_HOST_ID],
                |row| row.get(0),
            )
            .optional()?;

        if exists.is_none() {
            let now = Utc::now().to_rfc3339();
            let username = std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .unwrap_or_else(|_| "local".to_string());
            let os_info = OsInfo {
                os: Some(std::env::consts::OS.to_string()),
                distro_id: Some(std::env::consts::OS.to_string()),
                version_id: None,
                pretty_name: Some(format!("{} Local", std::env::consts::OS)),
                arch: Some(std::env::consts::ARCH.to_string()),
                kernel: None,
                cpu_model: None,
                cpu_cores: std::thread::available_parallelism()
                    .ok()
                    .map(|n| n.get() as u32),
                detected_at: Some(now.clone()),
            };
            conn.execute(
                "INSERT INTO hosts (
                    id, name, kind, addr, port, username, auth_type,
                    role, tags_json, links_json, os_json,
                    pinned, archived, sort_order, status, latency_ms,
                    last_connected_at, created_at, updated_at
                ) VALUES (?1, ?2, 'local', '127.0.0.1', 0, ?3, 'local',
                    'primary', '[]', '[]', ?4,
                    1, 0, -1, 'online', 0,
                    ?5, ?5, ?5)",
                params![
                    LOCAL_HOST_ID,
                    "本机",
                    username,
                    serde_json::to_string(&os_info)?,
                    now
                ],
            )?;
        } else if !cfg!(windows) {
            let raw: Option<String> = conn.query_row(
                "SELECT os_json FROM hosts WHERE id = ?1",
                params![LOCAL_HOST_ID],
                |row| row.get(0),
            )?;
            if let Some(mut os) = raw.and_then(|s| serde_json::from_str::<OsInfo>(&s).ok()) {
                if os.os.as_deref() == Some("windows") {
                    os.os = Some(std::env::consts::OS.to_string());
                    os.distro_id = Some(std::env::consts::OS.to_string());
                    os.version_id = None;
                    os.pretty_name = Some(format!("{} Local", std::env::consts::OS));
                    os.arch = Some(std::env::consts::ARCH.to_string());
                    conn.execute(
                        "UPDATE hosts SET os_json = ?1 WHERE id = ?2",
                        params![serde_json::to_string(&os)?, LOCAL_HOST_ID],
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn count_ssh_hosts(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM hosts WHERE kind = 'ssh'", [], |row| {
                row.get(0)
            })?;
        Ok(count as usize)
    }

    fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostRecord> {
        let tags_raw: String = row.get("tags_json")?;
        let links_raw: String = row.get("links_json")?;
        let os_raw: Option<String> = row.get("os_json")?;

        let tags: Vec<String> = serde_json::from_str(&tags_raw).unwrap_or_default();
        let links: Vec<HostLink> = serde_json::from_str(&links_raw).unwrap_or_default();
        let os_info: Option<OsInfo> = os_raw.as_deref().and_then(|s| serde_json::from_str(s).ok());

        Ok(HostRecord {
            id: row.get("id")?,
            name: row.get("name")?,
            kind: row.get("kind")?,
            addr: row.get("addr")?,
            port: row.get::<_, i64>("port")? as u16,
            username: row.get("username")?,
            auth_type: row.get("auth_type")?,
            encrypted_password: row.get("encrypted_password")?,
            encrypted_private_key: row.get("encrypted_private_key")?,
            encrypted_passphrase: row.get("encrypted_passphrase")?,
            jump_host_id: row.get("jump_host_id")?,
            role: row.get("role")?,
            tags,
            links,
            os_info,
            pinned: row.get::<_, i64>("pinned")? != 0,
            favorited: row.get::<_, i64>("favorited").unwrap_or(0) != 0,
            archived: row.get::<_, i64>("archived")? != 0,
            mcp_exposed: row.get::<_, i64>("mcp_exposed").unwrap_or(1) != 0,
            sort_order: row.get("sort_order")?,
            status: row.get("status")?,
            latency_ms: row.get("latency_ms")?,
            last_connected_at: row.get("last_connected_at")?,
            last_error: row.get("last_error")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    pub fn list_records(&self) -> Result<Vec<HostRecord>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let mut stmt = conn.prepare(
            "SELECT * FROM hosts
             ORDER BY
               CASE WHEN id = 'local' THEN 0 ELSE 1 END ASC,
               pinned DESC,
               sort_order ASC,
               name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_public_hosts(&self) -> Result<Vec<PublicHost>> {
        let records = self.list_records()?;
        let (_, proxy_port) = self.get_proxy_settings();
        let name_map: HashMap<String, String> = records
            .iter()
            .map(|r| (r.id.clone(), r.name.clone()))
            .collect();

        Ok(records
            .into_iter()
            .map(|r| {
                let jump_host_name = match r.jump_host_id.as_deref() {
                    Some(crate::models::LOCAL_PROXY_JUMP_ID) => {
                        Some(format!("本机代理 :{proxy_port}"))
                    }
                    Some(jid) => name_map.get(jid).cloned(),
                    None => None,
                };
                Self::to_public(r, jump_host_name)
            })
            .collect())
    }

    pub fn get_record(&self, id: &str) -> Result<Option<HostRecord>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        conn.query_row(
            "SELECT * FROM hosts WHERE id = ?1",
            params![id],
            Self::row_to_record,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_public_host(&self, id: &str) -> Result<Option<PublicHost>> {
        let Some(rec) = self.get_record(id)? else {
            return Ok(None);
        };
        let jump_name = match rec.jump_host_id.as_deref() {
            Some(crate::models::LOCAL_PROXY_JUMP_ID) => {
                let (_, proxy_port) = self.get_proxy_settings();
                Some(format!("本机代理 :{proxy_port}"))
            }
            Some(jid) => self.get_record(jid)?.map(|j| j.name),
            None => None,
        };
        Ok(Some(Self::to_public(rec, jump_name)))
    }

    /// 读取本机代理配置 `(proxy_mode, proxy_port)`：
    /// - `proxy_mode`: `"off"` | `"per_host"` | `"global"`（默认 `"per_host"`）
    /// - `proxy_port`: 默认 `7890`
    pub fn get_proxy_settings(&self) -> (String, u16) {
        let mode = self
            .get_setting("proxy_mode")
            .ok()
            .flatten()
            .filter(|m| matches!(m.as_str(), "off" | "per_host" | "global"))
            .unwrap_or_else(|| "per_host".to_string());
        let port = self
            .get_setting("proxy_port")
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .filter(|p| *p >= 1)
            .unwrap_or(7890);
        (mode, port)
    }

    /// 解析某台 SSH 主机的连接路由：返回 `(SSH 跳板机记录, 本机代理端口)`。
    pub fn resolve_ssh_route(
        &self,
        record: &HostRecord,
    ) -> Result<(Option<HostRecord>, Option<u16>)> {
        let (mode, port) = self.get_proxy_settings();
        match record.jump_host_id.as_deref() {
            Some(crate::models::LOCAL_PROXY_JUMP_ID) => {
                if mode == "off" || port == 0 {
                    Ok((None, None))
                } else {
                    Ok((None, Some(port)))
                }
            }
            Some(jid) if !jid.is_empty() => {
                let jump = self.get_record(jid)?;
                let proxy = if mode == "global" && port > 0 {
                    Some(port)
                } else {
                    None
                };
                Ok((jump, proxy))
            }
            _ => {
                if mode == "global" && port > 0 {
                    Ok((None, Some(port)))
                } else {
                    Ok((None, None))
                }
            }
        }
    }

    fn to_public(r: HostRecord, jump_host_name: Option<String>) -> PublicHost {
        PublicHost {
            id: r.id,
            name: r.name,
            kind: r.kind,
            addr: r.addr,
            port: r.port,
            username: r.username,
            auth_type: r.auth_type,
            has_password: r.encrypted_password.is_some(),
            has_private_key: r.encrypted_private_key.is_some(),
            has_passphrase: r.encrypted_passphrase.is_some(),
            jump_host_id: r.jump_host_id,
            jump_host_name,
            role: r.role,
            tags: r.tags,
            links: r.links,
            os_info: r.os_info,
            pinned: r.pinned,
            favorited: r.favorited,
            archived: r.archived,
            mcp_exposed: r.mcp_exposed,
            sort_order: r.sort_order,
            status: r.status,
            latency_ms: r.latency_ms,
            last_connected_at: r.last_connected_at,
            last_error: r.last_error,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }

    pub fn create_host(&self, payload: HostUpsertPayload) -> Result<PublicHost> {
        let name = payload.name.trim().to_string();
        let addr = payload.addr.trim().to_string();
        if name.is_empty() || addr.is_empty() {
            return Err(anyhow!("主机名称与地址不能为空"));
        }

        let id = format!("host_{}", Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let enc_pwd = match payload.password.as_deref() {
            Some(p) if !p.is_empty() => self.master_key.encrypt(p)?,
            _ => None,
        };
        let enc_key = match payload.private_key.as_deref() {
            Some(k) if !k.trim().is_empty() => self.master_key.encrypt(k)?,
            _ => None,
        };
        let enc_pass = match payload.passphrase.as_deref() {
            Some(p) if !p.is_empty() => self.master_key.encrypt(p)?,
            _ => None,
        };

        let links = normalize_links(payload.links);
        let tags = normalize_tags(payload.tags);
        let jump_id = payload
            .jump_host_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let role = payload
            .role
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        {
            let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
            let max_order: i64 = conn
                .query_row("SELECT COALESCE(MAX(sort_order), 0) FROM hosts", [], |r| {
                    r.get(0)
                })
                .unwrap_or(0);

            conn.execute(
                "INSERT INTO hosts (
                    id, name, kind, addr, port, username, auth_type,
                    encrypted_password, encrypted_private_key, encrypted_passphrase,
                    jump_host_id, role, tags_json, links_json,
                    pinned, favorited, archived, mcp_exposed, sort_order, status, created_at, updated_at
                ) VALUES (?1, ?2, 'ssh', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 'unknown', ?19, ?19)",
                params![
                    id,
                    name,
                    addr,
                    payload.port as i64,
                    payload.username.trim(),
                    payload.auth_type,
                    enc_pwd,
                    enc_key,
                    enc_pass,
                    jump_id,
                    role,
                    serde_json::to_string(&tags)?,
                    serde_json::to_string(&links)?,
                    if payload.pinned { 1 } else { 0 },
                    if payload.favorited { 1 } else { 0 },
                    if payload.archived { 1 } else { 0 },
                    if payload.mcp_exposed { 1 } else { 0 },
                    max_order + 1,
                    now
                ],
            )?;
        }

        self.get_public_host(&id)?
            .ok_or_else(|| anyhow!("创建主机后读取失败"))
    }

    pub fn update_host(&self, id: &str, payload: HostUpsertPayload) -> Result<PublicHost> {
        let existing = self
            .get_record(id)?
            .ok_or_else(|| anyhow!("主机不存在: {id}"))?;

        let now = Utc::now().to_rfc3339();
        let links = normalize_links(payload.links);
        let tags = normalize_tags(payload.tags);
        let role = payload
            .role
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if existing.kind == "local" {
            {
                let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
                conn.execute(
                    "UPDATE hosts SET name = ?1, role = ?2, tags_json = ?3, links_json = ?4, mcp_exposed = ?5, updated_at = ?6 WHERE id = ?7",
                    params![
                        if payload.name.trim().is_empty() {
                            &existing.name
                        } else {
                            payload.name.trim()
                        },
                        role,
                        serde_json::to_string(&tags)?,
                        serde_json::to_string(&links)?,
                        if payload.mcp_exposed { 1 } else { 0 },
                        now,
                        id
                    ],
                )?;
            }
            return self
                .get_public_host(id)?
                .ok_or_else(|| anyhow!("更新本机后读取失败"));
        }

        let enc_pwd = match payload.password.as_deref() {
            Some(p) if !p.is_empty() => self.master_key.encrypt(p)?,
            _ => existing.encrypted_password,
        };
        let enc_key = match payload.private_key.as_deref() {
            Some(k) if !k.trim().is_empty() => self.master_key.encrypt(k)?,
            _ => existing.encrypted_private_key,
        };
        let enc_pass = match payload.passphrase.as_deref() {
            Some(p) if !p.is_empty() => self.master_key.encrypt(p)?,
            _ => existing.encrypted_passphrase,
        };
        let jump_id = payload
            .jump_host_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != id);

        {
            let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
            conn.execute(
                "UPDATE hosts SET
                    name = ?1, addr = ?2, port = ?3, username = ?4, auth_type = ?5,
                    encrypted_password = ?6, encrypted_private_key = ?7, encrypted_passphrase = ?8,
                    jump_host_id = ?9, role = ?10, tags_json = ?11, links_json = ?12,
                    pinned = ?13, favorited = ?14, archived = ?15, mcp_exposed = ?16, updated_at = ?17
                 WHERE id = ?18",
                params![
                    payload.name.trim(),
                    payload.addr.trim(),
                    payload.port as i64,
                    payload.username.trim(),
                    payload.auth_type,
                    enc_pwd,
                    enc_key,
                    enc_pass,
                    jump_id,
                    role,
                    serde_json::to_string(&tags)?,
                    serde_json::to_string(&links)?,
                    if payload.pinned { 1 } else { 0 },
                    if payload.favorited { 1 } else { 0 },
                    if payload.archived { 1 } else { 0 },
                    if payload.mcp_exposed { 1 } else { 0 },
                    now,
                    id
                ],
            )?;
        }

        self.get_public_host(id)?
            .ok_or_else(|| anyhow!("更新主机后读取失败"))
    }

    pub fn patch_meta(&self, id: &str, patch: HostMetaPatch) -> Result<PublicHost> {
        let existing = self
            .get_record(id)?
            .ok_or_else(|| anyhow!("主机不存在: {id}"))?;
        let now = Utc::now().to_rfc3339();
        let pinned = patch.pinned.unwrap_or(existing.pinned);
        let favorited = patch.favorited.unwrap_or(existing.favorited);
        let archived = patch.archived.unwrap_or(existing.archived);
        let mcp_exposed = patch.mcp_exposed.unwrap_or(existing.mcp_exposed);
        let role = match patch.role {
            Some(r) => {
                let t = r.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            }
            None => existing.role,
        };
        let tags = patch.tags.map(normalize_tags).unwrap_or(existing.tags);
        let links = patch.links.map(normalize_links).unwrap_or(existing.links);

        {
            let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
            conn.execute(
                "UPDATE hosts SET pinned = ?1, favorited = ?2, archived = ?3, mcp_exposed = ?4, role = ?5, tags_json = ?6, links_json = ?7, updated_at = ?8 WHERE id = ?9",
                params![
                    if pinned { 1 } else { 0 },
                    if favorited { 1 } else { 0 },
                    if archived { 1 } else { 0 },
                    if mcp_exposed { 1 } else { 0 },
                    role,
                    serde_json::to_string(&tags)?,
                    serde_json::to_string(&links)?,
                    now,
                    id
                ],
            )?;
        }

        self.get_public_host(id)?
            .ok_or_else(|| anyhow!("更新元数据后读取失败"))
    }

    pub fn set_mcp_exposed_batch(
        &self,
        exposed: bool,
        host_ids: Option<&[String]>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let val = if exposed { 1i64 } else { 0i64 };
        let mut conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        match host_ids {
            Some(ids) => {
                let tx = conn.transaction()?;
                for id in ids {
                    tx.execute(
                        "UPDATE hosts SET mcp_exposed = ?1, updated_at = ?2 WHERE id = ?3",
                        params![val, &now, id],
                    )?;
                }
                tx.commit()?;
            }
            None => {
                conn.execute(
                    "UPDATE hosts SET mcp_exposed = ?1, updated_at = ?2",
                    params![val, &now],
                )?;
            }
        }
        Ok(())
    }

    pub fn reorder(&self, ordered_ids: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let tx = conn.transaction()?;
        for (idx, id) in ordered_ids.iter().enumerate() {
            if id == LOCAL_HOST_ID {
                continue;
            }
            tx.execute(
                "UPDATE hosts SET sort_order = ?1 WHERE id = ?2",
                params![idx as i64, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_host(&self, id: &str) -> Result<()> {
        if id == LOCAL_HOST_ID {
            return Err(anyhow!("本机节点不能删除"));
        }
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        conn.execute(
            "UPDATE hosts SET jump_host_id = NULL WHERE jump_host_id = ?1",
            params![id],
        )?;
        conn.execute("DELETE FROM hosts WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 记录一次检测结果。`status` 取 "online" | "offline" | "unknown"：
    /// - online：SSH 已连上（探针是否采到数据不影响这个结论）；
    /// - offline：确定连不上（连接被拒绝 / 认证失败）；
    /// - unknown：无法判定（连接超时这类——可能只是线路慢，不直接判死）。
    pub fn update_check_result(
        &self,
        id: &str,
        status: &str,
        latency_ms: Option<i64>,
        os_info: Option<OsInfo>,
        error: Option<String>,
    ) -> Result<PublicHost> {
        let existing = self
            .get_record(id)?
            .ok_or_else(|| anyhow!("主机不存在: {id}"))?;
        let now = Utc::now().to_rfc3339();
        let status = match status {
            "online" => "online",
            "offline" => "offline",
            _ => "unknown",
        };
        let merged_os = os_info.or(existing.os_info);
        let merged_latency = if status == "online" {
            latency_ms.or(existing.latency_ms)
        } else {
            latency_ms
        };
        let last_connected = if status == "online" {
            Some(now.clone())
        } else {
            existing.last_connected_at
        };

        {
            let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
            conn.execute(
                "UPDATE hosts SET
                    status = ?1,
                    latency_ms = ?2,
                    os_json = ?3,
                    last_connected_at = ?4,
                    last_error = ?5,
                    updated_at = ?6
                 WHERE id = ?7",
                params![
                    status,
                    merged_latency,
                    merged_os
                        .as_ref()
                        .and_then(|o| serde_json::to_string(o).ok()),
                    last_connected,
                    error,
                    now,
                    id
                ],
            )?;
        }

        self.get_public_host(id)?
            .ok_or_else(|| anyhow!("更新检测结果后读取失败"))
    }

    /// 从本机旧版 1Shell (%APPDATA%\1Shell\data\1shell.db) 导入全部主机与偏好
    pub fn import_from_1shell(&self) -> Result<usize> {
        let db_path = find_1shell_db_path().ok_or_else(|| {
            anyhow!("未找到本机 1Shell 数据库 (%APPDATA%\\1Shell\\data\\1shell.db)")
        })?;

        let src_conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("只读打开 1Shell 数据库失败: {}", db_path.display()))?;

        let mut prefs: HashMap<String, LegacyPref> = HashMap::new();
        if let Ok(mut pref_stmt) = src_conn.prepare(
            "SELECT host_id, repository_order, console_order, pinned, role, tags_json, archived FROM host_preferences",
        ) {
            if let Ok(rows) = pref_stmt.query_map([], |row| {
                let host_id: String = row.get(0)?;
                let repo_order: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
                let console_order: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
                let pinned: bool = row.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0;
                let role: Option<String> = row.get(4)?;
                let tags_raw: Option<String> = row.get(5)?;
                let archived: bool = row.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0;
                let tags: Vec<String> = tags_raw
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                Ok((
                    host_id,
                    LegacyPref {
                        repository_order: repo_order,
                        console_order,
                        pinned,
                        role,
                        tags,
                        archived,
                    },
                ))
            }) {
                for item in rows.flatten() {
                    prefs.insert(item.0, item.1);
                }
            }
        }

        let mut stmt = src_conn.prepare("SELECT data FROM hosts ORDER BY created_at ASC")?;
        let raw_rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut imported = 0usize;
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let tx = conn.transaction()?;

        for (idx, raw) in raw_rows.into_iter().enumerate() {
            let Ok(legacy) = serde_json::from_str::<LegacyOneShellHost>(&raw) else {
                continue;
            };
            if legacy.id == LOCAL_HOST_ID || legacy.kind == "local" {
                continue;
            }

            let plain_pwd = legacy
                .encrypted_password
                .as_ref()
                .and_then(|p| decrypt_1shell_payload(p, None).ok().flatten());
            let plain_key = legacy
                .encrypted_private_key
                .as_ref()
                .and_then(|p| decrypt_1shell_payload(p, None).ok().flatten());
            let plain_pass = legacy
                .encrypted_passphrase
                .as_ref()
                .and_then(|p| decrypt_1shell_payload(p, None).ok().flatten());

            let enc_pwd = match plain_pwd.as_deref() {
                Some(s) => self.master_key.encrypt(s)?,
                None => None,
            };
            let enc_key = match plain_key.as_deref() {
                Some(s) => self.master_key.encrypt(s)?,
                None => None,
            };
            let enc_pass = match plain_pass.as_deref() {
                Some(s) => self.master_key.encrypt(s)?,
                None => None,
            };

            let pref = prefs.get(&legacy.id);
            let sort_order = pref
                .map(|p| {
                    if p.repository_order > 0 {
                        p.repository_order
                    } else if p.console_order > 0 {
                        p.console_order
                    } else {
                        idx as i64
                    }
                })
                .unwrap_or(idx as i64);
            let pinned = pref.map(|p| p.pinned).unwrap_or(false);
            let archived = pref.map(|p| p.archived).unwrap_or(false);
            let role = pref.and_then(|p| p.role.clone());
            let tags = pref.map(|p| p.tags.clone()).unwrap_or_default();
            let links = normalize_links(legacy.links);
            let os_json = legacy
                .os_info
                .as_ref()
                .and_then(|o| serde_json::to_string(o).ok());
            let created_at = legacy.created_at.unwrap_or_else(|| now.clone());
            let updated_at = legacy.updated_at.unwrap_or_else(|| now.clone());

            tx.execute(
                "INSERT INTO hosts (
                    id, name, kind, addr, port, username, auth_type,
                    encrypted_password, encrypted_private_key, encrypted_passphrase,
                    jump_host_id, role, tags_json, links_json, os_json,
                    pinned, archived, sort_order, status, created_at, updated_at
                ) VALUES (?1, ?2, 'ssh', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, 'unknown', ?18, ?19)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    addr = excluded.addr,
                    port = excluded.port,
                    username = excluded.username,
                    auth_type = excluded.auth_type,
                    encrypted_password = COALESCE(excluded.encrypted_password, hosts.encrypted_password),
                    encrypted_private_key = COALESCE(excluded.encrypted_private_key, hosts.encrypted_private_key),
                    encrypted_passphrase = COALESCE(excluded.encrypted_passphrase, hosts.encrypted_passphrase),
                    jump_host_id = excluded.jump_host_id,
                    role = COALESCE(hosts.role, excluded.role),
                    os_json = COALESCE(hosts.os_json, excluded.os_json),
                    updated_at = excluded.updated_at",
                params![
                    legacy.id,
                    legacy.name,
                    legacy.host,
                    legacy.port.unwrap_or(22) as i64,
                    if legacy.username.is_empty() {
                        "root".to_string()
                    } else {
                        legacy.username
                    },
                    if legacy.auth_type.is_empty() {
                        "password".to_string()
                    } else {
                        legacy.auth_type
                    },
                    enc_pwd,
                    enc_key,
                    enc_pass,
                    legacy.proxy_host_id.filter(|s| !s.is_empty()),
                    role,
                    serde_json::to_string(&tags)?,
                    serde_json::to_string(&links)?,
                    os_json,
                    if pinned { 1 } else { 0 },
                    if archived { 1 } else { 0 },
                    sort_order,
                    created_at,
                    updated_at
                ],
            )?;

            imported += 1;
        }

        tx.commit()?;
        Ok(imported)
    }

    // ==================== 系统设置、Web 登录鉴权与 MCP API Key ====================

    fn ensure_default_settings(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let has_key: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'mcp_api_key'",
                [],
                |r| r.get(0),
            )
            .optional()?;

        if has_key.is_none() {
            let api_key = generate_api_key();
            conn.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES ('mcp_api_key', ?1)",
                params![api_key],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES ('mcp_enabled', 'true')",
                [],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES ('mcp_require_api_key', 'true')",
                [],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES ('auth_enabled', ?1)",
                params![if default_auth_enabled() {
                    "true"
                } else {
                    "false"
                }],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES ('admin_username', 'admin')",
                [],
            )?;
        }
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 记录一次 MCP 工具调用（全程留痕：工具、目标主机、参数、成败、耗时、结果摘要、完整结果）。
    /// 只覆盖 MCP 这条链——这是外部 agent 唯一能下达命令的通道，其余内部执行不入库。
    pub fn record_mcp_call(
        &self,
        tool: &str,
        host: Option<&str>,
        args: &str,
        result: &str,
        ok: bool,
        duration_ms: u64,
        summary: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        conn.execute(
            "INSERT INTO mcp_calls (id, ts, tool, host, args, ok, duration_ms, summary, result)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                Uuid::new_v4().to_string(),
                Utc::now().to_rfc3339(),
                tool,
                host,
                clip_with_note(args, MCP_CALL_TEXT_LIMIT),
                if ok { 1 } else { 0 },
                duration_ms as i64,
                clip(summary, 2000),
                clip_with_note(result, MCP_CALL_TEXT_LIMIT),
            ],
        )?;
        Ok(())
    }

    /// 按条件读取 MCP 调用记录（倒序）。列表只回预览，正文由 `get_mcp_call` 单条取，
    /// 免得一次拉几百条把上百 KB 的输出全灌进浏览器。
    pub fn list_mcp_calls(&self, f: &McpCallFilter) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let mut stmt = conn.prepare(
            "SELECT id, ts, tool, host, ok, duration_ms, summary, args
             FROM mcp_calls
             WHERE (?1 IS NULL OR tool = ?1)
               AND (?2 IS NULL OR host = ?2 OR instr(',' || host || ',', ',' || ?2 || ',') > 0)
               AND (?3 IS NULL OR ok = ?3)
               AND (?4 IS NULL OR ts >= ?4)
               AND (?5 IS NULL OR ts < ?5)
             ORDER BY ts DESC LIMIT ?6",
        )?;
        let ok_flag = f.ok.map(|b| if b { 1i64 } else { 0i64 });
        let rows = stmt
            .query_map(
                params![
                    f.tool,
                    f.host,
                    ok_flag,
                    f.from,
                    f.to,
                    f.limit.clamp(1, 5000)
                ],
                |r| {
                    let args: String = r.get(7)?;
                    Ok(serde_json::json!({
                        "id": r.get::<_, String>(0)?,
                        "ts": r.get::<_, String>(1)?,
                        "tool": r.get::<_, String>(2)?,
                        "host": r.get::<_, Option<String>>(3)?,
                        "ok": r.get::<_, i64>(4)? != 0,
                        "durationMs": r.get::<_, i64>(5)?,
                        "summary": r.get::<_, String>(6)?,
                        "args": clip(&args, 400),
                    }))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 一条 MCP 调用记录的完整正文（参数与返回结果原文）。
    pub fn get_mcp_call(&self, id: &str) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let row = conn
            .query_row(
                "SELECT id, ts, tool, host, ok, duration_ms, summary, args, result
                 FROM mcp_calls WHERE id = ?1",
                params![id],
                |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, String>(0)?,
                        "ts": r.get::<_, String>(1)?,
                        "tool": r.get::<_, String>(2)?,
                        "host": r.get::<_, Option<String>>(3)?,
                        "ok": r.get::<_, i64>(4)? != 0,
                        "durationMs": r.get::<_, i64>(5)?,
                        "summary": r.get::<_, String>(6)?,
                        "args": r.get::<_, String>(7)?,
                        "result": r.get::<_, String>(8)?,
                    }))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 记录里出现过的工具与主机名，供筛选下拉框使用（取全表，不随筛选条件收缩）。
    pub fn mcp_call_facets(&self) -> Result<(Vec<String>, Vec<String>)> {
        let conn = self.conn.lock().map_err(|_| anyhow!("数据库锁异常"))?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT tool FROM mcp_calls WHERE tool <> '' ORDER BY tool COLLATE NOCASE",
        )?;
        let tools = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // host 列可能是 "a,b,c" 这种逗号串（批量调用），这里摊平成单个主机名去重。
        let mut stmt = conn.prepare("SELECT host FROM mcp_calls WHERE host IS NOT NULL")?;
        let raw = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut hosts: Vec<String> = raw
            .iter()
            .flat_map(|s| s.split(','))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        hosts.sort_by_key(|s| s.to_lowercase());
        hosts.dedup();
        Ok((hosts, tools))
    }

    pub fn get_app_settings(&self) -> Result<AppSettings> {
        let is_vps_mode = default_auth_enabled();

        let auth_enabled = match self.get_setting("auth_enabled")? {
            Some(v) => v == "true",
            None => {
                // Windows 本机默认关闭，VPS/Linux 部署（或绑定 0.0.0.0）默认开启
                let default_enabled = is_vps_mode;
                self.set_setting(
                    "auth_enabled",
                    if default_enabled { "true" } else { "false" },
                )?;
                default_enabled
            }
        };

        let admin_username = match self.get_setting("admin_username")? {
            Some(s) if !s.is_empty() => s,
            _ => {
                let u = "admin".to_string();
                let _ = self.set_setting("admin_username", &u);
                u
            }
        };

        let has_admin_password = match self.get_setting("admin_password_enc")? {
            Some(s) if !s.is_empty() => true,
            _ => {
                // 若是 VPS 部署且从未设置过密码，自动初始化一个初始密码 "vps-switch"
                if is_vps_mode {
                    if let Some(enc) = self.master_key.encrypt("vps-switch")? {
                        self.set_setting("admin_password_enc", &enc)?;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        };

        let mcp_enabled = self
            .get_setting("mcp_enabled")?
            .map(|v| v != "false")
            .unwrap_or(true);
        let mcp_require_api_key = self
            .get_setting("mcp_require_api_key")?
            .map(|v| v != "false")
            .unwrap_or(true);
        let mcp_api_key = match self.get_setting("mcp_api_key")? {
            Some(k) if !k.is_empty() => k,
            _ => {
                let k = generate_api_key();
                self.set_setting("mcp_api_key", &k)?;
                k
            }
        };
        let mcp_enabled_tools = self.get_enabled_mcp_tools();

        Ok(AppSettings {
            auth_enabled,
            admin_username,
            has_admin_password,
            mcp_enabled,
            mcp_require_api_key,
            mcp_api_key,
            mcp_enabled_tools,
        })
    }

    pub fn get_enabled_mcp_tools(&self) -> Vec<String> {
        match self.get_setting("mcp_enabled_tools") {
            Ok(Some(raw)) => match serde_json::from_str::<Vec<String>>(&raw) {
                Ok(list) => normalize_mcp_tools(&list),
                Err(_) => default_mcp_tools(),
            },
            _ => default_mcp_tools(),
        }
    }

    pub fn set_enabled_mcp_tools(&self, tools: &[String]) -> Result<Vec<String>> {
        let normalized = normalize_mcp_tools(tools);
        self.set_setting("mcp_enabled_tools", &serde_json::to_string(&normalized)?)?;
        Ok(normalized)
    }

    pub fn update_auth_settings(
        &self,
        auth_enabled: bool,
        username: &str,
        new_password: Option<&str>,
    ) -> Result<AppSettings> {
        let clean_user = username.trim();
        if clean_user.is_empty() {
            return Err(anyhow!("管理员账号不能为空"));
        }

        if let Some(pwd) = new_password.filter(|s| !s.is_empty()) {
            if let Some(enc) = self.master_key.encrypt(pwd)? {
                self.set_setting("admin_password_enc", &enc)?;
            }
        }

        let mut final_auth_enabled = auth_enabled;
        if new_password.filter(|s| !s.is_empty()).is_some() {
            // 输入并设置了新密码，自动开启密码保护
            final_auth_enabled = true;
        }

        if final_auth_enabled {
            let has_pwd = self
                .get_setting("admin_password_enc")?
                .filter(|s| !s.is_empty())
                .is_some();
            if !has_pwd {
                return Err(anyhow!("开启密码前请先设置管理员密码"));
            }
        }

        self.set_setting("admin_username", clean_user)?;
        self.set_setting(
            "auth_enabled",
            if final_auth_enabled { "true" } else { "false" },
        )?;
        self.get_app_settings()
    }

    pub fn verify_admin_credentials(&self, username: &str, password: &str) -> Result<bool> {
        let expected_user = self
            .get_setting("admin_username")?
            .unwrap_or_else(|| "admin".to_string());
        if username.trim() != expected_user {
            return Ok(false);
        }
        let enc = self.get_setting("admin_password_enc")?;
        let Some(stored_pwd) = self.master_key.decrypt(enc.as_deref())? else {
            return Ok(false);
        };
        Ok(stored_pwd == password)
    }

    pub fn update_mcp_settings(
        &self,
        mcp_enabled: Option<bool>,
        mcp_require_api_key: Option<bool>,
        regenerate_key: bool,
        mcp_enabled_tools: Option<&[String]>,
    ) -> Result<AppSettings> {
        if let Some(en) = mcp_enabled {
            self.set_setting("mcp_enabled", if en { "true" } else { "false" })?;
        }
        if let Some(req) = mcp_require_api_key {
            self.set_setting(
                "mcp_require_api_key",
                if req { "true" } else { "false" },
            )?;
        }
        if regenerate_key {
            let new_key = generate_api_key();
            self.set_setting("mcp_api_key", &new_key)?;
        }
        if let Some(tools) = mcp_enabled_tools {
            self.set_enabled_mcp_tools(tools)?;
        }
        self.get_app_settings()
    }
}

pub const ALL_MCP_TOOLS: [&str; 7] = [
    "vps_list",
    "vps_inspect",
    "vps_exec",
    "vps_terminal",
    "vps_fs",
    "vps_transfer",
    "vps_task",
];

fn default_mcp_tools() -> Vec<String> {
    ALL_MCP_TOOLS.iter().map(|s| (*s).to_string()).collect()
}

fn normalize_mcp_tools(tools: &[String]) -> Vec<String> {
    ALL_MCP_TOOLS
        .iter()
        .filter(|&&known| tools.iter().any(|t| t.trim() == known))
        .map(|&s| s.to_string())
        .collect()
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub auth_enabled: bool,
    pub admin_username: String,
    pub has_admin_password: bool,
    pub mcp_enabled: bool,
    pub mcp_require_api_key: bool,
    pub mcp_api_key: String,
    pub mcp_enabled_tools: Vec<String>,
}

fn default_auth_enabled() -> bool {
    !cfg!(windows)
        || std::env::var("HOST")
            .ok()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .map(|ip| !ip.is_loopback())
            .unwrap_or(false)
}

fn generate_api_key() -> String {
    format!(
        "vpsk_{}{}",
        Uuid::new_v4().simple(),
        &Uuid::new_v4().simple().to_string()[..12]
    )
}

/// 1Shell 的数据库只认它在系统里的正规位置：`%APPDATA%\1Shell\data\1shell.db`。
/// 这里曾经挂过一条指向某个开发机绝对路径的回退，既把构建机的目录结构带进了发布的 exe，
/// 又会在 APPDATA 那份不在时静默读到一份陈旧副本——两者都不要。
fn find_1shell_db_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    let candidate = PathBuf::from(appdata)
        .join("1Shell")
        .join("data")
        .join("1shell.db");
    candidate.exists().then_some(candidate)
}

fn normalize_links(links: Vec<HostLink>) -> Vec<HostLink> {
    links
        .into_iter()
        .filter(|l| !l.name.trim().is_empty() && !l.url.trim().is_empty())
        .map(|l| HostLink {
            id: if l.id.trim().is_empty() {
                format!("link_{}", Uuid::new_v4())
            } else {
                l.id.trim().to_string()
            },
            name: l.name.trim().to_string(),
            url: l.url.trim().to_string(),
            description: l.description.trim().to_string(),
        })
        .collect()
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for t in tags {
        let trimmed = t.trim().to_string();
        if !trimmed.is_empty() && !out.contains(&trimmed) {
            out.push(trimmed);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_database() -> (PathBuf, Database) {
        let dir = std::env::temp_dir().join(format!("vps-switch-db-test-{}", Uuid::new_v4()));
        let key = MasterKey::load_or_create(&dir).unwrap();
        let db = Database::open(&dir, key).unwrap();
        (dir, db)
    }

    #[test]
    fn first_start_auth_and_local_metadata_match_platform() {
        let (dir, db) = open_test_database();
        let settings = db.get_app_settings().unwrap();
        if !cfg!(windows) {
            assert!(settings.auth_enabled);
            assert!(settings.has_admin_password);
            assert!(db.verify_admin_credentials("admin", "vps-switch").unwrap());
        } else if std::env::var("HOST").is_err() {
            assert!(!settings.auth_enabled);
        }
        let local = db.get_record(LOCAL_HOST_ID).unwrap().unwrap();
        assert_eq!(
            local.os_info.unwrap().os.as_deref(),
            Some(std::env::consts::OS)
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_auth_choice_survives_restart() {
        let (dir, db) = open_test_database();
        db.set_setting("auth_enabled", "false").unwrap();
        drop(db);
        let db = Database::open(&dir, MasterKey::load_or_create(&dir).unwrap()).unwrap();
        assert!(!db.get_app_settings().unwrap().auth_enabled);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_reopen_repairs_legacy_windows_local_metadata() {
        let (dir, db) = open_test_database();
        db.conn.lock().unwrap().execute(
            "UPDATE hosts SET os_json = ?1 WHERE id = ?2",
            params![r#"{"os":"windows","distroId":"windows","versionId":"11","prettyName":"Windows 11 Local"}"#, LOCAL_HOST_ID]
        ).unwrap();
        drop(db);
        let db = Database::open(&dir, MasterKey::load_or_create(&dir).unwrap()).unwrap();
        let os = db
            .get_record(LOCAL_HOST_ID)
            .unwrap()
            .unwrap()
            .os_info
            .unwrap();
        assert_eq!(os.os.as_deref(), Some("linux"));
        assert_eq!(os.version_id, None);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
