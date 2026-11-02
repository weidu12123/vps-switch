use crate::{
    crypto::MasterKey,
    db::Database,
    models::HostRecord,
};
use anyhow::{anyhow, Context, Result};
use russh::{
    client::{self, AuthResult, Config, Handle, KeyboardInteractiveAuthResponse, Msg},
    keys::{decode_secret_key, key::PrivateKeyWithHashAlg, PublicKeyOrCertificate},
    Channel, ChannelMsg, Disconnect,
};
use russh_sftp::client::{Config as SftpConfig, SftpSession};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

pub struct SshClientHandler;

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

/// 同一 SSH 连接上的 session channel 总数受 sshd `MaxSessions`（默认 10）限制。
/// 每个 vps_exec / 探针都会开一个 channel，高频并发调用（agent 并行工具调用）会打满上限
/// 导致 channel open 失败。闸门只约束执行/探针类短通道；终端 pane 与 SFTP 不排队。
const MAX_CONCURRENT_EXEC_PER_HOST: usize = 6;

/// 目录缓存条数上限。缓存只为让"点进子目录/返回上级"瞬开，长驻进程里长时间乱逛会让它
/// 无界增长（每条的 DirListing 带着整目录的条目）。超限就按时间淘汰最旧的，不清空。
const DIR_CACHE_MAX_ENTRIES: usize = 256;

