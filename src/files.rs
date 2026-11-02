use crate::{
    db::Database,
    models::LOCAL_HOST_ID,
    ssh::SshPool,
};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use russh_sftp::{client::SftpSession, protocol::OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Instant, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const MAX_TEXT_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;
/// 流式读写块大小：1 MiB（喂给 russh-sftp 内部自动切分为 64 路 255KiB 并发流水线）
const STREAM_BUF_SIZE: usize = 1024 * 1024;
/// 失速看门狗超时：若连续 12 秒没有任何新字节推进，立即判定链路僵死并强制中断，防止拖死 Agent
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub permissions: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirListing {
    pub path: String,
    pub parent: Option<String>,
    pub items: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReadResult {
    pub path: String,
    pub content: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferTask {
    pub id: String,
    pub host_id: String,
    pub host_name: String,
    pub direction: String, // "upload" | "download"
    pub filename: String,
    pub local_path: Option<String>,
    pub remote_path: String,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub speed_bps: u64,
    pub state: String, // "running" | "completed" | "failed"
    pub error: Option<String>,
    /// 本次是从残片的第几字节接着传的（0 = 从头传）
    pub resumed_from: u64,
    /// 传输内容的 SHA-256（十六进制小写）
    pub sha256: Option<String>,
    /// 闭环校验结果：Some(true)=两端哈希一致；Some(false)=不一致（任务已判失败）；
    /// None=未校验（如哈希命令不可用，此时 sha256 为客户端侧增量哈希，仅作记录）
    pub verified: Option<bool>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

/// 传输任务表与顺序表由同一把锁持有：两把锁交叉获取曾构成 AB-BA 死锁
/// （create_task 持 order 取 tasks，clear_finished 持 tasks 取 order），单锁后该问题不复存在。
#[derive(Default)]
struct TransferStore {
    tasks: HashMap<String, TransferTask>,
    order: Vec<String>,
}

/// 统一传输队列管理器（Web UI 与未来板块六 MCP 工具共用同一套传输状态与引擎）
#[derive(Default)]
pub struct TransferManager {
    store: Mutex<TransferStore>,
}

impl TransferManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_task(
        &self,
        host_id: &str,
        host_name: &str,
        direction: &str,
        filename: &str,
        local_path: Option<String>,
        remote_path: &str,
        total_bytes: u64,
    ) -> String {
        let id = format!("tr_{}", Uuid::new_v4());
        let task = TransferTask {
            id: id.clone(),
            host_id: host_id.to_string(),
            host_name: host_name.to_string(),
            direction: direction.to_string(),
            filename: filename.to_string(),
            local_path,
            remote_path: remote_path.to_string(),
            total_bytes,
            transferred_bytes: 0,
            speed_bps: 0,
            state: "running".to_string(),
            error: None,
            resumed_from: 0,
            sha256: None,
            verified: None,
            started_at: Utc::now().to_rfc3339(),
            ended_at: None,
        };
        let mut store = self.store.lock().unwrap();
        store.tasks.insert(id.clone(), task);
        store.order.insert(0, id.clone());
        if store.order.len() > 100 {
            if let Some(old_id) = store.order.pop() {
                store.tasks.remove(&old_id);
            }
        }
        id
    }

    pub fn update_progress(&self, id: &str, transferred: u64, total: u64, elapsed_secs: f64) {
        if let Some(task) = self.store.lock().unwrap().tasks.get_mut(id) {
            task.transferred_bytes = transferred;
            if total > 0 {
                task.total_bytes = total;
            }
            if elapsed_secs > 0.05 {
                // 速率只算本次真正搬动的字节：续传跳过的那段不是这次的功劳
                let moved = transferred.saturating_sub(task.resumed_from);
                task.speed_bps = (moved as f64 / elapsed_secs) as u64;
            }
        }
    }

    /// 标记本次从残片的第几字节续传（残片对账通过后才调用）
    pub fn set_resume(&self, id: &str, resumed_from: u64) {
        if let Some(task) = self.store.lock().unwrap().tasks.get_mut(id) {
            task.resumed_from = resumed_from;
            task.transferred_bytes = resumed_from;
        }
    }

    pub fn finish_task(&self, id: &str, final_bytes: u64, elapsed_secs: f64) {
        if let Some(task) = self.store.lock().unwrap().tasks.get_mut(id) {
            task.transferred_bytes = final_bytes;
            if task.total_bytes == 0 {
                task.total_bytes = final_bytes;
            }
            if elapsed_secs > 0.01 {
                let moved = final_bytes.saturating_sub(task.resumed_from);
                task.speed_bps = (moved as f64 / elapsed_secs) as u64;
            }
            task.state = "completed".to_string();
            task.ended_at = Some(Utc::now().to_rfc3339());
        }
    }

    pub fn fail_task(&self, id: &str, err_msg: impl ToString) {
        if let Some(task) = self.store.lock().unwrap().tasks.get_mut(id) {
            task.state = "failed".to_string();
            task.error = Some(err_msg.to_string());
            task.ended_at = Some(Utc::now().to_rfc3339());
        }
    }

    pub fn set_verification(&self, id: &str, sha256: Option<String>, verified: Option<bool>) {
        if let Some(task) = self.store.lock().unwrap().tasks.get_mut(id) {
            task.sha256 = sha256;
            task.verified = verified;
        }
    }

    pub fn list_tasks(&self, host_id: Option<&str>) -> Vec<TransferTask> {
        let store = self.store.lock().unwrap();
        store
            .order
            .iter()
            .filter_map(|id| store.tasks.get(id))
            .filter(|t| host_id.map(|h| t.host_id == h).unwrap_or(true))
            .cloned()
            .collect()
    }

    pub fn clear_finished(&self) {
        let mut store = self.store.lock().unwrap();
        let keep: std::collections::HashSet<String> = store
            .tasks
            .iter()
            .filter(|(_, t)| t.state == "running")
            .map(|(id, _)| id.clone())
            .collect();
        store.tasks.retain(|id, _| keep.contains(id));
        store.order.retain(|id| keep.contains(id));
    }
}

// ==================== 统一目录浏览与文件操作（本机 + 远端 SFTP） ====================

pub async fn list_dir(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    dir_path: Option<&str>,
    force_refresh: bool,
) -> Result<DirListing> {
    if host_id == LOCAL_HOST_ID {
        return list_local_dir(dir_path).await;
    }

    let req_key = dir_path.unwrap_or("").trim();
    if !force_refresh {
        if let Some(cached) = pool
            .get_cached_dir(host_id, req_key, std::time::Duration::from_secs(45))
            .await
        {
            return Ok(cached);
        }
    }

    let res = match list_remote_dir_once(pool, db, host_id, dir_path).await {
        Ok(r) => r,
        Err(_) => {
            pool.invalidate_sftp(host_id).await;
            list_remote_dir_once(pool, db, host_id, dir_path).await?
        }
    };
    let _ = db.update_check_result(host_id, "online", None, None, None);
    pool.put_cached_dir(host_id, req_key, &res).await;
    Ok(res)
}

async fn list_local_dir(dir_path: Option<&str>) -> Result<DirListing> {
    let raw = dir_path.unwrap_or("").trim();

    // Windows 顶层虚拟根目录 "drives"（此电脑）：列出 C:/ D:/ E:/ 等全部可用盘符
    if cfg!(windows) && (raw.eq_ignore_ascii_case("drives") || raw == "/" || raw == "\\") {
        let mut drives = Vec::new();
        for letter in b'A'..=b'Z' {
            let drive_str = format!("{}:/", letter as char);
            let p = PathBuf::from(&drive_str);
            if tokio::fs::metadata(&p).await.is_ok() {
                drives.push(FileEntry {
                    name: format!("本地磁盘 ({}:)", letter as char),
                    path: drive_str,
                    is_dir: true,
                    size: 0,
                    mtime: 0,
                    permissions: None,
                });
            }
        }
        return Ok(DirListing {
            path: "drives".to_string(),
            parent: None,
            items: drives,
        });
    }

    let target: PathBuf = if raw.is_empty() || raw == "~" {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(if cfg!(windows) { "C:/" } else { "/" }))
    } else if raw.len() == 2 && raw.as_bytes()[1] == b':' {
        PathBuf::from(format!("{raw}/"))
    } else {
        PathBuf::from(raw)
    };

    let canonical = tokio::fs::canonicalize(&target).await.unwrap_or(target);
    let clean_path = strip_verbatim_prefix(&canonical);

    let mut rd = tokio::fs::read_dir(&clean_path)
        .await
        .with_context(|| format!("读取本机目录失败: {}", clean_path.display()))?;

    let mut items = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let full = entry.path();
        let meta = tokio::fs::metadata(&full).await.ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        items.push(FileEntry {
            name,
            path: full.to_string_lossy().replace('\\', "/"),
            is_dir,
            size,
            mtime,
            permissions: None,
        });
    }

    sort_file_entries(&mut items);

    let norm_path = clean_path.to_string_lossy().replace('\\', "/");
    // 若当前已经是某个盘符根目录（如 C:/ 或 D:/），其 parent 指向 "drives"（此电脑盘符列表）
    let is_drive_root = norm_path.len() <= 3 && norm_path.ends_with(":/");
    let parent = if is_drive_root {
        Some("drives".to_string())
    } else {
        clean_path
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|p| !p.is_empty() && p != &norm_path)
            .or_else(|| cfg!(windows).then(|| "drives".to_string()))
    };

    Ok(DirListing {
        path: norm_path,
        parent,
        items,
    })
}

fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p.to_path_buf()
    }
}

async fn list_remote_dir_once(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    dir_path: Option<&str>,
) -> Result<DirListing> {
    let sftp = pool.get_or_open_sftp(db, host_id).await?;
    let raw = dir_path.unwrap_or("").trim();

    let resolved = if raw.is_empty() || raw == "~" || raw == "." {
        pool.get_home_dir(&sftp, host_id).await
    } else if let Some(rest) = raw.strip_prefix("~/") {
        let home = pool.get_home_dir(&sftp, host_id).await;
        join_remote_path(&home, rest)
    } else if raw.len() > 1 {
        raw.trim_end_matches('/').to_string()
    } else {
        raw.to_string()
    };

    let read_dir = sftp
        .read_dir(&resolved)
        .await
        .with_context(|| format!("SFTP 读取目录失败: {resolved}"))?;

    let mut items = Vec::new();
    for entry in read_dir {
        let name = entry.file_name();
        let meta = entry.metadata();
        let is_dir = meta.is_dir();
        let size = meta.size.unwrap_or(0);
        let mtime = meta.mtime.map(|m| (m as i64) * 1000).unwrap_or(0);
        let permissions = meta.permissions;
        let item_path = join_remote_path(&resolved, &name);

        items.push(FileEntry {
            name,
            path: item_path,
            is_dir,
            size,
            mtime,
            permissions,
        });
    }

    sort_file_entries(&mut items);

    let parent = if resolved == "/" {
        None
    } else {
        match resolved.rfind('/') {
            Some(0) => Some("/".to_string()),
            Some(idx) => Some(resolved[..idx].to_string()),
            None => None,
        }
    };

    Ok(DirListing {
        path: resolved,
        parent,
        items,
    })
}

