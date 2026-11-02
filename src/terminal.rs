use crate::{
    db::Database,
    models::LOCAL_HOST_ID,
    ssh::{open_pane_channel, SshPool},
};
use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{Config as TermConfig, Term, TermMode},
    vte::ansi::Processor,
};
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use russh::{ChannelMsg, ChannelWriteHalf, Pty};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

const RING_BUFFER_CAPACITY: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
struct TermDimensions {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneMeta {
    pub id: String,
    pub host_id: String,
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub running: bool,
    pub last_exit_code: Option<i32>,
    pub bracketed_paste: bool,
    pub headless: bool,
    pub exited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSnapshot {
    pub pane_id: String,
    pub host_id: String,
    pub state: String, // "idle" | "waiting_input" | "running" | "exited"
    pub cursor_line: String,
    pub screen: String,
    pub cwd: Option<String>,
    pub last_exit_code: Option<i32>,
    pub headless: bool,
    pub exited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UiEvent {
    RevealTerminal {
        host_id: String,
        pane: PaneMeta,
        reason: Option<String>,
    },
    ShowMainWindow,
    OpenStandaloneTerminal {
        host_id: String,
        pane_id: Option<String>,
    },
    HostsUpdated,
}

#[derive(Clone)]
pub enum PaneBroadcast {
    Output(Vec<u8>),
    Meta(PaneMeta),
}

pub enum PaneCommand {
    Input {
        bytes: Vec<u8>,
        sent: Option<oneshot::Sender<Result<u64>>>,
    },
    Paste(String),
    Resize(u16, u16),
    Close,
}

pub struct PaneInternalState {
    pub term: Term<VoidListener>,
    pub processor: Processor,
    pub ring_buffer: VecDeque<u8>,
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub running: bool,
    pub last_exit_code: Option<i32>,
    pub headless: bool,
    pub exited: bool,
    pub output_seq: u64,
    pub last_output_at: Instant,
    /// shell 类型探针结果（OSC 777，注入时用来选钩子；读任务负责填）
    pub shell_kind: Option<&'static str>,
    osc_tail: Vec<u8>,
}

impl PaneInternalState {
    fn new(cols: u16, rows: u16, initial_cwd: Option<String>, headless: bool) -> Self {
        let dims = TermDimensions {
            cols: cols.max(2) as usize,
            lines: rows.max(1) as usize,
        };
        let term = Term::new(TermConfig::default(), &dims, VoidListener);
        Self {
            term,
            processor: Processor::new(),
            ring_buffer: VecDeque::with_capacity(64 * 1024),
            cols,
            rows,
            cwd: initial_cwd,
            title: None,
            running: false,
            last_exit_code: None,
            headless,
            exited: false,
            output_seq: 0,
            last_output_at: Instant::now(),
            shell_kind: None,
            osc_tail: Vec::new(),
        }
    }

    pub fn feed_bytes(&mut self, data: &[u8]) -> bool {
        self.processor.advance(&mut self.term, data);
        self.output_seq = self.output_seq.wrapping_add(1);
        self.last_output_at = Instant::now();

        let clean_for_replay = strip_terminal_queries(data);
        if clean_for_replay.len() >= RING_BUFFER_CAPACITY {
            self.ring_buffer.clear();
            let start = clean_for_replay.len() - RING_BUFFER_CAPACITY;
            self.ring_buffer.extend(&clean_for_replay[start..]);
        } else if !clean_for_replay.is_empty() {
            let overflow = (self.ring_buffer.len() + clean_for_replay.len())
                .saturating_sub(RING_BUFFER_CAPACITY);
            if overflow > 0 {
                self.ring_buffer.drain(..overflow);
            }
            self.ring_buffer.extend(&clean_for_replay);
        }

        self.scan_osc_sequences(data)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(2);
        self.rows = rows.max(1);
        let dims = TermDimensions {
            cols: self.cols as usize,
            lines: self.rows as usize,
        };
        self.term.resize(dims);
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// 直接从 alacritty_terminal::Term 二维屏幕网格读取最后 max_lines 行与光标所在行（零 ANSI 噪音）
    pub fn read_screen_grid(&self, max_lines: usize) -> (String, String) {
        let bottom = self.term.bottommost_line();
        let top = self.term.topmost_line();
        let last_col = self.term.last_column();
        let cursor_pt = self.term.grid().cursor.point;

        let cursor_line = self
            .term
            .bounds_to_string(
                Point::new(cursor_pt.line, Column(0)),
                Point::new(cursor_pt.line, last_col),
            )
            .trim_end()
            .to_string();

        let requested = max_lines.clamp(5, 200) as i32;
        let start_line = std::cmp::max(top, Line(bottom.0 - requested + 1));
        let raw_screen = self.term.bounds_to_string(
            Point::new(start_line, Column(0)),
            Point::new(bottom, last_col),
        );

        let lines: Vec<&str> = raw_screen.lines().collect();
        let mut last_non_empty = lines.len();
        while last_non_empty > 0 && lines[last_non_empty - 1].trim().is_empty() {
            last_non_empty -= 1;
        }
        let trimmed_screen = lines[..last_non_empty].join("\n");
        (trimmed_screen, cursor_line)
    }

    fn scan_osc_sequences(&mut self, chunk: &[u8]) -> bool {
        let mut changed = false;
        self.osc_tail.extend_from_slice(chunk);
        if self.osc_tail.len() > 4096 {
            let keep = 1024;
            let drain_len = self.osc_tail.len() - keep;
            self.osc_tail.drain(..drain_len);
        }

        let mut idx = 0;
        let mut consumed = 0;
        while idx + 2 <= self.osc_tail.len() {
            if self.osc_tail[idx] == 0x1b && self.osc_tail[idx + 1] == b']' {
                let start = idx + 2;
                let mut end = None;
                let mut next_pos = start;
                while next_pos < self.osc_tail.len() {
                    if self.osc_tail[next_pos] == 0x07 {
                        end = Some((next_pos, next_pos + 1));
                        break;
                    }
                    if next_pos + 1 < self.osc_tail.len()
                        && self.osc_tail[next_pos] == 0x1b
                        && self.osc_tail[next_pos + 1] == b'\\'
                    {
                        end = Some((next_pos, next_pos + 2));
                        break;
                    }
                    next_pos += 1;
                }

                if let Some((payload_end, after_end)) = end {
                    let osc_owned = std::str::from_utf8(&self.osc_tail[start..payload_end])
                        .ok()
                        .map(str::to_owned);
                    if let Some(osc_str) = osc_owned {
                        if self.handle_osc_payload(&osc_str) {
                            changed = true;
                        }
                    }
                    consumed = after_end;
                    idx = after_end;
                } else {
                    break;
                }
            } else {
                idx += 1;
            }
        }

        if consumed > 0 {
            self.osc_tail.drain(..consumed);
        } else if let Some(esc_pos) = self.osc_tail.iter().rposition(|&b| b == 0x1b) {
            if esc_pos > 0 {
                self.osc_tail.drain(..esc_pos);
            }
        } else {
            self.osc_tail.clear();
        }

        changed
    }

    fn handle_osc_payload(&mut self, osc: &str) -> bool {
        if let Some(rest) = osc.strip_prefix("777;vps-shell:") {
            if self.shell_kind.is_none() {
                if let Some(kind) = parse_shell_probe(rest) {
                    self.shell_kind = Some(kind);
                    return true;
                }
            }
            return false;
        }

        if let Some(rest) = osc.strip_prefix("7;") {
            if let Some(path_part) = rest.strip_prefix("file://") {
                if let Some(slash_idx) = path_part.find('/') {
                    let raw_path = &path_part[slash_idx..];
                    let decoded = percent_decode(raw_path);
                    let clean = if decoded.len() >= 3
                        && decoded.starts_with('/')
                        && decoded.as_bytes()[2] == b':'
                    {
                        decoded[1..].to_string()
                    } else {
                        decoded
                    };
                    if !clean.is_empty() && self.cwd.as_deref() != Some(&clean) {
                        self.cwd = Some(clean);
                        return true;
                    }
                }
            }
        }

        if let Some(rest) = osc.strip_prefix("133;") {
            if rest.starts_with('A') {
                if self.running {
                    self.running = false;
                    return true;
                }
            } else if rest.starts_with('C') {
                if !self.running {
                    self.running = true;
                    return true;
                }
            } else if let Some(done_rest) = rest.strip_prefix('D') {
                let code = done_rest
                    .strip_prefix(';')
                    .and_then(|s| s.split(';').next())
                    .and_then(|s| s.trim().parse::<i32>().ok());
                let prev_running = self.running;
                let prev_code = self.last_exit_code;
                self.running = false;
                if code.is_some() {
                    self.last_exit_code = code;
                }
                return prev_running || prev_code != self.last_exit_code;
            }
        }

        if let Some(title) = osc.strip_prefix("0;").or_else(|| osc.strip_prefix("2;")) {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                let mut changed = false;
                if self.title.as_deref() != Some(trimmed) {
                    self.title = Some(trimmed.to_string());
                    changed = true;
                }
                if let Some((_, dir_part)) = trimmed.split_once(": ") {
                    let dir = dir_part.trim();
                    if (dir.starts_with('/') || dir.starts_with('~'))
                        && self.cwd.as_deref() != Some(dir)
                    {
                        self.cwd = Some(dir.to_string());
                        changed = true;
                    }
                }
                if self.running {
                    self.running = false;
                    changed = true;
                }
                return changed;
            }
        }

        false
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn strip_terminal_queries(data: &[u8]) -> Vec<u8> {
    const QUERIES: &[&[u8]] = &[
        b"\x1b[c",
        b"\x1b[0c",
        b"\x1b[>c",
        b"\x1b[>0c",
        b"\x1b[=c",
        b"\x1b[=0c",
        b"\x1b[5n",
        b"\x1b[6n",
        b"\x1b[?6n",
    ];
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    'outer: while i < data.len() {
        if data[i] == 0x1b {
            for q in QUERIES {
                if data[i..].starts_with(q) {
                    i += q.len();
                    continue 'outer;
                }
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// 严格版：只认明确的关键词（password: / [y/n] / proceed? …）。
/// 用于非交互 exec 的早退判定——那里没有 stdin，误判会直接掐断一条正常命令，
/// 所以宁可漏判，也不能凭"以 ? / : 结尾"这种文本特征去猜（`Processing:` 就会被误伤）。
pub fn looks_like_explicit_interactive_prompt(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    let patterns = [
        "[y/n]",
        "[yes/no]",
        "(y/n)",
        "(yes/no",
        "continue?",
        "proceed?",
        "password:",
        "password for",
        "passphrase",
        "enter ",
        "press enter",
        "overwrite",
        "are you sure",
        "请确认",
        "请输入",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// 宽松版：明确关键词，或"以 ? / : 结尾且不像 shell 提示符"。
/// 只用于交互式终端（vps_terminal）的屏幕状态判定——那里本来就在等用户输入，
/// 多报一次 waiting_input 无伤大雅；非交互 exec 请用严格版。
pub fn looks_like_interactive_prompt(line: &str) -> bool {
    if looks_like_explicit_interactive_prompt(line) {
        return true;
    }
    let lower = line.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    (lower.ends_with('?') || lower.ends_with(':')) && !looks_like_shell_prompt(line)
}

pub fn looks_like_shell_prompt(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if t.starts_with("PS ") && t.ends_with('>') {
        return true;
    }
    if (t.contains('@') || t.starts_with('[') || t.contains(":~") || t.contains(":/"))
        && (t.ends_with('#') || t.ends_with('$') || t.ends_with('%'))
    {
        return true;
    }
    if t.ends_with(":~#") || t.ends_with(":~$") || t == "#" || t == "$" {
        return true;
    }
    // 花哨提示符（starship / powerlevel10k / agnoster 等的默认尾字符）
    if t.ends_with('❯') || t.ends_with('➜') || t.ends_with('λ') || t.ends_with('»') {
        return true;
    }
    false
}

/// Windows 原生 shell 的提示符（PowerShell `PS C:\>` / cmd `C:\Users\x>`）：
/// 这类主机不能注入 POSIX 钩子，直接跳过 shell integration。
fn prompt_is_windows_style(line: &str) -> bool {
    let t = line.trim();
    if t.starts_with("PS ") && t.ends_with('>') {
        return true;
    }
    let b = t.as_bytes();
    b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/')
        && t.ends_with('>')
}

pub struct PaneHandle {
    pub id: String,
    pub host_id: String,
    pub state: Arc<Mutex<PaneInternalState>>,
    pub cmd_tx: mpsc::UnboundedSender<PaneCommand>,
    pub broadcast_tx: broadcast::Sender<PaneBroadcast>,
    input_lock: tokio::sync::Mutex<()>,
}

impl PaneHandle {
    pub fn snapshot_meta(&self) -> PaneMeta {
        let guard = self.state.lock().unwrap();
        PaneMeta {
            id: self.id.clone(),
            host_id: self.host_id.clone(),
            cols: guard.cols,
            rows: guard.rows,
            cwd: guard.cwd.clone(),
            title: guard.title.clone(),
            running: guard.running,
            last_exit_code: guard.last_exit_code,
            bracketed_paste: guard.bracketed_paste_enabled(),
            headless: guard.headless,
            exited: guard.exited,
        }
    }

    pub fn replay_bytes(&self) -> Vec<u8> {
        let guard = self.state.lock().unwrap();
        let (a, b) = guard.ring_buffer.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }

    pub fn read_screen_snapshot(&self, lines: usize) -> ScreenSnapshot {
        let guard = self.state.lock().unwrap();
        let (screen, cursor_line) = guard.read_screen_grid(lines);
        let state_str = if guard.exited {
            "exited"
        } else if looks_like_interactive_prompt(&cursor_line) {
            "waiting_input"
        } else if looks_like_shell_prompt(&cursor_line) || !guard.running {
            "idle"
        } else {
            "running"
        };
        ScreenSnapshot {
            pane_id: self.id.clone(),
            host_id: self.host_id.clone(),
            state: state_str.to_string(),
            cursor_line,
            screen,
            cwd: guard.cwd.clone(),
            last_exit_code: guard.last_exit_code,
            headless: guard.headless,
            exited: guard.exited,
        }
    }

    /// 向 PTY 发送输入并智能等待：
    /// - 若命令执行完毕回到提示符，立即返回 `idle`；
    /// - 若脚本中途停在 `[Y/n]` / `yes/no` / `password:` 等交互提问超过 600ms，立即返回 `waiting_input`（绝不卡死！）。
    pub async fn send_and_wait(
        &self,
        bytes: Vec<u8>,
        max_lines: usize,
        wait_ms: u64,
    ) -> Result<ScreenSnapshot> {
        let _input_guard = self.input_lock.lock().await;
        let (sent, received) = oneshot::channel();
        self.cmd_tx
            .send(PaneCommand::Input {
                bytes,
                sent: Some(sent),
            })
            .map_err(|_| anyhow::anyhow!("终端输入通道已关闭"))?;
        // 基线来自真正写入 PTY 的时刻：初始化/注入输出不能冒充本次输入的响应。
        let seq_before = received.await.context("终端在发送输入前已关闭")??;

        let deadline = Instant::now() + Duration::from_millis(wait_ms.clamp(200, 60_000));
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let (exited, seq_now, quiet_ms, running, integrated, cursor_line) = {
                let g = self.state.lock().unwrap();
                let (_, cl) = g.read_screen_grid(5);
                (
                    g.exited,
                    g.output_seq,
                    g.last_output_at.elapsed().as_millis() as u64,
                    g.running,
                    g.shell_kind.is_some(),
                    cl,
                )
            };

            if exited {
                break;
            }
            if seq_now > seq_before {
                if looks_like_interactive_prompt(&cursor_line) && quiet_ms >= 550 {
                    break;
                }
                if (!running || (!integrated && looks_like_shell_prompt(&cursor_line)))
                    && quiet_ms >= 350
                {
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }

        Ok(self.read_screen_snapshot(max_lines))
    }
}

pub struct TerminalManager {
    panes: Mutex<HashMap<String, Arc<PaneHandle>>>,
    ui_events_tx: broadcast::Sender<UiEvent>,
    /// 每主机一把建 pane 的异步锁：串行化"检查有无活 pane → 释放旧连接 → 建新 pane"这段。
    /// 否则两个并发 create（例如 agent 同时调 vps_terminal）会让后者的 drop_pane_session
    /// 把前者刚建好的连接断掉，先建的那个 pane 立刻死。
    create_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        let (ui_events_tx, _) = broadcast::channel(128);
        Self {
            panes: Mutex::new(HashMap::new()),
            ui_events_tx,
            create_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn subscribe_ui_events(&self) -> broadcast::Receiver<UiEvent> {
        self.ui_events_tx.subscribe()
    }

    pub fn notify_show_main_window(&self) {
        let _ = self.ui_events_tx.send(UiEvent::ShowMainWindow);
    }

    pub fn notify_open_standalone_terminal(&self, host_id: &str, pane_id: Option<&str>) {
        let _ = self.ui_events_tx.send(UiEvent::OpenStandaloneTerminal {
            host_id: host_id.to_string(),
            pane_id: pane_id.map(|s| s.to_string()),
        });
    }

    pub fn notify_hosts_updated(&self) {
        let _ = self.ui_events_tx.send(UiEvent::HostsUpdated);
    }

    pub fn list_panes(
        &self,
        host_id_filter: Option<&str>,
        include_headless: bool,
    ) -> Vec<PaneMeta> {
        let guard = self.panes.lock().unwrap();
        guard
            .values()
            .filter(|p| host_id_filter.map(|h| p.host_id == h).unwrap_or(true))
            .map(|p| p.snapshot_meta())
            .filter(|m| include_headless || !m.headless)
            .collect()
    }

    pub fn get_pane(&self, pane_id: &str) -> Option<Arc<PaneHandle>> {
        let guard = self.panes.lock().unwrap();
        guard.get(pane_id).cloned()
    }

    pub fn find_active_pane_for_host(&self, host_id: &str) -> Option<Arc<PaneHandle>> {
        let guard = self.panes.lock().unwrap();
        guard
            .values()
            .find(|p| p.host_id == host_id && !p.state.lock().unwrap().exited)
            .cloned()
    }

    /// 将一个无头终端（或已存在的终端）唤起到前端界面，供用户亲自输入隐私密码或手动操作
    pub fn reveal_pane_in_ui(&self, pane_id: &str, reason: Option<String>) -> Option<PaneMeta> {
        let pane = self.get_pane(pane_id)?;
        {
            let mut g = pane.state.lock().unwrap();
            g.headless = false;
        }
        let meta = pane.snapshot_meta();
        let _ = self.ui_events_tx.send(UiEvent::RevealTerminal {
            host_id: pane.host_id.clone(),
            pane: meta.clone(),
            reason,
        });
        Some(meta)
    }

    pub async fn close_pane(&self, pool: &SshPool, pane_id: &str) -> bool {
        let removed = {
            let mut guard = self.panes.lock().unwrap();
            guard.remove(pane_id)
        };
        let Some(pane) = removed else {
            return false;
        };
        let host_id = pane.host_id.clone();
        let _ = pane.cmd_tx.send(PaneCommand::Close);
        drop(pane);
        if !self.has_live_pane(&host_id) {
            pool.drop_pane_session(&host_id).await;
        }
        true
    }

    /// 该主机是否还有活着的终端（pane 自己退出也算没了）。
    fn has_live_pane(&self, host_id: &str) -> bool {
        let guard = self.panes.lock().unwrap();
        guard
            .values()
            .any(|p| p.host_id == host_id && !p.state.lock().unwrap().exited)
    }

    pub async fn create_pane(
        &self,
        pool: &SshPool,
        db: &Database,
        host_id: &str,
        cols: u16,
        rows: u16,
        initial_cwd: Option<String>,
        headless: bool,
    ) -> Result<PaneMeta> {
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(5, 200);
        let pane_id = format!("pane_{}", Uuid::new_v4());

        // 串行化同一主机的并发建 pane（见 create_locks 注释）
        let host_lock = {
            let mut g = self.create_locks.lock().unwrap();
            g.entry(host_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _create_guard = host_lock.lock().await;

        // 先回收该主机已退出的旧 pane。一个活的都不剩 = 上个终端已经关完了：
        // 释放终端连接，新 pane 会重新登录，登录横幅（MOTD）才会再次出现。
        let has_live = {
            let mut guard = self.panes.lock().unwrap();
            guard.retain(|_, p| !(p.host_id == host_id && p.state.lock().unwrap().exited));
            guard.values().any(|p| p.host_id == host_id)
        };
        if !has_live && host_id != LOCAL_HOST_ID {
            pool.drop_pane_session(host_id).await;
        }

        let handle = if host_id == LOCAL_HOST_ID {
            spawn_local_conpty_pane(
                pane_id.clone(),
                host_id.to_string(),
                cols,
                rows,
                initial_cwd,
                headless,
            )?
        } else {
            match spawn_ssh_pty_pane(
                pool,
                db,
                pane_id.clone(),
                host_id.to_string(),
                cols,
                rows,
                initial_cwd,
                headless,
            )
            .await
            {
                Ok(h) => {
                    self.notify_hosts_updated();
                    h
                }
                Err(err) => {
                    self.notify_hosts_updated();
                    return Err(err);
                }
            }
        };

        let meta = handle.snapshot_meta();
        {
            let mut guard = self.panes.lock().unwrap();
            guard.insert(pane_id, handle);
        }
        Ok(meta)
    }
}

fn build_meta_from_guard(pane_id: &str, host_id: &str, guard: &PaneInternalState) -> PaneMeta {
    PaneMeta {
        id: pane_id.to_string(),
        host_id: host_id.to_string(),
        cols: guard.cols,
        rows: guard.rows,
        cwd: guard.cwd.clone(),
        title: guard.title.clone(),
        running: guard.running,
        last_exit_code: guard.last_exit_code,
        bracketed_paste: guard.bracketed_paste_enabled(),
        headless: guard.headless,
        exited: guard.exited,
    }
}

fn spawn_local_conpty_pane(
    pane_id: String,
    host_id: String,
    cols: u16,
    rows: u16,
    initial_cwd: Option<String>,
    headless: bool,
) -> Result<Arc<PaneHandle>> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("创建本机 ConPTY 失败")?;

    #[cfg(windows)]
    let cmd = {
        let mut c = CommandBuilder::new("powershell.exe");
        let init_hook = r#"$global:__vps_orig_prompt = $function:prompt; function global:prompt { $ec = if ($?) { 0 } else { if ($LASTEXITCODE) { $LASTEXITCODE } else { 1 } }; $p = ($executionContext.SessionState.Path.CurrentLocation.Path -replace '\\','/'); [Console]::Write("$([char]27)]133;D;$ec$([char]7)$([char]27)]7;file://localhost/$p$([char]7)$([char]27)]133;A$([char]7)"); & $global:__vps_orig_prompt }"#;
        c.args(["-NoLogo", "-NoExit"]);
        if headless {
            c.args(["-Command", init_hook]);
        }
        c.env("TERM", "xterm-256color");
        c.env("TERM_PROGRAM", "vps-switch");
        if let Some(ref cwd) = initial_cwd {
            c.cwd(cwd);
        } else if let Ok(home) = std::env::var("USERPROFILE") {
            c.cwd(home);
        }
        c
    };

    #[cfg(not(windows))]
    let cmd = {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut c = CommandBuilder::new(shell);
        c.env("TERM", "xterm-256color");
        c.env("TERM_PROGRAM", "vps-switch");
        if let Some(ref cwd) = initial_cwd {
            c.cwd(cwd);
        } else if let Ok(home) = std::env::var("HOME") {
            c.cwd(home);
        }
        c
    };

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("启动本机 PowerShell 失败")?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("克隆 PTY reader 失败")?;
    let mut writer = pair.master.take_writer().context("获取 PTY writer 失败")?;
    let master = pair.master;

    let state = Arc::new(Mutex::new(PaneInternalState::new(
        cols,
        rows,
        initial_cwd,
        headless,
    )));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PaneCommand>();
    let (broadcast_tx, _) = broadcast::channel::<PaneBroadcast>(512);

    let handle = Arc::new(PaneHandle {
        id: pane_id.clone(),
        host_id: host_id.clone(),
        state: state.clone(),
        cmd_tx,
        broadcast_tx: broadcast_tx.clone(),
        input_lock: tokio::sync::Mutex::new(()),
    });

    let state_read = state.clone();
    let bcast_read = broadcast_tx.clone();
    let pid_read = pane_id.clone();
    let hid_read = host_id.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    let meta_opt = {
                        let mut guard = state_read.lock().unwrap();
                        let changed = guard.feed_bytes(&chunk);
                        if changed {
                            Some(build_meta_from_guard(&pid_read, &hid_read, &guard))
                        } else {
                            None
                        }
                    };
                    let _ = bcast_read.send(PaneBroadcast::Output(chunk));
                    if let Some(m) = meta_opt {
                        let _ = bcast_read.send(PaneBroadcast::Meta(m));
                    }
                }
            }
        }

        let final_meta = {
            let mut guard = state_read.lock().unwrap();
            guard.exited = true;
            guard.running = false;
            build_meta_from_guard(&pid_read, &hid_read, &guard)
        };
        let _ = bcast_read.send(PaneBroadcast::Meta(final_meta));
    });

    let state_cmd = state.clone();
    let bcast_cmd = broadcast_tx.clone();
    tokio::spawn(async move {
        let mut initial_input = true;
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                PaneCommand::Input { bytes, sent } => {
                    if initial_input && sent.is_some() {
                        if !wait_state(&state_cmd, Duration::from_secs(6), |g| {
                            prompt_settled(g, 250)
                        })
                        .await
                        {
                            let _ = sent
                                .unwrap()
                                .send(Err(anyhow::anyhow!("终端初始提示符尚未就绪")));
                            continue;
                        }
                    }
                    initial_input = false;
                    let seq_before = state_cmd.lock().unwrap().output_seq;
                    if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
                        let mut guard = state_cmd.lock().unwrap();
                        guard.running = true;
                    }
                    let result = writer.write_all(&bytes).and_then(|_| writer.flush());
                    if let Some(sent) = sent {
                        let _ = sent.send(result.map(|_| seq_before).map_err(Into::into));
                    }
                }
                PaneCommand::Paste(text) => {
                    let payload = encode_safe_paste(&state_cmd, &text);
                    let _ = writer.write_all(&payload);
                    let _ = writer.flush();
                }
                PaneCommand::Resize(c, r) => {
                    let meta = {
                        let mut guard = state_cmd.lock().unwrap();
                        guard.resize(c, r);
                        build_meta_from_guard(&pane_id, &host_id, &guard)
                    };
                    let _ = master.resize(PtySize {
                        rows: r.max(1),
                        cols: c.max(2),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    let _ = bcast_cmd.send(PaneBroadcast::Meta(meta));
                }
                PaneCommand::Close => {
                    let _ = child.kill();
                    break;
                }
            }
        }
    });

    Ok(handle)
}

// ==================== 远端 Shell Integration 注入（OSC 133 / OSC 7，零安装） ====================
//
// 连接时在 pty-req 里就把远端回显关掉（`Pty::ECHO = 0`），随后静默写入探针与钩子——
// 写进去的每一行都不会出现在屏幕上，因此没有任何东西需要擦除。注入结束（或中途放弃）
// 时把回显还给用户。任一环节超时/异常都安全降级：退回提示符启发式判定，绝不把终端弄坏。

/// 探测当前 shell（OSC 777 私有标记，对屏幕不可见）
const SHELL_PROBE_LINE: &str = r#"printf '\033]777;vps-shell:%s|%s|%s|%s\007' "$0" "$BASH_VERSION" "$ZSH_VERSION" "$FISH_VERSION""#;

/// bash：PROMPT_COMMAND 出 D/7/A，DEBUG trap 出 C。
/// __vps_ready 守卫防止 PROMPT_COMMAND 自身误触发 C；trap 内 `return $e` 原样保留
/// 状态码，保证 __vps_pp 里 `local e=$?` 拿到的仍是用户命令的真实退出码。
const INJECT_BASH: &str = r#"__vps_pp(){ local e=$?; printf '\033]133;D;%s\007\033]7;file://%s%s\007\033]133;A\007' "$e" "${HOSTNAME:-localhost}" "$PWD"; }; __vps_pre(){ local e=$?; if [ -n "$__vps_ready" ]; then __vps_ready=; printf '\033]133;C\007'; fi; return $e; }; trap '__vps_pre' DEBUG; PROMPT_COMMAND="__vps_pp${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; __vps_ready=1"#;

/// zsh：precmd/preexec 一对干净回调
const INJECT_ZSH: &str = r#"__vps_pp(){ local e=$?; printf '\033]133;D;%s\007\033]7;file://%s%s\007\033]133;A\007' "$e" "${HOST:-localhost}" "$PWD"; }; __vps_pre(){ printf '\033]133;C\007'; }; typeset -ga precmd_functions preexec_functions; precmd_functions+=(__vps_pp); preexec_functions+=(__vps_pre)"#;

/// fish：事件函数
const INJECT_FISH: &str = r#"function __vps_pp --on-event fish_prompt; printf '\e]133;D;%s\a\e]7;file://%s%s\a\e]133;A\a' $status (hostname) $PWD; end; function __vps_pre --on-event fish_preexec; printf '\e]133;C\a'; end"#;

/// ash/dash 等 POSIX shell：只能挂 PS1 命令替换。$? 在该上下文不可靠，
/// 因此只发不带退出码的 D——命令完成判定照常坚如磐石，但绝不伪造退出码。
const INJECT_POSIX: &str = r#"__vps_pp(){ printf '\033]133;D\007\033]7;file://%s%s\007\033]133;A\007' "${HOSTNAME:-localhost}" "$PWD"; }; PS1='$(__vps_pp)'"$PS1""#;

/// 解析 OSC 777 探针载荷（形如 `-bash|5.2.21(1)-release||`），得出 shell 类型。
fn parse_shell_probe(payload: &str) -> Option<&'static str> {
    let fields: Vec<&str> = payload.split('|').collect();
    let f0 = fields.first().copied().unwrap_or("");
    let has = |i: usize| fields.get(i).map(|v| !v.is_empty()).unwrap_or(false);
    if has(1) || f0.contains("bash") {
        return Some("bash");
    }
    if has(2) || f0.contains("zsh") {
        return Some("zsh");
    }
    if has(3) || f0.contains("fish") {
        return Some("fish");
    }
    let base = f0.rsplit('/').next().unwrap_or("").trim_start_matches('-');
    if base == "sh" || base.contains("ash") || base.contains("dash") {
        return Some("posix");
    }
    None
}

fn injection_line_for(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(INJECT_BASH),
        "zsh" => Some(INJECT_ZSH),
        "fish" => Some(INJECT_FISH),
        "posix" => Some(INJECT_POSIX),
        _ => None,
    }
}

fn quiet_for(g: &PaneInternalState, ms: u64) -> bool {
    g.last_output_at.elapsed() >= Duration::from_millis(ms)
}

fn prompt_settled(g: &PaneInternalState, ms: u64) -> bool {
    if !quiet_for(g, ms) {
        return false;
    }
    let (_, cursor) = g.read_screen_grid(5);
    looks_like_shell_prompt(&cursor)
}

/// 等状态满足条件（由常驻读任务喂状态，本函数不碰通道）。true=满足，false=超时。
async fn wait_state<F>(
    state: &Arc<Mutex<PaneInternalState>>,
    timeout: Duration,
    mut cond: F,
) -> bool
where
    F: FnMut(&PaneInternalState) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        {
            let g = state.lock().unwrap();
            if cond(&g) {
                return true;
            }
            if g.exited {
                return false;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

/// Shell integration 注入（OSC 133 / OSC 7，零安装）。
///
/// 回显在 pty-req 阶段就已经关掉（`Pty::ECHO = 0`），注入写进去的每一行都不会出现在
/// 屏幕上——没有需要擦除的东西，也就不存在"擦错行"。代价是：注入期间用户自己也看不见
/// 输入，所以每一条退出路径都必须把回显还回去。
async fn inject_shell_integration(
    write: &ChannelWriteHalf<russh::client::Msg>,
    state: &Arc<Mutex<PaneInternalState>>,
) {
    /// 还回回显。注入的每一条退出路径都要经过这里。
    async fn restore_echo(
        write: &ChannelWriteHalf<russh::client::Msg>,
        state: &Arc<Mutex<PaneInternalState>>,
    ) {
        let seq_before = state.lock().unwrap().output_seq;
        if write.data(&b"stty echo 2>/dev/null\n"[..]).await.is_ok() {
            let _ = wait_state(state, Duration::from_secs(3), |g| {
                g.output_seq > seq_before && prompt_settled(g, 250)
            })
            .await;
        }
    }

    // P0：等首个提示符。失败（超时/Windows 原生 shell）就把回显还回去、原地退出。
    if !wait_state(state, Duration::from_secs(6), |g| prompt_settled(g, 300)).await {
        restore_echo(write, state).await;
        return;
    }
    let windows_shell = {
        let g = state.lock().unwrap();
        let (_, cursor) = g.read_screen_grid(5);
        prompt_is_windows_style(&cursor)
    };
    if windows_shell {
        restore_echo(write, state).await;
        return;
    }

    // P1：探测 shell 类型（回显已关，这一行在屏幕上不可见）
    let seq_before = state.lock().unwrap().output_seq;
    if write
        .data(format!("{SHELL_PROBE_LINE}\n").as_bytes())
        .await
        .is_err()
    {
        return;
    }
    if !wait_state(state, Duration::from_millis(2500), move |g| {
        g.output_seq > seq_before && g.shell_kind.is_some()
    })
    .await
    {
        restore_echo(write, state).await;
        return;
    }
    let Some(shell) = state.lock().unwrap().shell_kind else {
        restore_echo(write, state).await;
        return;
    };
    // P2：写入钩子。等待条件带 output_seq 护栏——回显已关，上一阶段的提示符仍在
    // 光标行上，没有护栏会在注入尚未执行时就误判"已完成"。
    let Some(inject_line) = injection_line_for(shell) else {
        restore_echo(write, state).await;
        return;
    };
    let seq_before_inject = state.lock().unwrap().output_seq;
    if write
        .data(format!("{inject_line}\n").as_bytes())
        .await
        .is_err()
    {
        return;
    }
    let _ = wait_state(state, Duration::from_secs(3), move |g| {
        g.output_seq > seq_before_inject && prompt_settled(g, 250)
    })
    .await;

    // P3：还回回显。注入疑似失败（例如 shell 语法报错）也照样还——报错上下文留在屏幕上
    // 由用户自己看，绝不为了"擦干净"去动光标。
    restore_echo(write, state).await;
}

async fn spawn_ssh_pty_pane(
    pool: &SshPool,
    db: &Database,
    pane_id: String,
    host_id: String,
    cols: u16,
    rows: u16,
    initial_cwd: Option<String>,
    headless: bool,
) -> Result<Arc<PaneHandle>> {
    let (record, channel) = open_pane_channel(pool, db, &host_id).await?;

    // 注入只服务于 agent 链路（MCP 的无头 pane）：手点开/托盘打开的交互式终端是纯 pty，
    // 一个字节都不注入——终端是给人用的，不需要向里写任何东西。
    // Windows 主机（PowerShell / cmd）也不注入 POSIX 钩子，也不去碰它的回显。
    let is_windows = record
        .os_info
        .as_ref()
        .and_then(|o| o.os.as_deref())
        .map(|s| s.eq_ignore_ascii_case("windows"))
        .unwrap_or(false);
    let skip_injection = !headless || is_windows;

    let state = Arc::new(Mutex::new(PaneInternalState::new(
        cols,
        rows,
        initial_cwd.or_else(|| Some("~".to_string())),
        headless,
    )));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PaneCommand>();
    let (broadcast_tx, _) = broadcast::channel::<PaneBroadcast>(512);

    let handle = Arc::new(PaneHandle {
        id: pane_id.clone(),
        host_id: host_id.clone(),
        state: state.clone(),
        cmd_tx,
        broadcast_tx: broadcast_tx.clone(),
        input_lock: tokio::sync::Mutex::new(()),
    });

    let (mut read_half, write_half) = channel.split();

    // 读半边立刻独立成任务：从这一刻起服务端发出的每一个字节都会被记录。
    // （此前是"先把 pty/shell 请求发完、再开始读"，落在那个窗口里的字节会静默丢掉——
    //  登录横幅 MOTD 正是卡在这个窗口里没的。）
    {
        let state = state.clone();
        let bcast = broadcast_tx.clone();
        let pid = pane_id.clone();
        let hid = host_id.clone();
        tokio::spawn(async move {
            while let Some(msg) = read_half.wait().await {
                match msg {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                        let chunk = data.to_vec();
                        let meta_opt = {
                            let mut guard = state.lock().unwrap();
                            if guard.feed_bytes(&chunk) {
                                Some(build_meta_from_guard(&pid, &hid, &guard))
                            } else {
                                None
                            }
                        };
                        let _ = bcast.send(PaneBroadcast::Output(chunk));
                        if let Some(m) = meta_opt {
                            let _ = bcast.send(PaneBroadcast::Meta(m));
                        }
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        let mut guard = state.lock().unwrap();
                        guard.last_exit_code = Some(exit_status as i32);
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
            let final_meta = {
                let mut guard = state.lock().unwrap();
                guard.exited = true;
                guard.running = false;
                build_meta_from_guard(&pid, &hid, &guard)
            };
            let _ = bcast.send(PaneBroadcast::Meta(final_meta));
        });
    }

    // 写半边：请求 pty（注入前先把远端回显关掉，注入写进去的每一行都不会留痕）→ 请求 shell
    // → 静默注入 shell integration → 进入输入/控制循环。
    tokio::spawn(async move {
        let modes: &[(Pty, u32)] = if skip_injection {
            &[]
        } else {
            &[(Pty::ECHO, 0)]
        };
        if write_half
            .request_pty(
                false,
                "xterm-256color",
                cols as u32,
                rows as u32,
                0,
                0,
                modes,
            )
            .await
            .is_err()
        {
            return;
        }
        if write_half.request_shell(true).await.is_err() {
            return;
        }
        if !skip_injection {
            inject_shell_integration(&write_half, &state).await;
        }

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                PaneCommand::Input { bytes, sent } => {
                    let seq_before = state.lock().unwrap().output_seq;
                    if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
                        state.lock().unwrap().running = true;
                    }
                    let result = write_half.data(&bytes[..]).await;
                    let failed = result.is_err();
                    if let Some(sent) = sent {
                        let _ = sent.send(result.map(|_| seq_before).map_err(Into::into));
                    }
                    if failed {
                        break;
                    }
                }
                PaneCommand::Paste(text) => {
                    let payload = encode_safe_paste(&state, &text);
                    if write_half.data(&payload[..]).await.is_err() {
                        break;
                    }
                }
                PaneCommand::Resize(c, r) => {
                    let meta = {
                        let mut guard = state.lock().unwrap();
                        guard.resize(c, r);
                        build_meta_from_guard(&pane_id, &host_id, &guard)
                    };
                    let _ = write_half
                        .window_change(c.max(2) as u32, r.max(1) as u32, 0, 0)
                        .await;
                    let _ = broadcast_tx.send(PaneBroadcast::Meta(meta));
                }
                PaneCommand::Close => {
                    let _ = write_half.close().await;
                    break;
                }
            }
        }
    });

    Ok(handle)
}

fn encode_safe_paste(state: &Arc<Mutex<PaneInternalState>>, text: &str) -> Vec<u8> {
    let bracketed = state.lock().unwrap().bracketed_paste_enabled();
    let sanitized: String = text.chars().filter(|&c| c != '\x1b' && c != '\0').collect();
    if bracketed {
        let mut out = Vec::with_capacity(sanitized.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}