/// 单主机 SSH 与 SFTP 连接池：
/// - 终端、执行、SFTP 文件浏览、高速上传下载、探针全部共用同一条底层 SSH 连接；
/// - 通道窗口拉大到 16 MiB + SFTP 64 路并发流水线（255 KiB 大包），彻底跑满跨境高 RTT 带宽。
#[derive(Default)]
pub struct SshPool {
    sessions: Mutex<HashMap<String, Arc<Handle<SshClientHandler>>>>,
    /// 终端专用连接：登录横幅（MOTD）是 sshd 挂在 **连接第一次会话** 上的东西——
    /// 同一条连接上的第二个会话不会再打印它。执行/探针/SFTP 共用池里那条连接，
    /// 横幅会被第一个用它的会话（通常是状态检查）吃掉，终端就再也看不到了。
    /// 所以终端单独一条连接，且该主机最后一个终端关掉时释放：下次打开终端是全新连接，
    /// 横幅必然出现（与普通 SSH 客户端一次登录一条连接的行为一致）。
    pane_sessions: Mutex<HashMap<String, Arc<Handle<SshClientHandler>>>>,
    sftps: Mutex<HashMap<String, Arc<SftpSession>>>,
    home_dirs: Mutex<HashMap<String, String>>,
    dir_cache: Mutex<HashMap<(String, String), (Instant, crate::files::DirListing)>>,
    exec_gates: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl SshPool {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            pane_sessions: Mutex::new(HashMap::new()),
            sftps: Mutex::new(HashMap::new()),
            home_dirs: Mutex::new(HashMap::new()),
            dir_cache: Mutex::new(HashMap::new()),
            exec_gates: Mutex::new(HashMap::new()),
        }
    }

    /// 申请该主机的执行通道额度；持有返回的 permit 直到 channel 用完为止。
    pub async fn acquire_exec_slot(&self, host_id: &str) -> Result<OwnedSemaphorePermit> {
        let sem = {
            let mut gates = self.exec_gates.lock().await;
            gates
                .entry(host_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(MAX_CONCURRENT_EXEC_PER_HOST)))
                .clone()
        };
        tokio::time::timeout(Duration::from_secs(60), sem.acquire_owned())
            .await
            .map_err(|_| {
                anyhow!(
                    "该主机并发执行通道已满（{MAX_CONCURRENT_EXEC_PER_HOST} 路在用），请稍后重试"
                )
            })?
            .map_err(|_| anyhow!("执行通道信号量已关闭"))
    }

    pub async fn get_cached_dir(
        &self,
        host_id: &str,
        path: &str,
        max_age: Duration,
    ) -> Option<crate::files::DirListing> {
        let guard = self.dir_cache.lock().await;
        guard
            .get(&(host_id.to_string(), path.to_string()))
            .and_then(|(ts, listing)| {
                if ts.elapsed() <= max_age {
                    Some(listing.clone())
                } else {
                    None
                }
            })
    }

    pub async fn put_cached_dir(
        &self,
        host_id: &str,
        req_path: &str,
        listing: &crate::files::DirListing,
    ) {
        let mut guard = self.dir_cache.lock().await;
        let now = Instant::now();
        guard.insert(
            (host_id.to_string(), listing.path.clone()),
            (now, listing.clone()),
        );
        if req_path != listing.path {
            guard.insert(
                (host_id.to_string(), req_path.to_string()),
                (now, listing.clone()),
            );
        }
        if guard.len() > DIR_CACHE_MAX_ENTRIES {
            let mut by_age: Vec<(Instant, (String, String))> = guard
                .iter()
                .map(|(key, (ts, _))| (*ts, key.clone()))
                .collect();
            by_age.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, key) in by_age.into_iter().take(guard.len() - DIR_CACHE_MAX_ENTRIES) {
                guard.remove(&key);
            }
        }
    }

    pub async fn invalidate_dir_cache(&self, host_id: &str) {
        let mut guard = self.dir_cache.lock().await;
        guard.retain(|(h, _), _| h != host_id);
    }

    pub async fn get_home_dir(&self, sftp: &SftpSession, host_id: &str) -> String {
        {
            let guard = self.home_dirs.lock().await;
            if let Some(h) = guard.get(host_id) {
                return h.clone();
            }
        }
        match sftp.canonicalize(".").await {
            Ok(resolved) => {
                self.home_dirs
                    .lock()
                    .await
                    .insert(host_id.to_string(), resolved.clone());
                resolved
            }
            Err(_) => "/root".to_string(),
        }
    }

    pub async fn get_or_connect(
        &self,
        host: &HostRecord,
        jump_host: Option<&HostRecord>,
        local_proxy_port: Option<u16>,
        master_key: &MasterKey,
    ) -> Result<Arc<Handle<SshClientHandler>>> {
        {
            let guard = self.sessions.lock().await;
            if let Some(existing) = guard.get(&host.id) {
                if !existing.is_closed() {
                    return Ok(existing.clone());
                }
            }
        }
        // 旧 SSH 连接已断开或尚未建立，挂在旧连接上的 SFTP 会话必然已死，同步清除避免残留死句柄
        self.sftps.lock().await.remove(&host.id);

        let handle =
            connect_single_or_jump(host, jump_host, local_proxy_port, master_key).await?;
        let arc_handle = Arc::new(handle);
        {
            let mut guard = self.sessions.lock().await;
            guard.insert(host.id.clone(), arc_handle.clone());
        }
        Ok(arc_handle)
    }

    /// 终端专用连接（见 `pane_sessions` 注释）。与执行/SFTP 的连接完全分开。
    pub async fn get_or_connect_pane(
        &self,
        host: &HostRecord,
        jump_host: Option<&HostRecord>,
        local_proxy_port: Option<u16>,
        master_key: &MasterKey,
    ) -> Result<Arc<Handle<SshClientHandler>>> {
        {
            let guard = self.pane_sessions.lock().await;
            if let Some(existing) = guard.get(&host.id) {
                if !existing.is_closed() {
                    return Ok(existing.clone());
                }
            }
        }

        let handle =
            connect_single_or_jump(host, jump_host, local_proxy_port, master_key).await?;
        let arc_handle = Arc::new(handle);
        {
            let mut guard = self.pane_sessions.lock().await;
            guard.insert(host.id.clone(), arc_handle.clone());
        }
        Ok(arc_handle)
    }

    /// 该主机已无终端时释放终端连接：下次打开终端会重新登录，横幅才会再次出现。
    pub async fn drop_pane_session(&self, host_id: &str) {
        let removed = { self.pane_sessions.lock().await.remove(host_id) };
        if let Some(handle) = removed {
            let _ = handle
                .disconnect(Disconnect::ByApplication, "no more terminals", "en")
                .await;
        }
    }

    pub async fn open_session_channel(
        &self,
        host: &HostRecord,
        jump_host: Option<&HostRecord>,
        local_proxy_port: Option<u16>,
        master_key: &MasterKey,
    ) -> Result<Channel<Msg>> {
        let handle = self
            .get_or_connect(host, jump_host, local_proxy_port, master_key)
            .await?;
        match handle.channel_open_session().await {
            Ok(ch) => Ok(ch),
            Err(_) => {
                self.invalidate_host(&host.id).await;
                let fresh = self
                    .get_or_connect(host, jump_host, local_proxy_port, master_key)
                    .await?;
                fresh
                    .channel_open_session()
                    .await
                    .context("打开 SSH 会话通道失败")
            }
        }
    }

    pub async fn invalidate_host(&self, host_id: &str) {
        self.sftps.lock().await.remove(host_id);
        self.sessions.lock().await.remove(host_id);
        self.pane_sessions.lock().await.remove(host_id);
        self.exec_gates.lock().await.remove(host_id);
    }

    /// 当全局代理路由或端口变更时，清空已缓存的连接句柄以便新路由立即生效。
    pub async fn invalidate_all(&self) {
        self.sftps.lock().await.clear();
        self.sessions.lock().await.clear();
        self.pane_sessions.lock().await.clear();
    }

    pub async fn invalidate_sftp(&self, host_id: &str) {
        if self.sftps.lock().await.remove(host_id).is_some() {
            tracing::warn!("SFTP 会话失效并清理缓存: host_id={host_id}");
        }
    }

    pub async fn get_or_open_sftp(
        &self,
        db: &Database,
        host_id: &str,
    ) -> Result<Arc<SftpSession>> {
        let ssh_alive = {
            let guard = self.sessions.lock().await;
            guard.get(host_id).is_some_and(|h| !h.is_closed())
        };
        if ssh_alive {
            let guard = self.sftps.lock().await;
            if let Some(existing) = guard.get(host_id) {
                return Ok(existing.clone());
            }
        } else {
            self.sftps.lock().await.remove(host_id);
        }

        let sftp = match self.open_sftp_once(db, host_id).await {
            Ok(s) => s,
            Err(_) => {
                self.invalidate_host(host_id).await;
                self.open_sftp_once(db, host_id).await?
            }
        };
        let arc_sftp = Arc::new(sftp);
        {
            let mut guard = self.sftps.lock().await;
            guard.insert(host_id.to_string(), arc_sftp.clone());
        }
        Ok(arc_sftp)
    }

    async fn open_sftp_once(&self, db: &Database, host_id: &str) -> Result<SftpSession> {
        let (_record, channel) = open_host_channel(self, db, host_id).await?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .context("请求 SFTP 子系统失败")?;

        // 高速调优：利用 OpenSSH limits@openssh.com (255KiB 单包) + 64 并发读写流水线 (16 MiB 在途窗口)
        let sftp_cfg = SftpConfig {
            max_packet_len: 1024 * 1024,
            max_concurrent_reads: 64,
            max_concurrent_writes: 64,
            max_write_packet_len: 261_120,
            request_timeout_secs: 60,
        };

        SftpSession::new_with_config(channel.into_stream(), sftp_cfg)
            .await
            .context("初始化 SFTP 会话失败")
    }
}