fn sort_file_entries(items: &mut [FileEntry]) {
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

pub fn join_remote_path(dir: &str, name: &str) -> String {
    let clean_dir = dir.trim_end_matches('/');
    let clean_name = name.trim_start_matches('/');
    if clean_dir.is_empty() || clean_dir == "/" {
        format!("/{clean_name}")
    } else {
        format!("{clean_dir}/{clean_name}")
    }
}

pub async fn read_text_file(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    file_path: &str,
) -> Result<FileReadResult> {
    if host_id == LOCAL_HOST_ID {
        let meta = tokio::fs::metadata(file_path).await?;
        if meta.len() > MAX_TEXT_PREVIEW_BYTES {
            return Err(anyhow!("文件过大 ({} MB)，超过在线编辑上限 8 MB", meta.len() / 1024 / 1024));
        }
        let bytes = tokio::fs::read(file_path).await?;
        let content = String::from_utf8(bytes).context("该文件不是有效的 UTF-8 文本文件")?;
        return Ok(FileReadResult {
            path: file_path.to_string(),
            content,
            size: meta.len(),
        });
    }

    match read_remote_text_file_once(pool, db, host_id, file_path).await {
        Ok(res) => Ok(res),
        Err(err) if crate::ssh::is_anyhow_sftp_session_dead(&err) => {
            pool.invalidate_sftp(host_id).await;
            read_remote_text_file_once(pool, db, host_id, file_path).await
        }
        Err(err) => Err(err),
    }
}

async fn read_remote_text_file_once(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    file_path: &str,
) -> Result<FileReadResult> {
    let sftp = pool.get_or_open_sftp(db, host_id).await?;
    let meta = sftp
        .metadata(file_path)
        .await
        .with_context(|| format!("读取远端文件属性失败: {file_path}"))?;
    let size = meta.size.unwrap_or(0);
    if size > MAX_TEXT_PREVIEW_BYTES {
        return Err(anyhow!(
            "文件过大 ({:.1} MB)，超过在线编辑上限 8 MB，请直接下载",
            size as f64 / 1024.0 / 1024.0
        ));
    }
    let bytes = sftp
        .read(file_path)
        .await
        .with_context(|| format!("读取远端文件失败: {file_path}"))?;
    let content = String::from_utf8(bytes).context("该文件不是有效的 UTF-8 文本文件")?;
    Ok(FileReadResult {
        path: file_path.to_string(),
        content,
        size,
    })
}

pub async fn write_text_file(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    file_path: &str,
    content: &str,
) -> Result<u64> {
    let bytes = content.as_bytes();
    if host_id == LOCAL_HOST_ID {
        tokio::fs::write(file_path, bytes).await?;
        return Ok(bytes.len() as u64);
    }

    let res = match write_remote_text_file_once(pool, db, host_id, file_path, bytes).await {
        Ok(n) => n,
        Err(err) if crate::ssh::is_anyhow_sftp_session_dead(&err) => {
            pool.invalidate_sftp(host_id).await;
            write_remote_text_file_once(pool, db, host_id, file_path, bytes).await?
        }
        Err(err) => return Err(err),
    };
    pool.invalidate_dir_cache(host_id).await;
    Ok(res)
}

async fn write_remote_text_file_once(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    file_path: &str,
    bytes: &[u8],
) -> Result<u64> {
    let sftp = pool.get_or_open_sftp(db, host_id).await?;
    let mut file = sftp
        .create(file_path)
        .await
        .with_context(|| format!("打开远端文件写入失败: {file_path}"))?;
    file.write_all(bytes).await?;
    file.close().await?;
    Ok(bytes.len() as u64)
}

pub async fn create_dir(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    dir_path: &str,
) -> Result<()> {
    if host_id == LOCAL_HOST_ID {
        tokio::fs::create_dir_all(dir_path).await?;
        return Ok(());
    }
    let sftp = pool.get_or_open_sftp(db, host_id).await?;
    if let Err(err) = sftp.create_dir(dir_path).await {
        if crate::ssh::is_sftp_session_dead(&err) {
            pool.invalidate_sftp(host_id).await;
            let fresh = pool.get_or_open_sftp(db, host_id).await?;
            fresh
                .create_dir(dir_path)
                .await
                .with_context(|| format!("创建远端目录失败: {dir_path}"))?;
        } else {
            return Err(err).with_context(|| format!("创建远端目录失败: {dir_path}"));
        }
    }
    pool.invalidate_dir_cache(host_id).await;
    Ok(())
}

pub async fn rename_item(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    old_path: &str,
    new_path: &str,
) -> Result<()> {
    if host_id == LOCAL_HOST_ID {
        tokio::fs::rename(old_path, new_path).await?;
        return Ok(());
    }
    match rename_remote_item_once(pool, db, host_id, old_path, new_path).await {
        Ok(()) => {}
        Err(err) if crate::ssh::is_anyhow_sftp_session_dead(&err) => {
            pool.invalidate_sftp(host_id).await;
            rename_remote_item_once(pool, db, host_id, old_path, new_path).await?;
        }
        Err(err) => return Err(err),
    }
    pool.invalidate_dir_cache(host_id).await;
    Ok(())
}

async fn rename_remote_item_once(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    old_path: &str,
    new_path: &str,
) -> Result<()> {
    let sftp = pool.get_or_open_sftp(db, host_id).await?;
    // 若目标已存在，先删除旧目标再 rename；但若 try_exists 因死会话报错则直接向上冒泡触发重连
    match sftp.try_exists(new_path).await {
        Ok(true) => {
            let _ = sftp.remove_file(new_path).await;
        }
        Ok(false) => {}
        Err(err) if crate::ssh::is_sftp_session_dead(&err) => {
            return Err(err).with_context(|| format!("重命名前探测目标失败: {new_path}"));
        }
        Err(_) => {}
    }
    sftp.rename(old_path, new_path)
        .await
        .with_context(|| format!("重命名失败: {old_path} -> {new_path}"))?;
    Ok(())
}

/// 是否是"根"这类不该被当作普通目标处理的路径：POSIX 的 `/`，Windows 的盘符根
/// （`C:` / `C:/` / `C:\`），以及 `.` / `..`。
///
/// 判据：统一斜杠、去掉尾部斜杠之后，要么为空（`/`、`//`、空串），要么就是一个盘符。
/// 此前只挡了字面量 `"/"`，而 Windows 上文件管理器的"此电脑"会把盘符当条目列出来
/// （`list_local_dir` 返回 `C:/`），删除按钮对每一行都渲染，于是能一路走到
/// `remove_dir_all("C:/")`。
fn is_root_like(path: &str) -> bool {
    let t = path.trim().replace('\\', "/");
    let t = t.trim_end_matches('/');
    if t.is_empty() || t == "." || t == ".." {
        return true;
    }
    let b = t.as_bytes();
    b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic()
}

pub async fn remove_item(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    target_path: &str,
    is_dir: bool,
) -> Result<()> {
    if is_root_like(target_path) {
        return Err(anyhow!("禁止删除根目录或当前目录（{target_path}）"));
    }
    if host_id == LOCAL_HOST_ID {
        if is_dir {
            tokio::fs::remove_dir_all(target_path).await?;
        } else {
            tokio::fs::remove_file(target_path).await?;
        }
        return Ok(());
    }

    match remove_remote_item_once(pool, db, host_id, target_path, is_dir).await {
        Ok(()) => {}
        Err(err) if crate::ssh::is_anyhow_sftp_session_dead(&err) => {
            pool.invalidate_sftp(host_id).await;
            remove_remote_item_once(pool, db, host_id, target_path, is_dir).await?;
        }
        Err(err) => return Err(err),
    }
    pool.invalidate_dir_cache(host_id).await;
    Ok(())
}

async fn remove_remote_item_once(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    target_path: &str,
    is_dir: bool,
) -> Result<()> {
    let sftp = pool.get_or_open_sftp(db, host_id).await?;
    if is_dir {
        remove_remote_dir_recursive(&sftp, target_path).await?;
    } else {
        sftp.remove_file(target_path)
            .await
            .with_context(|| format!("删除远端文件失败: {target_path}"))?;
    }
    Ok(())
}

async fn remove_remote_dir_recursive(sftp: &SftpSession, dir_path: &str) -> Result<()> {
    let mut stack = vec![dir_path.to_string()];
    let mut dirs_to_remove = Vec::new();

    while let Some(curr) = stack.pop() {
        let entries = sftp.read_dir(&curr).await?;
        dirs_to_remove.push(curr.clone());
        for entry in entries {
            let sub_path = join_remote_path(&curr, &entry.file_name());
            if entry.metadata().is_dir() {
                stack.push(sub_path);
            } else {
                sftp.remove_file(&sub_path).await?;
            }
        }
    }

    for dir in dirs_to_remove.into_iter().rev() {
        sftp.remove_dir(&dir).await?;
    }
    Ok(())
}

// ==================== 压缩与解压（复用 SshPool 连接 / 本机命令） ====================

fn shell_quote(val: &str) -> String {
    format!("'{}'", val.replace('\'', r"'\''"))
}

fn detect_archive_kind(path: &str) -> Option<(&'static str, &'static str)> {
    let lower = path.to_lowercase();
    for (suffix, kind) in [
        (".tar.gz", "tar.gz"),
        (".tgz", "tar.gz"),
        (".tar.xz", "tar.xz"),
        (".txz", "tar.xz"),
        (".tar.bz2", "tar.bz2"),
        (".tbz2", "tar.bz2"),
        (".tar", "tar"),
        (".zip", "zip"),
        (".gz", "gz"),
    ] {
        if lower.ends_with(suffix) {
            return Some((suffix, kind));
        }
    }
    None
}

/// 压缩/解压的远端命令执行：600 秒上限 + 并发闸 + 超时主动 close。
/// 此前这里是无超时的 `while let Some(msg) = channel.wait()`——命令一旦挂住，
/// channel 永不回收，反复调用会打满远端 MaxSessions。
async fn run_ssh_exec_checked(pool: &SshPool, db: &Database, host_id: &str, cmd: &str) -> Result<String> {
    let cap = crate::ssh::exec_host_capture(
        pool,
        db,
        host_id,
        cmd,
        std::time::Duration::from_secs(600),
    )
    .await?;
    if cap.timed_out {
        return Err(anyhow!("远端命令执行超时（600 秒），已强制中断"));
    }
    if cap.exit_code.unwrap_or(0) != 0 {
        let msg = if !cap.stderr.is_empty() {
            cap.stderr
        } else if !cap.stdout.is_empty() {
            cap.stdout
        } else {
            format!("exit code {}", cap.exit_code.unwrap_or(1))
        };
        return Err(anyhow!(msg));
    }
    Ok(cap.stdout)
}

pub async fn compress_item(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    target_path: &str,
) -> Result<String> {
    let clean = target_path.trim_end_matches(['/', '\\']);
    // "drives" 是 Windows 上"此电脑"那个虚拟层，不是真实目录
    if is_root_like(clean) || clean == "drives" {
        return Err(anyhow!("不能直接压缩根目录或当前目录"));
    }

    if host_id == LOCAL_HOST_ID {
        let p = PathBuf::from(clean);
        let parent = p.parent().ok_or_else(|| anyhow!("无效路径"))?;
        let base = p.file_name().and_then(|s| s.to_str()).ok_or_else(|| anyhow!("无效文件名"))?;
        let mut out_name = format!("{base}.zip");
        let mut out_path = parent.join(&out_name);
        if tokio::fs::metadata(&out_path).await.is_ok() {
            out_name = format!("{base}-{}.zip", Uuid::new_v4().simple());
            out_path = parent.join(&out_name);
        }
        let tar_bin = if cfg!(windows) { "tar.exe" } else { "tar" };
        let mut tar_cmd = tokio::process::Command::new(tar_bin);
        tar_cmd.args(["-a", "-cf", &out_name, "--", base])
            .current_dir(parent);
        #[cfg(windows)]
        tar_cmd.creation_flags(0x08000000);
        let status = tar_cmd.output().await.context("调用本机 tar.exe 压缩失败")?;
        if !status.status.success() {
            return Err(anyhow!(
                "压缩失败: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }
        return Ok(out_path.to_string_lossy().replace('\\', "/"));
    }

    let (dir, base) = match clean.rfind('/') {
        Some(0) => ("/", &clean[1..]),
        Some(idx) => (&clean[..idx], &clean[idx + 1..]),
        None => (".", clean),
    };
    let archive_name = format!("{base}.tar.gz");
    let archive_full = join_remote_path(dir, &archive_name);

    let cmd = format!(
        "set -eu; cd {}; out={}; if [ -e \"$out\" ]; then out={}; fi; tar -czf \"$out\" -- {}; printf '%s\\n' \"$out\"",
        shell_quote(dir),
        shell_quote(&archive_name),
        shell_quote(&format!("{base}-{}.tar.gz", &Uuid::new_v4().simple().to_string()[..6])),
        shell_quote(base),
    );
    let out = run_ssh_exec_checked(pool, db, host_id, &cmd).await?;
    pool.invalidate_dir_cache(host_id).await;
    Ok(if out.is_empty() {
        archive_full
    } else {
        join_remote_path(dir, &out)
    })
}

pub async fn extract_item(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    archive_path: &str,
) -> Result<String> {
    let (suffix, kind) = detect_archive_kind(archive_path)
        .ok_or_else(|| anyhow!("不支持的压缩包格式（支持 .tar.gz / .tgz / .tar.xz / .tar.bz2 / .tar / .zip / .gz）"))?;

    if host_id == LOCAL_HOST_ID {
        let p = PathBuf::from(archive_path);
        let parent = p.parent().ok_or_else(|| anyhow!("无效路径"))?;
        let fname = p.file_name().and_then(|s| s.to_str()).ok_or_else(|| anyhow!("无效文件名"))?;
        let stem = fname[..fname.len() - suffix.len()].trim();
        let folder_name = if stem.is_empty() { "extracted" } else { stem };
        let mut out_dir = parent.join(folder_name);
        if tokio::fs::metadata(&out_dir).await.is_ok() {
            out_dir = parent.join(format!("{folder_name}.extracted"));
        }
        tokio::fs::create_dir_all(&out_dir).await?;
        let tar_bin = if cfg!(windows) { "tar.exe" } else { "tar" };
        let mut tar_cmd = tokio::process::Command::new(tar_bin);
        tar_cmd.args(["-xf", archive_path, "-C", &out_dir.to_string_lossy()]);
        #[cfg(windows)]
        tar_cmd.creation_flags(0x08000000);
        let status = tar_cmd.output().await.context("调用本机 tar.exe 解压失败")?;
        if !status.status.success() {
            return Err(anyhow!(
                "解压失败: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }
        return Ok(out_dir.to_string_lossy().replace('\\', "/"));
    }

    let (dir, fname) = match archive_path.rfind('/') {
        Some(0) => ("/", &archive_path[1..]),
        Some(idx) => (&archive_path[..idx], &archive_path[idx + 1..]),
        None => (".", archive_path),
    };
    let stem = fname[..fname.len() - suffix.len()].trim();
    let folder_name = if stem.is_empty() { "extracted" } else { stem };
    let out_dir = join_remote_path(dir, folder_name);
    let fallback_dir = join_remote_path(dir, &format!("{folder_name}.extracted"));

    let extract_cmd = match kind {
        "tar.gz" => "tar -xzf \"$arch\" -C \"$dest\"".to_string(),
        "tar.xz" => "tar -xJf \"$arch\" -C \"$dest\"".to_string(),
        "tar.bz2" => "tar -xjf \"$arch\" -C \"$dest\"".to_string(),
        "tar" => "tar -xf \"$arch\" -C \"$dest\"".to_string(),
        "zip" => "unzip -q \"$arch\" -d \"$dest\"".to_string(),
        "gz" => format!("gzip -dc \"$arch\" > \"$dest\"/{}", shell_quote(folder_name)),
        _ => return Err(anyhow!("不支持的归档类型")),
    };

    let cmd = format!(
        "set -eu; arch={}; dest={}; if [ -e \"$dest\" ]; then dest={}; fi; mkdir -p -- \"$dest\"; {}; printf '%s\\n' \"$dest\"",
        shell_quote(archive_path),
        shell_quote(&out_dir),
        shell_quote(&fallback_dir),
        extract_cmd,
    );
    let out = run_ssh_exec_checked(pool, db, host_id, &cmd).await?;
    pool.invalidate_dir_cache(host_id).await;
    Ok(if out.is_empty() { out_dir } else { out })
}

// ==================== 板块四：满速流式传输核心（UI 与未来 MCP 共用） ====================

/// 本地文件整读 SHA-256（落地对账用）
async fn hash_local_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; STREAM_BUF_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 远端重读哈希的超时：基础 60 秒 + 按 20 MiB/s 估算，封顶 900 秒
fn hash_timeout_for(bytes: u64) -> std::time::Duration {
    let secs = 60 + bytes / (20 * 1024 * 1024);
    std::time::Duration::from_secs(secs.min(900))
}

/// 续传前把源端已经传过的前 N 字节读掉并喂进增量哈希。
/// 这段字节省不掉——闭环校验的哈希必须覆盖整个文件；省掉的是把这 N 字节
/// 重新写到目标端的那条慢链路。
async fn consume_and_hash_prefix<R>(
    reader: &mut R,
    hasher: &mut sha2::Sha256,
    mut remaining: u64,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use sha2::Digest;
    let mut buf = vec![0u8; STREAM_BUF_SIZE];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = tokio::time::timeout(STALL_TIMEOUT, reader.read(&mut buf[..want]))
            .await
            .map_err(|_| anyhow!("续传对账读取失速超时 (连续 12 秒无数据推进)"))??;
        if n == 0 {
            return Err(anyhow!("源数据在续传对账阶段提前结束：还差 {remaining} 字节"));
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(())
}

/// 把本机残片的内容喂进增量哈希，返回读到的字节数
async fn feed_local_file_into_hasher(
    path: &Path,
    hasher: &mut sha2::Sha256,
) -> Result<u64> {
    use sha2::Digest;
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; STREAM_BUF_SIZE];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok(total)
}

/// 从任意异步流（如 HTTP 上传流或本机文件流）高速写入目标主机，
/// 写 `.vps-part` 临时文件并在完成后原子改名，全程实时刷新速率与进度；
/// 完成后做传输闭环校验：上传流增量 SHA-256 vs 落地文件重读 SHA-256。
///
/// 断点续传：残片名对同一个目标文件是固定的，失败时残片留在原地不删；
/// 下次再传同一目标时，先用「远端残片整文件哈希 == 源文件前 N 字节哈希」对账，
/// 对得上就 seek 到 N 接着写，对不上就清掉残片并如实报错（绝不拿错残片拼出一个坏文件）。
pub async fn stream_upload_to_host<R>(
    pool: &SshPool,
    db: &Database,
    transfers: &TransferManager,
    task_id: &str,
    host_id: &str,
    remote_dir: &str,
    filename: &str,
    expected_size: u64,
    mut reader: R,
) -> Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use sha2::{Digest, Sha256};
    let safe_name = Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("无效的文件名"))?;
    let final_path = join_remote_path(remote_dir, safe_name);
    let temp_path = format!("{final_path}.vps-part");
    let started = Instant::now();
    let mut hasher = Sha256::new();

    if host_id == LOCAL_HOST_ID {
        let local_final = PathBuf::from(remote_dir).join(safe_name);
        let local_temp = PathBuf::from(remote_dir).join(format!(".{safe_name}.vps-part"));

        // 续传对账：残片内容必须恰好等于源文件的前 N 字节
        let mut resume_from = 0u64;
        if expected_size > 0 {
            if let Ok(pmeta) = tokio::fs::metadata(&local_temp).await {
                let partial = pmeta.len();
                if partial > 0 && partial < expected_size {
                    let mut partial_hasher = Sha256::new();
                    match feed_local_file_into_hasher(&local_temp, &mut partial_hasher).await {
                        Ok(read) if read == partial => {
                            let partial_digest = hex::encode(partial_hasher.finalize());
                            if let Err(err) =
                                consume_and_hash_prefix(&mut reader, &mut hasher, partial).await
                            {
                                transfers.fail_task(task_id, &err);
                                return Err(err);
                            }
                            if hex::encode(hasher.clone().finalize()) == partial_digest {
                                resume_from = partial;
                                transfers.set_resume(task_id, resume_from);
                            } else {
                                let _ = tokio::fs::remove_file(&local_temp).await;
                                let msg = format!(
                                    "残片与源文件前 {partial} 字节不一致，已清除残片，请重新发起传输"
                                );
                                transfers.fail_task(task_id, &msg);
                                return Err(anyhow!(msg));
                            }
                        }
                        _ => {
                            let _ = tokio::fs::remove_file(&local_temp).await;
                        }
                    }
                } else {
                    // 残片为空或已不小于源文件：不可信，清掉从头来
                    let _ = tokio::fs::remove_file(&local_temp).await;
                }
            }
        } else {
            let _ = tokio::fs::remove_file(&local_temp).await;
        }

        let mut out_file = if resume_from > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&local_temp)
                .await?
        } else {
            tokio::fs::File::create(&local_temp).await?
        };
        let mut buf = vec![0u8; STREAM_BUF_SIZE];
        let mut transferred = resume_from;
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out_file.write_all(&buf[..n]).await?;
            hasher.update(&buf[..n]);
            transferred += n as u64;
            transfers.update_progress(
                task_id,
                transferred,
                expected_size,
                started.elapsed().as_secs_f64(),
            );
        }
        out_file.flush().await?;
        drop(out_file);
        if expected_size > 0 && transferred != expected_size {
            // 残片留着：下次同源再传会对账后续写
            let msg = format!(
                "传输字节数不符：预期 {expected_size}，实收 {transferred}（残片已保留 {}，重传将自动续传）",
                local_temp.display()
            );
            transfers.fail_task(task_id, &msg);
            return Err(anyhow!(msg));
        }
        tokio::fs::rename(&local_temp, &local_final).await?;
        let digest = hex::encode(hasher.finalize());
        let landed = hash_local_file(&local_final).await;
        transfers.finish_task(task_id, transferred, started.elapsed().as_secs_f64());
        match landed {
            Ok(landed) if landed == digest => {
                transfers.set_verification(task_id, Some(digest), Some(true));
            }
            Ok(landed) => {
                let msg = format!(
                    "SHA-256 校验不一致：传输流 {digest}，落地文件 {landed}（文件已保留，请人工核查）"
                );
                transfers.fail_task(task_id, &msg);
                transfers.set_verification(task_id, Some(digest), Some(false));
                return Err(anyhow!(msg));
            }
            Err(err) => {
                tracing::warn!("本机落地文件重读哈希失败: {err:#}");
                transfers.set_verification(task_id, Some(digest), None);
            }
        }
        return Ok(local_final.to_string_lossy().replace('\\', "/"));
    }

    let mut sftp = pool.get_or_open_sftp(db, host_id).await?;

    // 续传对账：远端残片的整文件哈希必须等于源文件前 N 字节的哈希。
    // 取不到残片哈希（例如远端没有 sha256sum）就不敢续，清掉残片从头传。
    let mut resume_from = 0u64;
    if expected_size > 0 {
        let meta_res = match sftp.metadata(&temp_path).await {
            Ok(meta) => Ok(meta),
            Err(err) if crate::ssh::is_sftp_session_dead(&err) => {
                // 缓存 SFTP 通道已关闭（如 session closed），当场清理重连再查残片，避免误吞错误并拖到 open_with_flags 才炸
                pool.invalidate_sftp(host_id).await;
                sftp = pool.get_or_open_sftp(db, host_id).await?;
                sftp.metadata(&temp_path).await
            }
            Err(err) => Err(err),
        };
        if let Ok(meta) = meta_res {
            let partial = meta.size.unwrap_or(0);
            if partial > 0 && partial < expected_size {
                match crate::ssh::remote_sha256(
                    pool,
                    db,
                    host_id,
                    &temp_path,
                    hash_timeout_for(partial),
                )
                .await
                {
                    Ok(partial_digest) => {
                        if let Err(err) =
                            consume_and_hash_prefix(&mut reader, &mut hasher, partial).await
                        {
                            transfers.fail_task(task_id, &err);
                            return Err(err);
                        }
                        let prefix_digest = hex::encode(hasher.clone().finalize());
                        if prefix_digest == partial_digest {
                            resume_from = partial;
                            transfers.set_resume(task_id, resume_from);
                        } else {
                            let _ = sftp.remove_file(&temp_path).await;
                            let msg = format!(
                                "残片与源文件前 {partial} 字节不一致（残片 {partial_digest}，源 {prefix_digest}），已清除残片，请重新发起传输"
                            );
                            transfers.fail_task(task_id, &msg);
                            return Err(anyhow!(msg));
                        }
                    }
                    Err(err) => {
                        tracing::warn!("远端残片哈希不可用，放弃续传从头传: {err:#}");
                        let _ = sftp.remove_file(&temp_path).await;
                    }
                }
            } else if partial >= expected_size {
                let _ = sftp.remove_file(&temp_path).await;
            }
        }
    }

    // 续传分支固定使用 CREATE | WRITE（绝不加 APPEND）：russh-sftp 的 File::write_all 按本地 seek 游标传 offset，
    // 若带 APPEND 会被 SFTP 服务端强制忽略偏移导致静默写坏文件。
    let open_flags = if resume_from > 0 {
        OpenFlags::CREATE | OpenFlags::WRITE
    } else {
        OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE
    };
    let mut remote_file = match sftp.open_with_flags(&temp_path, open_flags).await {
        Ok(f) => f,
        Err(err) if crate::ssh::is_sftp_session_dead(&err) => {
            pool.invalidate_sftp(host_id).await;
            sftp = pool.get_or_open_sftp(db, host_id).await?;
            sftp.open_with_flags(&temp_path, open_flags)
                .await
                .with_context(|| format!("创建远端临时文件失败: {temp_path}"))?
        }
        Err(err) => {
            return Err(err).with_context(|| format!("创建远端临时文件失败: {temp_path}"));
        }
    };
    if resume_from > 0 {
        use tokio::io::AsyncSeekExt;
        remote_file
            .seek(std::io::SeekFrom::Start(resume_from))
            .await
            .with_context(|| format!("定位远端残片续写位置失败: {temp_path}"))?;
    }

    let mut buf = vec![0u8; STREAM_BUF_SIZE];
    let mut transferred = resume_from;
    let write_res: Result<()> = async {
        loop {
            let n = tokio::time::timeout(STALL_TIMEOUT, reader.read(&mut buf))
                .await
                .map_err(|_| anyhow!("上传读取失速超时 (连续 12 秒无数据推进)"))??;
            if n == 0 {
                break;
            }
            tokio::time::timeout(STALL_TIMEOUT, remote_file.write_all(&buf[..n]))
                .await
                .map_err(|_| anyhow!("远端写入失速超时 (连续 12 秒无数据推进)"))??;
            hasher.update(&buf[..n]);
            transferred += n as u64;
            transfers.update_progress(
                task_id,
                transferred,
                expected_size,
                started.elapsed().as_secs_f64(),
            );
        }
        tokio::time::timeout(STALL_TIMEOUT, remote_file.close())
            .await
            .map_err(|_| anyhow!("关闭远端文件句柄失速超时"))??;
        Ok(())
    }
    .await;

    if let Err(err) = write_res {
        // 残片留在远端，下次传同一个文件会对账后续写
        transfers.fail_task(
            task_id,
            format!("{err:#}（已保留远端残片 {temp_path}，重传将自动续传）"),
        );
        return Err(err);
    }

    if expected_size > 0 && transferred != expected_size {
        let msg = format!(
            "传输字节数不符：预期 {expected_size}，实收 {transferred}（已保留远端残片 {temp_path}，重传将自动续传）"
        );
        transfers.fail_task(task_id, &msg);
        return Err(anyhow!(msg));
    }

    // 原子替换目标文件（若因 SFTP 通道闪断失败则自愈重连一次；失败保留 .vps-part 残片不删）
    if let Err(err) = atomic_replace_remote_file(&sftp, &temp_path, &final_path).await {
        let retry_res = if crate::ssh::is_sftp_session_dead(&err) {
            pool.invalidate_sftp(host_id).await;
            sftp = pool.get_or_open_sftp(db, host_id).await?;
            atomic_replace_remote_file(&sftp, &temp_path, &final_path).await
        } else {
            Err(err)
        };
        if let Err(err) = retry_res {
            transfers.fail_task(task_id, &err);
            return Err(err.into());
        }
    }

    let digest = hex::encode(hasher.finalize());
    match crate::ssh::remote_sha256(pool, db, host_id, &final_path, hash_timeout_for(transferred))
        .await
    {
        Ok(remote) if remote == digest => {
            transfers.finish_task(task_id, transferred, started.elapsed().as_secs_f64());
            transfers.set_verification(task_id, Some(digest), Some(true));
        }
        Ok(remote) => {
            let msg = format!(
                "SHA-256 校验不一致：传输流 {digest}，远端落地 {remote}（文件已保留，请人工核查）"
            );
            transfers.fail_task(task_id, &msg);
            transfers.set_verification(task_id, Some(digest), Some(false));
            return Err(anyhow!(msg));
        }
        Err(err) => {
            // 远端哈希命令不可用时：任务照常完成，但不冒充"已校验"
            tracing::warn!("远端落地文件哈希失败，跳过校验: {err:#}");
            transfers.finish_task(task_id, transferred, started.elapsed().as_secs_f64());
            transfers.set_verification(task_id, Some(digest), None);
        }
    }
    pool.invalidate_dir_cache(host_id).await;
    Ok(final_path)
}

async fn atomic_replace_remote_file(
    sftp: &SftpSession,
    temp_path: &str,
    final_path: &str,
) -> std::result::Result<(), russh_sftp::client::error::Error> {
    match sftp.try_exists(final_path).await {
        Ok(true) => {
            let _ = sftp.remove_file(final_path).await;
        }
        Ok(false) => {}
        Err(err) if crate::ssh::is_sftp_session_dead(&err) => return Err(err),
        Err(_) => {}
    }
    sftp.rename(temp_path, final_path).await
}

/// 本机路径 ↔ 远端路径 直接高速传输（桌面端直传 & 未来板块六 MCP `file.put` / `file.get` 直接调用此函数）
pub async fn transfer_local_remote_file(
    pool: &SshPool,
    db: &Database,
    transfers: &TransferManager,
    host_id: &str,
    direction: &str, // "upload" | "download"
    local_path: &str,
    remote_path: &str,
) -> Result<TransferTask> {
    let host = db
        .get_record(host_id)?
        .ok_or_else(|| anyhow!("主机不存在: {host_id}"))?;

    if direction == "upload" {
        let meta = tokio::fs::metadata(local_path)
            .await
            .with_context(|| format!("读取本机文件失败: {local_path}"))?;
        let filename = Path::new(local_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("upload.bin")
            .to_string();
        let remote_dir = if remote_path.ends_with('/') {
            remote_path.trim_end_matches('/').to_string()
        } else {
            match remote_path.rfind('/') {
                Some(0) => "/".to_string(),
                Some(idx) => remote_path[..idx].to_string(),
                None => ".".to_string(),
            }
        };
        let target_name = if remote_path.ends_with('/') {
            filename.clone()
        } else {
            remote_path
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&filename)
                .to_string()
        };

        let task_id = transfers.create_task(
            host_id,
            &host.name,
            "upload",
            &target_name,
            Some(local_path.to_string()),
            &join_remote_path(&remote_dir, &target_name),
            meta.len(),
        );
        let file = tokio::fs::File::open(local_path).await?;
        stream_upload_to_host(
            pool,
            db,
            transfers,
            &task_id,
            host_id,
            &remote_dir,
            &target_name,
            meta.len(),
            file,
        )
        .await?;

        transfers
            .list_tasks(None)
            .into_iter()
            .find(|t| t.id == task_id)
            .ok_or_else(|| anyhow!("读取传输任务状态失败"))
    } else if host_id == LOCAL_HOST_ID {
        // 本机主机上的「下载」就是本机内部复制：源和目标都在本机。
        // 此前这里没有本机分支，直接去开 SFTP——而本机主机的 addr 是 127.0.0.1、port 是 0，
        // 于是必然失败（实测 "SSH 连接失败: 请求的地址无效 (os error 10049)"），本机文件
        // 永远下载不下来。这里复用同一套流式引擎（临时文件 + 残片对账续传 + 原子替换 +
        // SHA-256 闭环），只把「读远端」换成「读本机文件」。
        let src = PathBuf::from(remote_path);
        let meta = tokio::fs::metadata(&src)
            .await
            .with_context(|| format!("本机源文件不存在: {remote_path}"))?;
        let total = meta.len();
        let filename = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download.bin")
            .to_string();

        let dest = PathBuf::from(local_path);
        let dest_dir = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let dest_name = dest
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("无效的本机落点: {local_path}"))?
            .to_string();

        let task_id = transfers.create_task(
            host_id,
            &host.name,
            "download",
            &filename,
            Some(local_path.to_string()),
            remote_path,
            total,
        );
        let src_file = tokio::fs::File::open(&src).await?;
        stream_upload_to_host(
            pool,
            db,
            transfers,
            &task_id,
            LOCAL_HOST_ID,
            &dest_dir,
            &dest_name,
            total,
            src_file,
        )
        .await?;

        transfers
            .list_tasks(None)
            .into_iter()
            .find(|t| t.id == task_id)
            .ok_or_else(|| anyhow!("读取传输任务状态失败"))
    } else {
        use sha2::{Digest, Sha256};
        let mut sftp = pool.get_or_open_sftp(db, host_id).await?;
        let meta = match sftp.metadata(remote_path).await {
            Ok(m) => m,
            Err(err) if crate::ssh::is_sftp_session_dead(&err) => {
                pool.invalidate_sftp(host_id).await;
                sftp = pool.get_or_open_sftp(db, host_id).await?;
                sftp.metadata(remote_path)
                    .await
                    .with_context(|| format!("远端文件不存在: {remote_path}"))?
            }
            Err(err) => {
                return Err(err).with_context(|| format!("远端文件不存在: {remote_path}"));
            }
        };
        let total = meta.size.unwrap_or(0);
        let filename = remote_path
            .rsplit('/')
            .next()
            .unwrap_or("download.bin")
            .to_string();

        let task_id = transfers.create_task(
            host_id,
            &host.name,
            "download",
            &filename,
            Some(local_path.to_string()),
            remote_path,
            total,
        );

        // 闭环校验的基准：先取源文件哈希（失败则跳过校验，不阻塞传输本身）
        let remote_hash = match crate::ssh::remote_sha256(
            pool,
            db,
            host_id,
            remote_path,
            hash_timeout_for(total),
        )
        .await
        {
            Ok(h) => Some(h),
            Err(err) => {
                tracing::warn!("下载前取远端哈希失败，跳过闭环校验: {err:#}");
                None
            }
        };

        let started = Instant::now();
        let mut remote_file = match sftp.open(remote_path).await {
            Ok(f) => f,
            Err(err) if crate::ssh::is_sftp_session_dead(&err) => {
                pool.invalidate_sftp(host_id).await;
                sftp = pool.get_or_open_sftp(db, host_id).await?;
                sftp.open(remote_path).await?
            }
            Err(err) => return Err(err.into()),
        };
        let temp_local = format!("{local_path}.vps-part");
        let mut hasher = Sha256::new();

        // 续传：本机残片的字节先进增量哈希，再从远端的对应偏移接着读。
        // 残片到底是不是这个源文件的前半段，由收尾那次「远端源哈希 vs 落地哈希」判定——
        // 对不上就如实报校验失败，不会悄悄留下一个坏文件。
        let mut resume_from = 0u64;
        if total > 0 {
            if let Ok(pmeta) = tokio::fs::metadata(&temp_local).await {
                let partial = pmeta.len();
                if partial > 0 && partial < total {
                    match feed_local_file_into_hasher(Path::new(&temp_local), &mut hasher).await {
                        Ok(read) if read == partial => {
                            use tokio::io::AsyncSeekExt;
                            remote_file
                                .seek(std::io::SeekFrom::Start(partial))
                                .await
                                .with_context(|| {
                                    format!("定位远端源文件续传偏移失败: {remote_path}")
                                })?;
                            resume_from = partial;
                            transfers.set_resume(&task_id, resume_from);
                        }
                        _ => {
                            hasher = Sha256::new();
                            let _ = tokio::fs::remove_file(&temp_local).await;
                        }
                    }
                } else {
                    let _ = tokio::fs::remove_file(&temp_local).await;
                }
            }
        } else {
            let _ = tokio::fs::remove_file(&temp_local).await;
        }

        let mut out_file = if resume_from > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&temp_local)
                .await?
        } else {
            tokio::fs::File::create(&temp_local).await?
        };
        let mut buf = vec![0u8; STREAM_BUF_SIZE];
        let mut transferred = resume_from;

        let res: Result<()> = async {
            loop {
                let n = tokio::time::timeout(STALL_TIMEOUT, remote_file.read(&mut buf))
                    .await
                    .map_err(|_| anyhow!("下载读取失速超时 (连续 12 秒无数据推进)"))??;
                if n == 0 {
                    break;
                }
                out_file.write_all(&buf[..n]).await?;
                hasher.update(&buf[..n]);
                transferred += n as u64;
                transfers.update_progress(
                    &task_id,
                    transferred,
                    total,
                    started.elapsed().as_secs_f64(),
                );
            }
            out_file.flush().await?;
            let _ = tokio::time::timeout(STALL_TIMEOUT, remote_file.close()).await;
            Ok(())
        }
        .await;

        if let Err(err) = res {
            // 残片留着：下次下同一个文件会从这里接着下
            transfers.fail_task(
                &task_id,
                format!("{err:#}（已保留本机残片 {temp_local}，重传将自动续传）"),
            );
            return Err(err);
        }

        if total > 0 && transferred != total {
            let msg = format!(
                "下载字节数不符：预期 {total}，实收 {transferred}（已保留本机残片 {temp_local}，重传将自动续传）"
            );
            transfers.fail_task(&task_id, &msg);
            return Err(anyhow!(msg));
        }

        tokio::fs::rename(&temp_local, local_path).await?;
        let digest = hex::encode(hasher.finalize());
        transfers.finish_task(&task_id, transferred, started.elapsed().as_secs_f64());
        match remote_hash {
            Some(remote) if remote == digest => {
                transfers.set_verification(&task_id, Some(digest), Some(true));
            }
            Some(remote) => {
                let msg = if resume_from > 0 {
                    format!(
                        "SHA-256 校验不一致：远端源 {remote}，落地 {digest}（本次从 {resume_from} 字节续传，残片很可能来自旧版源文件；文件已保留，请人工核查后重传）"
                    )
                } else {
                    format!(
                        "SHA-256 校验不一致：远端源 {remote}，落地 {digest}（文件已保留，请人工核查）"
                    )
                };
                transfers.fail_task(&task_id, &msg);
                transfers.set_verification(&task_id, Some(digest), Some(false));
                return Err(anyhow!(msg));
            }
            None => {
                // 远端算不出 SHA-256（没有 sha256sum/Get-FileHash）时，非续传的整文件下载
                // 仍可接受（只是无法"宣称已校验"）；但**续传**的下载绝不能静默放行——
                // 残片是不是这个源文件的前半段，唯一判据就是收尾这次对账，对不了账就
                // 可能把旧残片拼进成品。宁可报失败，也不产出一个悄悄坏掉的文件。
                if resume_from > 0 {
                    let msg = format!(
                        "本次从 {resume_from} 字节续传，但远端无法计算 SHA-256，无法确认残片与源文件一致；文件已保留，请人工核查后重传（或删除残片重下）"
                    );
                    transfers.fail_task(&task_id, &msg);
                    transfers.set_verification(&task_id, Some(digest), None);
                    return Err(anyhow!(msg));
                }
                transfers.set_verification(&task_id, Some(digest), None);
            }
        }

        transfers
            .list_tasks(None)
            .into_iter()
            .find(|t| t.id == task_id)
            .ok_or_else(|| anyhow!("读取传输任务状态失败"))
    }
}