/// 判定 SFTP 错误是否属于底层会话/通道已死（区别于文件不存在、权限拒绝等服务端正常返回的业务状态码）。
pub fn is_sftp_session_dead(err: &russh_sftp::client::error::Error) -> bool {
    use russh_sftp::client::error::Error as SftpErr;
    use russh_sftp::protocol::StatusCode;
    match err {
        SftpErr::Status(st) => matches!(
            st.status_code,
            StatusCode::NoConnection | StatusCode::ConnectionLost
        ),
        SftpErr::IO(_)
        | SftpErr::Timeout
        | SftpErr::UnexpectedPacket
        | SftpErr::UnexpectedBehavior(_) => true,
        SftpErr::Limited(_) => false,
    }
}

pub fn is_anyhow_sftp_session_dead(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(sftp_err) = cause.downcast_ref::<russh_sftp::client::error::Error>() {
            return is_sftp_session_dead(sftp_err);
        }
    }
    let msg = format!("{err:#}");
    msg.contains("session closed")
        || msg.contains("SendError:")
        || msg.contains("RecvError:")
        || msg.contains("Connection lost")
        || msg.contains("No connection")
}

async fn authenticate_session(
    session: &mut Handle<SshClientHandler>,
    host: &HostRecord,
    master_key: &MasterKey,
) -> Result<()> {
    let user = if host.username.trim().is_empty() {
        "root"
    } else {
        host.username.trim()
    };

    if host.auth_type == "none" {
        match session.authenticate_none(user).await? {
            AuthResult::Success => return Ok(()),
            AuthResult::Failure { .. } => {
                return Err(anyhow!("SSH none（免凭据）认证被服务器拒绝"));
            }
        }
    }

    if host.auth_type == "privateKey" {
        let key_pem = master_key
            .decrypt(host.encrypted_private_key.as_deref())?
            .ok_or_else(|| anyhow!("该主机未配置私钥"))?;
        let passphrase = master_key.decrypt(host.encrypted_passphrase.as_deref())?;
        let key_pair =
            decode_secret_key(&key_pem, passphrase.as_deref()).context("解析 SSH 私钥失败")?;
        let best_hash = session.best_supported_rsa_hash().await?.flatten();
        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key_pair), best_hash);
        match session.authenticate_publickey(user, key_with_alg).await? {
            AuthResult::Success => return Ok(()),
            AuthResult::Failure { .. } => return Err(anyhow!("SSH 私钥认证被服务器拒绝")),
        }
    }

    let Some(password) = master_key
        .decrypt(host.encrypted_password.as_deref())?
        .filter(|p| !p.is_empty())
    else {
        if let AuthResult::Success = session.authenticate_none(user).await? {
            return Ok(());
        }
        return Err(anyhow!("该主机未配置密码"));
    };

    match session.authenticate_password(user, &password).await? {
        AuthResult::Success => return Ok(()),
        AuthResult::Failure { .. } => {}
    }

    let mut ki = session
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await?;
    for _ in 0..3 {
        match ki {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => {
                return Err(anyhow!("SSH 密码认证失败"));
            }
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let responses = prompts.iter().map(|_| password.clone()).collect();
                ki = session
                    .authenticate_keyboard_interactive_respond(responses)
                    .await?;
            }
        }
    }

    Err(anyhow!("SSH 认证未通过"))
}

/// 通过本机代理端口 (`127.0.0.1:proxy_port`) 建立到 `(target_host, target_port)` 的 TCP 隧道。
/// 自动兼容 SOCKS5（如 Clash 7890 / v2rayN 10808）与 HTTP CONNECT（如 v2rayN 10809）代理端口。
pub async fn connect_via_local_proxy(
    proxy_port: u16,
    target_host: &str,
    target_port: u16,
) -> Result<tokio::net::TcpStream> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let proxy_addr = format!("127.0.0.1:{proxy_port}");

    // 1. 优先尝试 SOCKS5 握手（兼容 SOCKS5 端口与 Clash/Mihomo Mixed 混合端口）
    let mut stream = tokio::net::TcpStream::connect(&proxy_addr)
        .await
        .with_context(|| format!("无法连接本机代理端口 {proxy_addr}（请确认代理软件已启动）"))?;
    let _ = stream.set_nodelay(true);

    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut greet = [0u8; 2];
    if stream.read_exact(&mut greet).await.is_ok() && greet == [0x05, 0x00] {
        let mut req = vec![0x05, 0x01, 0x00];
        if let Ok(ipv4) = target_host.parse::<std::net::Ipv4Addr>() {
            req.push(0x01);
            req.extend_from_slice(&ipv4.octets());
        } else if let Ok(ipv6) = target_host.parse::<std::net::Ipv6Addr>() {
            req.push(0x04);
            req.extend_from_slice(&ipv6.octets());
        } else {
            let domain = target_host.as_bytes();
            if domain.len() > 255 {
                return Err(anyhow!("目标域名过长"));
            }
            req.push(0x03);
            req.push(domain.len() as u8);
            req.extend_from_slice(domain);
        }
        req.extend_from_slice(&target_port.to_be_bytes());
        stream.write_all(&req).await?;

        let mut head = [0u8; 4];
        stream
            .read_exact(&mut head)
            .await
            .context("读取本机 SOCKS5 代理响应失败")?;
        if head[0] != 0x05 || head[1] != 0x00 {
            return Err(anyhow!(
                "本机 SOCKS5 代理拒绝连接 {}:{} (code={})",
                target_host,
                target_port,
                head[1]
            ));
        }
        match head[3] {
            0x01 => {
                let mut skip = [0u8; 4 + 2];
                stream.read_exact(&mut skip).await?;
            }
            0x03 => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).await?;
                let mut skip = vec![0u8; len[0] as usize + 2];
                stream.read_exact(&mut skip).await?;
            }
            0x04 => {
                let mut skip = [0u8; 16 + 2];
                stream.read_exact(&mut skip).await?;
            }
            other => return Err(anyhow!("未知的 SOCKS5 地址类型: {other}")),
        }
        return Ok(stream);
    }

    // 2. 若代理端口不是 SOCKS5（例如纯 HTTP 代理端口），自动使用 HTTP CONNECT 握手
    let mut http_stream = tokio::net::TcpStream::connect(&proxy_addr)
        .await
        .with_context(|| format!("无法连接本机 HTTP 代理端口 {proxy_addr}"))?;
    let _ = http_stream.set_nodelay(true);
    let connect_req = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    http_stream.write_all(connect_req.as_bytes()).await?;

    let mut resp_buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while resp_buf.len() < 4096 {
        let n = http_stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        resp_buf.push(byte[0]);
        if resp_buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_str = String::from_utf8_lossy(&resp_buf);
    let first_line = header_str.lines().next().unwrap_or("");
    if first_line.contains(" 200 ") || first_line.ends_with(" 200") {
        Ok(http_stream)
    } else {
        Err(anyhow!(
            "本机代理 ({proxy_addr}) 隧道握手失败: {}",
            first_line.trim()
        ))
    }
}