/// 跨 VPS 直接流式对传（VPS A ──内存流──▶ VPS B，不落本机磁盘）
pub async fn transfer_remote_to_remote(
    pool: &SshPool,
    db: &Database,
    transfers: &TransferManager,
    src_host_id: &str,
    src_path: &str,
    dst_host_id: &str,
    dst_path: &str,
) -> Result<TransferTask> {
    let src_host = db
        .get_record(src_host_id)?
        .ok_or_else(|| anyhow!("源主机不存在: {src_host_id}"))?;
    let dst_host = db
        .get_record(dst_host_id)?
        .ok_or_else(|| anyhow!("目标主机不存在: {dst_host_id}"))?;

    let mut src_sftp = pool.get_or_open_sftp(db, src_host_id).await?;
    let meta = match src_sftp.metadata(src_path).await {
        Ok(m) => m,
        Err(err) if crate::ssh::is_sftp_session_dead(&err) => {
            pool.invalidate_sftp(src_host_id).await;
            src_sftp = pool.get_or_open_sftp(db, src_host_id).await?;
            src_sftp
                .metadata(src_path)
                .await
                .with_context(|| format!("源主机文件不存在: {src_path}"))?
        }
        Err(err) => {
            return Err(err).with_context(|| format!("源主机文件不存在: {src_path}"));
        }
    };
    let total = meta.size.unwrap_or(0);
    let src_filename = src_path
        .rsplit('/')
        .next()
        .unwrap_or("transfer.bin")
        .to_string();

    let (dst_dir, dst_name) = if dst_path.ends_with('/') {
        (
            dst_path.trim_end_matches('/').to_string(),
            src_filename.clone(),
        )
    } else {
        match dst_path.rfind('/') {
            Some(0) => ("/".to_string(), dst_path[1..].to_string()),
            Some(idx) => (
                dst_path[..idx].to_string(),
                dst_path[idx + 1..].to_string(),
            ),
            None => (".".to_string(), dst_path.to_string()),
        }
    };

    let label = format!("{} -> {}", src_host.name, dst_host.name);
    let task_id = transfers.create_task(
        dst_host_id,
        &label,
        "upload",
        &dst_name,
        Some(format!("{}:{src_path}", src_host.name)),
        &join_remote_path(&dst_dir, &dst_name),
        total,
    );

    let src_file = match src_sftp.open(src_path).await {
        Ok(f) => f,
        Err(err) if crate::ssh::is_sftp_session_dead(&err) => {
            pool.invalidate_sftp(src_host_id).await;
            src_sftp = pool.get_or_open_sftp(db, src_host_id).await?;
            src_sftp.open(src_path).await?
        }
        Err(err) => return Err(err.into()),
    };
    stream_upload_to_host(
        pool,
        db,
        transfers,
        &task_id,
        dst_host_id,
        &dst_dir,
        &dst_name,
        total,
        src_file,
    )
    .await?;

    transfers
        .list_tasks(None)
        .into_iter()
        .find(|t| t.id == task_id)
        .ok_or_else(|| anyhow!("读取跨主机传输任务状态失败"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 必须拦下：删掉就不可逆的"根"类目标。
    /// `C:/` 那几条是实测过的漏洞——文件管理器把盘符当条目列出来，删除按钮对每行都渲染，
    /// 旧判定只挡字面量 `"/"`，于是能一路走到 `remove_dir_all("C:/")`。
    #[test]
    fn root_like_targets_are_rejected() {
        for p in [
            "/", "//", "///", "", "   ", ".", "..", "./", "../", "C:", "C:/", "C:\\", "c:/",
            "D:\\", " Z: ",
        ] {
            assert!(is_root_like(p), "应判为根但放过了: {p:?}");
        }
    }

    /// 必须放行：正常目录与文件。误拦这些，文件管理器就没法用了。
    #[test]
    fn ordinary_paths_are_not_root_like() {
        for p in [
            "/etc", "/var/log/nginx.old", "/root", "C:/Users", "C:/Users/x/file.txt",
            "E:/1shell4.7/vps-switch", "./build", "../sibling", "drives", "a:/x", "/dev",
        ] {
            assert!(!is_root_like(p), "误判为根: {p:?}");
        }
    }

    #[test]
    fn sftp_dead_session_errors_are_distinguished_from_business_status() {
        use russh_sftp::client::error::Error as SftpErr;
        use russh_sftp::protocol::{Status, StatusCode};

        let closed = SftpErr::UnexpectedBehavior("session closed".into());
        assert!(crate::ssh::is_sftp_session_dead(&closed));
        let wrapped = anyhow::Error::new(closed).context("创建远端临时文件失败: /tmp/mini.txt.vps-part");
        assert!(crate::ssh::is_anyhow_sftp_session_dead(&wrapped));

        let timeout = SftpErr::Timeout;
        assert!(crate::ssh::is_sftp_session_dead(&timeout));

        let io_err = SftpErr::IO("unexpected eof".into());
        assert!(crate::ssh::is_sftp_session_dead(&io_err));

        let conn_lost = SftpErr::Status(Status {
            id: 1,
            status_code: StatusCode::ConnectionLost,
            error_message: "connection lost".into(),
            language_tag: "en".into(),
        });
        assert!(crate::ssh::is_sftp_session_dead(&conn_lost));

        // 业务状态码（文件不存在、权限拒绝）说明 SFTP 通道活着，不应触发会话失效重连
        let no_such_file = SftpErr::Status(Status {
            id: 2,
            status_code: StatusCode::NoSuchFile,
            error_message: "No such file".into(),
            language_tag: "en".into(),
        });
        assert!(!crate::ssh::is_sftp_session_dead(&no_such_file));
        let wrapped_biz = anyhow::Error::new(no_such_file).context("读取远端文件失败");
        assert!(!crate::ssh::is_anyhow_sftp_session_dead(&wrapped_biz));

        let perm_denied = SftpErr::Status(Status {
            id: 3,
            status_code: StatusCode::PermissionDenied,
            error_message: "Permission denied".into(),
            language_tag: "en".into(),
        });
        assert!(!crate::ssh::is_sftp_session_dead(&perm_denied));
    }
}