async fn connect_raw_or_proxy(
    config: Arc<Config>,
    target_host: &str,
    target_port: u16,
    local_proxy_port: Option<u16>,
) -> Result<Handle<SshClientHandler>> {
    if let Some(proxy_port) = local_proxy_port.filter(|p| *p > 0) {
        let stream = connect_via_local_proxy(proxy_port, target_host, target_port).await?;
        client::connect_stream(config, stream, SshClientHandler)
            .await
            .context("经本机代理进行 SSH 握手失败")
    } else {
        let addr = format!("{target_host}:{target_port}");
        client::connect(config, addr.as_str(), SshClientHandler)
            .await
            .context("SSH 连接失败")
    }
}

pub async fn connect_single_or_jump(
    host: &HostRecord,
    jump_host: Option<&HostRecord>,
    local_proxy_port: Option<u16>,
    master_key: &MasterKey,
) -> Result<Handle<SshClientHandler>> {
    let mut cfg = Config::default();
    // 关键性能优化：将 SSH Channel 窗口从默认 2MiB 提升至 16MiB，单包上限拉满 65535，
    // 彻底消除跨境高 RTT 线路上传下载时等待 SSH_MSG_CHANNEL_WINDOW_ADJUST 造成的断流限速
    cfg.window_size = 16 * 1024 * 1024;
    cfg.maximum_packet_size = 65535;
    cfg.channel_buffer_size = 1024;
    cfg.inactivity_timeout = Some(Duration::from_secs(3600));
    cfg.keepalive_interval = Some(Duration::from_secs(25));
    cfg.nodelay = true;
    let config = Arc::new(cfg);

    if let Some(jump) = jump_host {
        let mut jump_session = tokio::time::timeout(
            Duration::from_secs(10),
            connect_raw_or_proxy(
                config.clone(),
                jump.addr.trim(),
                jump.port,
                local_proxy_port,
            ),
        )
        .await
        .context("连接跳板机超时")??;

        authenticate_session(&mut jump_session, jump, master_key)
            .await
            .context("跳板机认证失败")?;

        let direct_channel = tokio::time::timeout(
            Duration::from_secs(10),
            jump_session.channel_open_direct_tcpip(
                host.addr.trim(),
                host.port as u32,
                "127.0.0.1",
                0,
            ),
        )
        .await
        .context("通过跳板机建立隧道超时")?
        .context("通过跳板机建立隧道失败")?;

        let stream = direct_channel.into_stream();
        let mut target_session = tokio::time::timeout(
            Duration::from_secs(10),
            client::connect_stream(config, stream, SshClientHandler),
        )
        .await
        .context("目标主机 SSH 握手超时")?
        .context("目标主机 SSH 握手失败")?;

        authenticate_session(&mut target_session, host, master_key).await?;
        Ok(target_session)
    } else {
        let mut session = tokio::time::timeout(
            Duration::from_secs(10),
            connect_raw_or_proxy(config, host.addr.trim(), host.port, local_proxy_port),
        )
        .await
        .context("SSH 连接超时")??;

        authenticate_session(&mut session, host, master_key).await?;
        Ok(session)
    }
}

pub struct ExecCapture {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<u32>,
    pub timed_out: bool,
}

/// 执行一条远端命令并完整收集 stdout / stderr / exit code。
/// 关键：超时后必须显式 `channel.close()` —— russh 的 Channel 被 Drop 时不会发 CHANNEL_CLOSE，
/// 直接丢弃会让服务端 channel 一直悬挂（远端命令不结束就不回收），高频调用会打满 MaxSessions。
/// 主动 close 后 OpenSSH 会终止远端进程并立即回收该 channel。
pub async fn exec_capture(
    session: &Handle<SshClientHandler>,
    cmd: &str,
    timeout: Duration,
) -> Result<ExecCapture> {
    let mut channel = session.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code: Option<u32> = None;
    let mut timed_out = false;
    let collect = async {
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status),
                // EOF 只表示"输出结束"：OpenSSH 的 exit-status 常在其之后到达，读到 Close 才收尾
                ChannelMsg::Eof => {}
                ChannelMsg::Close => break,
                _ => {}
            }
        }
    };
    if tokio::time::timeout(timeout, collect).await.is_err() {
        timed_out = true;
        let _ = channel.close().await;
    }
    Ok(ExecCapture {
        stdout: String::from_utf8_lossy(&stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        exit_code,
        timed_out,
    })
}

/// 短命令采集（探针脚本等）：5 秒上限，输出合并为一段文本（保持原探针解析契约）。
pub async fn exec_short_command(session: &Handle<SshClientHandler>, cmd: &str) -> Result<String> {
    let cap = exec_capture(session, cmd, Duration::from_secs(5)).await?;
    let mut merged = cap.stdout;
    if !cap.stderr.is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&cap.stderr);
    }
    Ok(merged)
}

/// 将 SSH 建连失败同步回写到主机状态表（超时/本机代理未开/网络未就绪记为 `unknown`，其余记为 `offline`）。
pub fn record_ssh_connect_failure(db: &Database, host_id: &str, err: &anyhow::Error) {
    if host_id == crate::models::LOCAL_HOST_ID {
        return;
    }
    let msg = format!("{err:#}");
    let status = if crate::inspector::connect_error_is_indeterminate(&msg) {
        "unknown"
    } else {
        "offline"
    };
    let _ = db.update_check_result(host_id, status, None, None, Some(msg));
}

/// 主机级执行采集：并发闸 + 建连 + 完整输出/退出码。命令超时后主动 close channel。
pub async fn exec_host_capture(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    cmd: &str,
    timeout: Duration,
) -> Result<ExecCapture> {
    let _permit = pool.acquire_exec_slot(host_id).await?;
    let record = db
        .get_record(host_id)?
        .ok_or_else(|| anyhow!("主机不存在: {host_id}"))?;
    let (jump, proxy_port) = db.resolve_ssh_route(&record)?;
    let started = Instant::now();
    let handle = match pool
        .get_or_connect(&record, jump.as_ref(), proxy_port, db.master_key())
        .await
    {
        Ok(h) => h,
        Err(err) => {
            record_ssh_connect_failure(db, host_id, &err);
            return Err(err);
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let latency = (elapsed_ms >= 2).then_some(elapsed_ms);
    let _ = db.update_check_result(host_id, "online", latency, None, None);
    exec_capture(&handle, cmd, timeout).await
}

fn ps_single_quote(val: &str) -> String {
    format!("'{}'", val.replace('\'', "''"))
}

/// 计算远端文件的 SHA-256：Linux 走 `sha256sum`，Windows 走 `Get-FileHash`。
/// 用于文件传输闭环校验：上传完成后重读目标文件对账；下载前先取源文件哈希。
pub async fn remote_sha256(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
    path: &str,
    timeout: Duration,
) -> Result<String> {
    let record = db
        .get_record(host_id)?
        .ok_or_else(|| anyhow!("主机不存在: {host_id}"))?;
    let is_windows = record
        .os_info
        .as_ref()
        .and_then(|o| o.os.as_deref())
        .map(|s| s.eq_ignore_ascii_case("windows"))
        .unwrap_or(false);

    let cmd = if is_windows {
        let ps = format!(
            "$h = (Get-FileHash -Algorithm SHA256 -LiteralPath {}).Hash; Write-Output $h.ToLower()",
            ps_single_quote(path)
        );
        let utf16: Vec<u8> = ps.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&utf16);
        format!("powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {b64}")
    } else {
        format!("LC_ALL=C sha256sum {}", shell_quote_posix(path))
    };

    let cap = exec_host_capture(pool, db, host_id, &cmd, timeout).await?;
    if cap.timed_out {
        return Err(anyhow!("远端 SHA-256 计算超时"));
    }
    if cap.exit_code.unwrap_or(0) != 0 {
        return Err(anyhow!(
            "远端 SHA-256 计算失败: {}",
            if cap.stderr.is_empty() {
                format!("exit code {}", cap.exit_code.unwrap_or(1))
            } else {
                cap.stderr
            }
        ));
    }
    let hash = cap
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(anyhow!("远端 SHA-256 输出异常: {}", cap.stdout));
    }
    Ok(hash)
}

fn shell_quote_posix(val: &str) -> String {
    format!("'{}'", val.replace('\'', r"'\''"))
}

pub async fn open_host_channel(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
) -> Result<(HostRecord, Channel<Msg>)> {
    let record = db
        .get_record(host_id)?
        .ok_or_else(|| anyhow!("主机不存在: {host_id}"))?;
    let (jump, proxy_port) = db.resolve_ssh_route(&record)?;
    let started = Instant::now();
    let channel = match pool
        .open_session_channel(&record, jump.as_ref(), proxy_port, db.master_key())
        .await
    {
        Ok(ch) => ch,
        Err(err) => {
            record_ssh_connect_failure(db, &record.id, &err);
            return Err(err);
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let latency = (elapsed_ms >= 2).then_some(elapsed_ms);
    let _ = db.update_check_result(&record.id, "online", latency, None, None);
    Ok((record, channel))
}

/// 终端 pane 专用：走该主机的终端连接（与执行/SFTP 那条分开），保证打开终端时的登录横幅。
pub async fn open_pane_channel(
    pool: &SshPool,
    db: &Database,
    host_id: &str,
) -> Result<(HostRecord, Channel<Msg>)> {
    let record = db
        .get_record(host_id)?
        .ok_or_else(|| anyhow!("主机不存在: {host_id}"))?;
    let (jump, proxy_port) = db.resolve_ssh_route(&record)?;
    let started = Instant::now();
    let handle = match pool
        .get_or_connect_pane(&record, jump.as_ref(), proxy_port, db.master_key())
        .await
    {
        Ok(h) => h,
        Err(err) => {
            record_ssh_connect_failure(db, &record.id, &err);
            return Err(err);
        }
    };
    let channel = match handle.channel_open_session().await {
        Ok(ch) => ch,
        Err(_) => {
            pool.drop_pane_session(host_id).await;
            let fresh = match pool
                .get_or_connect_pane(&record, jump.as_ref(), proxy_port, db.master_key())
                .await
            {
                Ok(h) => h,
                Err(err) => {
                    record_ssh_connect_failure(db, &record.id, &err);
                    return Err(err);
                }
            };
            match fresh
                .channel_open_session()
                .await
                .context("打开 SSH 会话通道失败")
            {
                Ok(ch) => ch,
                Err(err) => {
                    record_ssh_connect_failure(db, &record.id, &err);
                    return Err(err);
                }
            }
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let latency = (elapsed_ms >= 2).then_some(elapsed_ms);
    let _ = db.update_check_result(&record.id, "online", latency, None, None);
    Ok((record, channel))
}

#[cfg(test)]
mod tests {
    use super::connect_via_local_proxy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn local_proxy_tunnel_supports_both_socks5_and_http_connect() {
        // 1. 验证 SOCKS5 隧道握手与后续 SSH Banner 字节流无损透传
        let socks_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_port = socks_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = socks_listener.accept().await.unwrap();
            let mut greet = [0u8; 3];
            sock.read_exact(&mut greet).await.unwrap();
            assert_eq!(greet, [0x05, 0x01, 0x00]);
            sock.write_all(&[0x05, 0x00]).await.unwrap();

            let mut req = [0u8; 10];
            sock.read_exact(&mut req).await.unwrap();
            assert_eq!(&req[..4], &[0x05, 0x01, 0x00, 0x01]);
            sock.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 22])
                .await
                .unwrap();
            sock.write_all(b"SSH-2.0-OpenSSH_9.6\r\n").await.unwrap();
        });

        let mut stream = connect_via_local_proxy(socks_port, "198.51.100.8", 22)
            .await
            .expect("SOCKS5 隧道应握手成功");
        let mut banner = [0u8; 21];
        stream.read_exact(&mut banner).await.unwrap();
        assert_eq!(&banner, b"SSH-2.0-OpenSSH_9.6\r\n");

        // 2. 验证纯 HTTP 代理端口（首个连接非 SOCKS5，自动回退 HTTP CONNECT）且不吞后续字节
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // 第一次连接（SOCKS5 探测）：返回非 0x05 的 HTTP 400
            let (mut first, _) = http_listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            let _ = first.read_exact(&mut buf).await;
            let _ = first.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            drop(first);

            // 第二次连接（HTTP CONNECT）：返回 200 Connection Established 并紧跟 SSH banner
            let (mut second, _) = http_listener.accept().await.unwrap();
            let mut req_buf = [0u8; 128];
            let n = second.read(&mut req_buf).await.unwrap();
            let req_str = String::from_utf8_lossy(&req_buf[..n]);
            assert!(req_str.starts_with("CONNECT example.com:2222 HTTP/1.1\r\n"));
            second
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nSSH-2.0-Test\r\n")
                .await
                .unwrap();
        });

        let mut http_tunnel = connect_via_local_proxy(http_port, "example.com", 2222)
            .await
            .expect("HTTP CONNECT 隧道应握手成功");
        let mut ssh_banner = [0u8; 14];
        http_tunnel.read_exact(&mut ssh_banner).await.unwrap();
        assert_eq!(&ssh_banner, b"SSH-2.0-Test\r\n");
    }
}

