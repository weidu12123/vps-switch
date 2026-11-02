//! 工作区：板块一（VPS 仓库）+ 板块二（终端）+ 板块三/四（文件浏览器与传输）的状态机与布局容器。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::component::Root;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use crate::api::{Api, ApiCommand, ApiEvent, FileEntry, Host, Inspection, McpCallRecord, NetHistory};
use crate::file_browser::{FileBrowserState, editor_language_for};
use crate::host_modal::{HostModalState, render_delete_confirm, render_host_modal};
use crate::palette::Palette;
use crate::settings_page::SettingsState;
use crate::terminal::TerminalView;
use crate::{detail_drawer, file_browser, host_card, settings_page, terminal_stage, top_bar};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Workspace,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CenterStage {
    Terminal,
    Files,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightWing {
    Detail,
    Files,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    Row,
    Col,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusFilter {
    All,
    Online,
    Offline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleFilter {
    All,
    Primary,
    Proxy,
    Test,
    Archived,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Ok,
    Err,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: Instant,
}

pub struct OpenTab {
    pub host_id: String,
    pub mode: CenterStage,
    pub right_wing: Option<RightWing>,
    pub split: SplitDirection,
    pub panes: Vec<Entity<TerminalView>>,
    pub focused_pane: Option<usize>,
}

pub struct Workspace {
    pub api: Api,
    pub main_window: AnyWindowHandle,
    pub standalone_windows: HashMap<String, AnyWindowHandle>,
    pub standalone_panes: HashMap<String, Entity<TerminalView>>,
    pub focus_handle: FocusHandle,
    pub hosts: Vec<Host>,
    pub loading: bool,
    pub checking_all: bool,
    pub checking: HashSet<String>,

    pub open_tabs: Vec<OpenTab>,
    pub stage_host_id: Option<String>,
    pub center_stage: Option<CenterStage>,
    pub detail_host_id: Option<String>,
    pub right_wing: Option<RightWing>,
    pub show_sidebar_in_stage: bool,

    pub search: Entity<InputState>,
    pub search_query: String,
    pub filter_open: bool,
    pub status_filter: StatusFilter,
    pub favorite_only: bool,
    pub role_filter: RoleFilter,
    pub tag_filter: String,

    pub tag_input: Entity<InputState>,
    pub host_modal: Option<HostModalState>,
    pub deleting_host: Option<Host>,

    pub files: FileBrowserState,
    pub settings: SettingsState,

    pub is_dark: bool,
    pub terminal_dark: bool,
    pub page: Page,
    pub toast: Option<Toast>,

    pub inspections: HashMap<String, Inspection>,
    pub inspection_errors: HashMap<String, String>,
    pub net_history: NetHistory,
    pub inspecting_host: Option<String>,
    last_inspect: Option<Instant>,
    pub mock_terminal_for_test: bool,
}

impl Workspace {
    pub fn new(api: Api, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let main_window = window.window_handle();
        let focus_handle = cx.focus_handle();
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("搜索名称 / IP / 标签 (Ctrl+K)")
        });
        cx.observe(&search, |this, state, cx| {
            this.search_query = state.read(cx).value().to_string();
            cx.notify();
        })
        .detach();

        let tag_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("添加标签（回车保存）"));
        cx.subscribe_in(&tag_input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event
                && let Some(host_id) = this.right_wing_host().map(|h| h.id.clone())
            {
                this.add_tag_from_input(&host_id, window, cx);
            }
        })
        .detach();

        let files = FileBrowserState::new(window, cx);
        let settings = SettingsState::new(window, cx);

        api.send(ApiCommand::LoadHosts);
        api.send(ApiCommand::LoadSettings);
        Self::poll(cx);

        Self {
            api,
            main_window,
            standalone_windows: HashMap::new(),
            standalone_panes: HashMap::new(),
            focus_handle,
            hosts: Vec::new(),
            loading: true,
            checking_all: false,
            checking: HashSet::new(),
            open_tabs: Vec::new(),
            stage_host_id: None,
            center_stage: None,
            detail_host_id: None,
            right_wing: None,
            show_sidebar_in_stage: false,
            search,
            search_query: String::new(),
            filter_open: false,
            status_filter: StatusFilter::All,
            favorite_only: false,
            role_filter: RoleFilter::All,
            tag_filter: String::new(),
            tag_input,
            host_modal: None,
            deleting_host: None,
            files,
            settings,
            is_dark: false,
            terminal_dark: false,
            page: Page::Workspace,
            toast: None,
            inspections: HashMap::new(),
            inspection_errors: HashMap::new(),
            net_history: NetHistory::new(),
            inspecting_host: None,
            last_inspect: None,
            mock_terminal_for_test: false,
        }
    }

    pub fn set_hosts_for_test(&mut self, hosts: Vec<Host>, cx: &mut Context<Self>) {
        self.hosts = hosts;
        crate::tray::update_hosts(&self.hosts);
        self.loading = false;
        self.mock_terminal_for_test = true;
        // 为测试预置一份目录缓存，无需联网即可验证文件浏览器完整交互
        self.files.store_cache(
            "vip-hk",
            "",
            "/root".into(),
            Some("/".into()),
            vec![
                FileEntry {
                    name: "app".into(),
                    path: "/root/app".into(),
                    is_dir: true,
                    size: 0,
                    mtime: 1_760_000_000_000,
                    permissions: None,
                },
                FileEntry {
                    name: "nginx.conf".into(),
                    path: "/root/nginx.conf".into(),
                    is_dir: false,
                    size: 2048,
                    mtime: 1_760_000_000_000,
                    permissions: None,
                },
                FileEntry {
                    name: "backup.tar.gz".into(),
                    path: "/root/backup.tar.gz".into(),
                    is_dir: false,
                    size: 1_048_576,
                    mtime: 1_760_000_000_000,
                    permissions: None,
                },
            ],
        );
        self.settings.calls = vec![
            McpCallRecord {
                id: "call-1".into(),
                ts: "2026-10-17T12:00:00Z".into(),
                tool: "vps_exec".into(),
                host: "vip-hk".into(),
                args: "{\n  \"host\": \"vip-hk\",\n  \"command\": \"uname -a &&\\ndf -h &&\\nfree -m\"\n}".into(),
                ok: true,
                duration_ms: 82,
                summary: "Linux vip-hk 6.8.0 x86_64\nFilesystem Size Used Avail\n/dev/vda1 40G 12G 28G".into(),
            },
            McpCallRecord {
                id: "call-2".into(),
                ts: "2026-10-17T12:01:00Z".into(),
                tool: "vps_list".into(),
                host: String::new(),
                args: "{}".into(),
                ok: true,
                duration_ms: 4,
                summary: String::new(),
            },
            McpCallRecord {
                id: "call-3".into(),
                ts: "2026-10-17T12:02:00Z".into(),
                tool: "vps_inspect".into(),
                host: "us-node".into(),
                args: "{\"host\":\"us-node\"}".into(),
                ok: false,
                duration_ms: 1500,
                summary: "connection timeout".into(),
            },
        ];
        cx.notify();
    }

    // ---------- 查询 ----------

    pub fn host_by_id(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|host| host.id == id)
    }

    pub fn has_stage(&self) -> bool {
        self.center_stage.is_some() && self.stage_host_id.is_some()
    }

    pub fn stage_host(&self) -> Option<&Host> {
        self.stage_host_id
            .as_deref()
            .and_then(|id| self.host_by_id(id))
    }

    pub fn right_wing_host(&self) -> Option<&Host> {
        self.detail_host_id
            .as_deref()
            .and_then(|id| self.host_by_id(id))
            .or_else(|| self.stage_host())
    }

    pub fn active_tab_index(&self) -> Option<usize> {
        let id = self.stage_host_id.as_deref()?;
        self.open_tabs.iter().position(|tab| tab.host_id == id)
    }

    /// 获取某台主机当前焦点终端 pane 的 OSC 7 实时工作目录（供文件浏览器一键同步）。
    pub fn active_terminal_cwd(&self, host_id: &str, cx: &App) -> Option<String> {
        let tab = self.open_tabs.iter().find(|t| t.host_id == host_id)?;
        let idx = tab
            .focused_pane
            .filter(|i| *i < tab.panes.len())
            .or_else(|| (!tab.panes.is_empty()).then(|| tab.panes.len() - 1))?;
        tab.panes[idx].read(cx).meta.cwd.clone()
    }

    pub fn online_count(&self) -> usize {
        self.hosts
            .iter()
            .filter(|host| !host.archived && host.status == "online")
            .count()
    }

    pub fn offline_count(&self) -> usize {
        self.hosts
            .iter()
            .filter(|host| !host.archived && host.status == "offline")
            .count()
    }

    pub fn total_active_count(&self) -> usize {
        self.hosts.iter().filter(|host| !host.archived).count()
    }

    pub fn favorite_count(&self) -> usize {
        self.hosts
            .iter()
            .filter(|host| !host.archived && host.favorited)
            .count()
    }

    pub fn all_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .hosts
            .iter()
            .filter(|host| !host.archived || self.role_filter == RoleFilter::Archived)
            .flat_map(|host| host.tags.clone())
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }

    pub fn has_active_filter(&self) -> bool {
        self.status_filter != StatusFilter::All
            || self.favorite_only
            || self.role_filter != RoleFilter::All
            || !self.tag_filter.is_empty()
    }

    pub fn reset_filters(&mut self) {
        self.status_filter = StatusFilter::All;
        self.favorite_only = false;
        self.role_filter = RoleFilter::All;
        self.tag_filter.clear();
    }

    pub fn filtered_hosts(&self) -> Vec<&Host> {
        let query = self.search_query.trim().to_lowercase();
        self.hosts
            .iter()
            .filter(|host| {
                if self.role_filter == RoleFilter::Archived {
                    if !host.archived {
                        return false;
                    }
                } else {
                    if host.archived {
                        return false;
                    }
                    let role_ok = match self.role_filter {
                        RoleFilter::All => true,
                        RoleFilter::Primary => host.role.as_deref() == Some("primary"),
                        RoleFilter::Proxy => host.role.as_deref() == Some("proxy"),
                        RoleFilter::Test => host.role.as_deref() == Some("test"),
                        RoleFilter::Archived => true,
                    };
                    if !role_ok {
                        return false;
                    }
                }
                if self.favorite_only && !host.favorited {
                    return false;
                }
                match self.status_filter {
                    StatusFilter::All => {}
                    StatusFilter::Online if host.status != "online" => return false,
                    StatusFilter::Offline if host.status != "offline" => return false,
                    _ => {}
                }
                if !self.tag_filter.is_empty() && !host.tags.contains(&self.tag_filter) {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                let haystack = format!(
                    "{} {} {} {} {} {}",
                    host.name,
                    host.addr,
                    host.port,
                    host.username,
                    host.pretty_name.as_deref().unwrap_or(""),
                    host.tags.join(" ")
                )
                .to_lowercase();
                haystack.contains(&query)
            })
            .collect()
    }

    // ---------- 交互动作 ----------

    pub fn show_toast(&mut self, text: impl Into<String>, kind: ToastKind, cx: &mut Context<Self>) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            at: Instant::now(),
        });
        cx.notify();
    }

    pub fn sort_hosts(&mut self) {
        self.hosts.sort_by(|a, b| {
            let a_local = a.kind == "local" || a.id == "local";
            let b_local = b.kind == "local" || b.id == "local";
            b_local
                .cmp(&a_local)
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| a.sort_order.cmp(&b.sort_order))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    fn replace_host(&mut self, mut updated: Host) {
        match self.hosts.iter_mut().find(|host| host.id == updated.id) {
            Some(existing) => *existing = updated,
            None => {
                let min_order = self
                    .hosts
                    .iter()
                    .filter(|h| h.kind != "local" && h.id != "local")
                    .map(|h| h.sort_order)
                    .min()
                    .unwrap_or(0);
                updated.sort_order = min_order - 1;
                let insert_pos = self
                    .hosts
                    .iter()
                    .position(|host| host.kind != "local")
                    .unwrap_or(self.hosts.len());
                self.hosts.insert(insert_pos, updated);
            }
        }
        self.sort_hosts();
    }

    pub fn toggle_detail(&mut self, host_id: &str, cx: &mut Context<Self>) {
        self.filter_open = false;
        if self.right_wing == Some(RightWing::Detail)
            && self.detail_host_id.as_deref() == Some(host_id)
        {
            self.right_wing = None;
            if !self.has_stage() {
                self.detail_host_id = None;
            }
        } else {
            self.detail_host_id = Some(host_id.to_owned());
            self.right_wing = Some(RightWing::Detail);
            self.last_inspect = None;
            if let Some(host) = self.host_by_id(host_id)
                && host.status != "offline"
            {
                self.api.send(ApiCommand::PrewarmFiles(host_id.to_owned()));
                self.prefetch_files_dir(host_id, "");
            }
        }
        self.sync_current_tab();
        cx.notify();
    }

    pub fn toggle_right_wing(&mut self, mode: RightWing, cx: &mut Context<Self>) {
        let Some(stage_id) = self.stage_host_id.clone() else {
            return;
        };
        if self.right_wing == Some(mode) && self.detail_host_id.as_deref() == Some(&stage_id) {
            self.right_wing = None;
        } else {
            self.detail_host_id = Some(stage_id.clone());
            self.right_wing = Some(mode);
            if mode == RightWing::Detail {
                self.last_inspect = None;
            } else if mode == RightWing::Files {
                let cwd = self.tab_terminal_cwd(&stage_id, cx);
                self.ensure_files_host(&stage_id, cwd.as_deref());
            }
        }
        self.sync_current_tab();
        cx.notify();
    }

    fn tab_terminal_cwd(&self, host_id: &str, cx: &App) -> Option<String> {
        let tab = self.open_tabs.iter().find(|t| t.host_id == host_id)?;
        let last = tab.panes.last()?;
        last.read(cx).meta.cwd.clone()
    }

    pub fn open_terminal(&mut self, host_id: &str, cx: &mut Context<Self>) {
        self.page = Page::Workspace;
        self.filter_open = false;
        self.sync_current_tab();
        if let Some(host) = self.host_by_id(host_id)
            && host.status != "offline"
        {
            self.api.send(ApiCommand::PrewarmFiles(host_id.to_owned()));
            self.prefetch_files_dir(host_id, "");
        }
        let index = match self
            .open_tabs
            .iter()
            .position(|tab| tab.host_id == host_id)
        {
            Some(idx) => {
                self.open_tabs[idx].mode = CenterStage::Terminal;
                idx
            }
            None => {
                let restore_detail = self.right_wing == Some(RightWing::Detail)
                    && self.detail_host_id.as_deref() == Some(host_id);
                self.open_tabs.push(OpenTab {
                    host_id: host_id.to_owned(),
                    mode: CenterStage::Terminal,
                    right_wing: restore_detail.then_some(RightWing::Detail),
                    split: SplitDirection::Row,
                    panes: Vec::new(),
                    focused_pane: None,
                });
                self.open_tabs.len() - 1
            }
        };
        self.stage_host_id = Some(host_id.to_owned());
        self.center_stage = Some(CenterStage::Terminal);
        self.detail_host_id = Some(host_id.to_owned());
        self.right_wing = self.open_tabs[index].right_wing;
        if self.open_tabs[index].panes.is_empty() {
            self.create_pane(index, cx);
        }
        cx.notify();
    }

    pub fn create_pane(&mut self, tab_index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.open_tabs.get(tab_index) else {
            return;
        };
        let host_id = tab.host_id.clone();
        let dark = self.terminal_dark;
        let is_mock = self.mock_terminal_for_test;
        let pane = cx.new(|cx| {
            if is_mock {
                TerminalView::new_mock(host_id, dark, "root@vip-hk:~# ", cx)
            } else {
                TerminalView::new(host_id, dark, cx)
            }
        });
        cx.observe(&pane, |this, pane, cx| {
            let (dirty, host_id, connected) = {
                let p = pane.read(cx);
                (
                    p.status_dirty,
                    p.host_id.clone(),
                    !p.connecting && p.pane_id.is_some() && p.create_error.is_none(),
                )
            };
            if dirty {
                pane.update(cx, |p, _| p.status_dirty = false);
                if connected
                    && let Some(h) = this.hosts.iter_mut().find(|h| h.id == host_id)
                {
                    h.status = "online".into();
                    crate::tray::update_hosts(&this.hosts);
                }
                this.api.send(ApiCommand::LoadHosts);
            }
            cx.notify();
        })
        .detach();
        if let Some(tab) = self.open_tabs.get_mut(tab_index) {
            tab.panes.push(pane);
            tab.focused_pane = Some(tab.panes.len() - 1);
        }
        cx.notify();
    }

    pub fn split(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.active_tab_index() else {
            return;
        };
        self.open_tabs[index].split = direction;
        self.create_pane(index, cx);
        if let Some(last_pane) = self.open_tabs[index].panes.last() {
            let fh = last_pane.read(cx).focus_handle.clone();
            window.focus(&fh, cx);
        }
    }

    pub fn close_pane(&mut self, tab_index: usize, pane_index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.open_tabs.get_mut(tab_index) else {
            return;
        };
        if pane_index >= tab.panes.len() {
            return;
        }
        let removed = tab.panes.remove(pane_index);
        if let Some(pane_id) = removed.read(cx).pane_id.clone() {
            self.api.send(ApiCommand::DeletePane(pane_id));
        }
        tab.focused_pane = if tab.panes.is_empty() {
            None
        } else {
            Some(pane_index.min(tab.panes.len() - 1))
        };
        if tab.panes.is_empty() {
            let host_id = tab.host_id.clone();
            self.close_tab(&host_id, cx);
        } else {
            cx.notify();
        }
    }

    pub fn focus_pane(&mut self, tab_index: usize, pane_index: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.open_tabs.get_mut(tab_index) {
            tab.focused_pane = Some(pane_index);
            cx.notify();
        }
    }

    pub fn refresh_active_terminal_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.active_tab_index() else {
            return;
        };
        let host_id = self.open_tabs[tab_idx].host_id.clone();
        let host_name = self
            .host_by_id(&host_id)
            .map(|h| h.name.clone())
            .unwrap_or_else(|| host_id.clone());
        let tab = &self.open_tabs[tab_idx];
        let pane_idx = tab
            .focused_pane
            .filter(|i| *i < tab.panes.len())
            .or_else(|| (!tab.panes.is_empty()).then(|| tab.panes.len() - 1));
        let Some(idx) = pane_idx else {
            return;
        };
        let pane = tab.panes[idx].clone();
        let focus = pane.read(cx).focus_handle.clone();
        pane.update(cx, |view, cx| {
            view.reconnect_fresh(cx);
        });
        window.focus(&focus, cx);
        self.show_toast(format!("正在刷新「{host_name}」终端…"), ToastKind::Ok, cx);
    }

    pub fn toggle_terminal_dark(&mut self, cx: &mut Context<Self>) {
        self.terminal_dark = !self.terminal_dark;
        let dark = self.terminal_dark;
        for tab in &self.open_tabs {
            for pane in &tab.panes {
                pane.update(cx, |view, cx| view.set_dark(dark, cx));
            }
        }
        for pane in self.standalone_panes.values() {
            pane.update(cx, |view, cx| view.set_dark(dark, cx));
        }
        cx.notify();
    }

    pub fn request_show_main_window(&self, cx: &mut Context<Self>) {
        self.api.send(ApiCommand::LoadHosts);
        let handle = self.main_window;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, _cx| {
                crate::tray::show_window(window);
            });
        });
    }

    /// 响应 `/api/events` 的 `revealTerminal` 事件：自动附着到指定 pane、切换到终端舞台并弹 Toast。
    pub fn reveal_terminal(
        &mut self,
        host_id: &str,
        pane_meta: crate::terminal::PaneMeta,
        reason: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.request_show_main_window(cx);
        self.page = Page::Workspace;
        self.sync_current_tab();

        let index = match self.open_tabs.iter().position(|tab| tab.host_id == host_id) {
            Some(idx) => {
                self.open_tabs[idx].mode = CenterStage::Terminal;
                idx
            }
            None => {
                self.open_tabs.push(OpenTab {
                    host_id: host_id.to_owned(),
                    mode: CenterStage::Terminal,
                    right_wing: None,
                    split: SplitDirection::Row,
                    panes: Vec::new(),
                    focused_pane: None,
                });
                self.open_tabs.len() - 1
            }
        };

        let existing_pane_idx = self.open_tabs[index]
            .panes
            .iter()
            .position(|p| p.read(cx).pane_id.as_deref() == Some(pane_meta.id.as_str()));

        if let Some(p_idx) = existing_pane_idx {
            self.open_tabs[index].focused_pane = Some(p_idx);
        } else {
            let dark = self.terminal_dark;
            let is_mock = self.mock_terminal_for_test;
            let hid = host_id.to_owned();
            let meta_clone = pane_meta.clone();
            let pane = cx.new(|cx| {
                if is_mock {
                    let mut v = TerminalView::new_mock(hid, dark, "root@vip-hk:~# ", cx);
                    v.pane_id = Some(meta_clone.id.clone());
                    v.meta = meta_clone;
                    v
                } else {
                    TerminalView::new_attached(hid, meta_clone, dark, cx)
                }
            });
            cx.observe(&pane, |this, pane, cx| {
            let (dirty, host_id, connected) = {
                let p = pane.read(cx);
                (
                    p.status_dirty,
                    p.host_id.clone(),
                    !p.connecting && p.pane_id.is_some() && p.create_error.is_none(),
                )
            };
            if dirty {
                pane.update(cx, |p, _| p.status_dirty = false);
                if connected
                    && let Some(h) = this.hosts.iter_mut().find(|h| h.id == host_id)
                {
                    h.status = "online".into();
                    crate::tray::update_hosts(&this.hosts);
                }
                this.api.send(ApiCommand::LoadHosts);
            }
            cx.notify();
        })
        .detach();
            if let Some(tab) = self.open_tabs.get_mut(index) {
                tab.panes.push(pane);
                tab.focused_pane = Some(tab.panes.len() - 1);
            }
        }

        self.stage_host_id = Some(host_id.to_owned());
        self.center_stage = Some(CenterStage::Terminal);
        self.detail_host_id = Some(host_id.to_owned());
        self.right_wing = self.open_tabs[index].right_wing;

        let host_name = self
            .host_by_id(host_id)
            .map(|h| h.name.clone())
            .unwrap_or_else(|| host_id.to_owned());
        let msg = match reason.as_deref().filter(|s| !s.is_empty()) {
            Some(r) => format!("Agent 唤起「{host_name}」终端: {r}"),
            None => format!("Agent 已唤起「{host_name}」终端供你操作"),
        };
        self.show_toast(msg, ToastKind::Ok, cx);
    }

    pub fn popout_terminal(&mut self, host_id: &str, cx: &mut Context<Self>) {
        let name = self
            .host_by_id(host_id)
            .map(|h| h.name.clone())
            .unwrap_or_else(|| host_id.to_owned());

        // 若该主机已有打开的独立终端窗口，直接唤醒并前置聚焦
        if let Some(&existing) = self.standalone_windows.get(host_id) {
            if existing
                .update(cx, |_, window, _cx| {
                    crate::tray::show_window(window);
                })
                .is_ok()
            {
                self.show_toast(
                    format!("已唤醒「{name}」独立终端窗口"),
                    ToastKind::Ok,
                    cx,
                );
                return;
            }
            self.standalone_windows.remove(host_id);
            self.standalone_panes.remove(host_id);
        }

        let title = format!("{name} · 终端 · VPS Switch");
        let hid = host_id.to_owned();
        let hid_for_close = host_id.to_owned();
        let dark = self.terminal_dark;
        let is_mock = self.mock_terminal_for_test;
        let ws_weak = cx.entity().downgrade();
        let mut created_view: Option<Entity<TerminalView>> = None;
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(960.), px(640.)),
                cx,
            ))),
            ..WindowOptions::default()
        };
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            window.set_window_title(&title);
            let view = cx.new(|cx| {
                if is_mock {
                    TerminalView::new_mock(hid, dark, "root@vip-hk:~# ", cx)
                } else {
                    TerminalView::new(hid, dark, cx)
                }
            });
            created_view = Some(view.clone());
            let view_weak = view.downgrade();
            window.on_window_should_close(cx, move |_window, cx| {
                let pane_id = view_weak
                    .upgrade()
                    .and_then(|v| v.read(cx).pane_id.clone());
                if let Some(ws) = ws_weak.upgrade() {
                    let _ = ws.update(cx, |w, _| {
                        w.standalone_windows.remove(&hid_for_close);
                        w.standalone_panes.remove(&hid_for_close);
                        if let Some(pid) = pane_id {
                            w.api.send(ApiCommand::DeletePane(pid));
                        }
                    });
                }
                true
            });
            let fh = view.read(cx).focus_handle.clone();
            window.focus(&fh, cx);
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            self.standalone_windows
                .insert(host_id.to_owned(), handle.into());
            if let Some(v) = created_view {
                cx.observe(&v, |this, pane, cx| {
                    let (dirty, host_id, connected) = {
                        let p = pane.read(cx);
                        (
                            p.status_dirty,
                            p.host_id.clone(),
                            !p.connecting && p.pane_id.is_some() && p.create_error.is_none(),
                        )
                    };
                    if dirty {
                        pane.update(cx, |p, _| p.status_dirty = false);
                        if connected
                            && let Some(h) = this.hosts.iter_mut().find(|h| h.id == host_id)
                        {
                            h.status = "online".into();
                            crate::tray::update_hosts(&this.hosts);
                        }
                        this.api.send(ApiCommand::LoadHosts);
                    }
                    cx.notify();
                })
                .detach();
                self.standalone_panes.insert(host_id.to_owned(), v);
            }
        }
        self.show_toast(
            format!("已在独立原生窗口打开「{name}」终端"),
            ToastKind::Ok,
            cx,
        );
    }

    pub fn open_files(&mut self, host_id: &str, cx: &mut Context<Self>) {
        self.page = Page::Workspace;
        self.filter_open = false;
        self.sync_current_tab();
        match self
            .open_tabs
            .iter()
            .position(|tab| tab.host_id == host_id)
        {
            Some(idx) => {
                self.open_tabs[idx].mode = CenterStage::Files;
                if self.open_tabs[idx].right_wing == Some(RightWing::Files) {
                    self.open_tabs[idx].right_wing = None;
                }
            }
            None => {
                self.open_tabs.push(OpenTab {
                    host_id: host_id.to_owned(),
                    mode: CenterStage::Files,
                    right_wing: None,
                    split: SplitDirection::Row,
                    panes: Vec::new(),
                    focused_pane: None,
                });
            }
        }
        self.stage_host_id = Some(host_id.to_owned());
        self.center_stage = Some(CenterStage::Files);
        self.detail_host_id = Some(host_id.to_owned());
        if self.right_wing == Some(RightWing::Files) {
            self.right_wing = None;
        }
        let cwd = self.tab_terminal_cwd(host_id, cx);
        self.ensure_files_host(host_id, cwd.as_deref());
        cx.notify();
    }

    fn sync_current_tab(&mut self) {
        if let Some(tab) = self.active_tab_index()
            && let Some(mode) = self.center_stage
        {
            self.open_tabs[tab].mode = mode;
            self.open_tabs[tab].right_wing = self.right_wing;
        }
    }

    pub fn close_stage(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Workspace;
        self.filter_open = false;
        self.sync_current_tab();
        self.center_stage = None;
        self.stage_host_id = None;
        if self.right_wing == Some(RightWing::Files) {
            self.right_wing = None;
        }
        cx.notify();
    }

    pub fn activate_tab(&mut self, host_id: &str, cx: &mut Context<Self>) {
        self.page = Page::Workspace;
        self.filter_open = false;
        self.sync_current_tab();
        let Some(tab) = self.open_tabs.iter().find(|tab| tab.host_id == host_id) else {
            return;
        };
        let mode = tab.mode;
        let right_wing = tab.right_wing;
        self.stage_host_id = Some(host_id.to_owned());
        self.center_stage = Some(mode);
        self.detail_host_id = Some(host_id.to_owned());
        self.right_wing = right_wing;
        if mode == CenterStage::Terminal {
            if let Some(idx) = self.active_tab_index()
                && self.open_tabs[idx].panes.is_empty()
            {
                self.create_pane(idx, cx);
            }
        } else if mode == CenterStage::Files || right_wing == Some(RightWing::Files) {
            let cwd = self.tab_terminal_cwd(host_id, cx);
            self.ensure_files_host(host_id, cwd.as_deref());
        }
        cx.notify();
    }

    pub fn close_tab(&mut self, host_id: &str, cx: &mut Context<Self>) {
        let Some(index) = self
            .open_tabs
            .iter()
            .position(|tab| tab.host_id == host_id)
        else {
            return;
        };
        let removed_tab = self.open_tabs.remove(index);
        for pane in &removed_tab.panes {
            if let Some(pane_id) = pane.read(cx).pane_id.clone() {
                self.api.send(ApiCommand::DeletePane(pane_id));
            }
        }
        if self.stage_host_id.as_deref() == Some(host_id) {
            match self.open_tabs.last() {
                Some(last) => {
                    let last_id = last.host_id.clone();
                    self.activate_tab(&last_id, cx);
                }
                None => self.close_stage(cx),
            }
        }
        cx.notify();
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.show_sidebar_in_stage = !self.show_sidebar_in_stage;
        cx.notify();
    }

    pub fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.is_dark = !self.is_dark;
        self.terminal_dark = self.is_dark;
        let mode = if self.is_dark {
            gpui_kit::component::ThemeMode::Dark
        } else {
            gpui_kit::component::ThemeMode::Light
        };
        gpui_kit::component::Theme::change(mode, None, cx);
        let dark = self.terminal_dark;
        for tab in &self.open_tabs {
            for pane in &tab.panes {
                pane.update(cx, |view, cx| view.set_dark(dark, cx));
            }
        }
        for pane in self.standalone_panes.values() {
            pane.update(cx, |view, cx| view.set_dark(dark, cx));
        }
        cx.notify();
    }

    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.filter_open = false;
        if self.page == Page::Settings {
            self.page = Page::Workspace;
        } else {
            self.page = Page::Settings;
            self.api.send(ApiCommand::LoadSettings);
            self.reload_mcp_calls(cx);
        }
        cx.notify();
    }

    pub fn inspect_now(&mut self, host_id: &str, cx: &mut Context<Self>) {
        self.inspecting_host = Some(host_id.to_owned());
        self.last_inspect = Some(Instant::now());
        self.api.send(ApiCommand::Inspect(host_id.to_owned()));
        cx.notify();
    }

    pub fn check_all(&mut self, cx: &mut Context<Self>) {
        if self.checking_all {
            return;
        }
        self.checking_all = true;
        self.show_toast(
            "正在并发探索全部主机在线状态与实时硬件快照...",
            ToastKind::Ok,
            cx,
        );
        self.api.send(ApiCommand::CheckAll);
        cx.notify();
    }

    pub fn patch_meta(&mut self, host_id: &str, patch: serde_json::Value, cx: &mut Context<Self>) {
        let mut need_resort = false;
        if let Some(host) = self.hosts.iter_mut().find(|h| h.id == host_id) {
            if let Some(fav) = patch.get("favorited").and_then(|v| v.as_bool()) {
                host.favorited = fav;
            }
            if let Some(pin) = patch.get("pinned").and_then(|v| v.as_bool()) {
                host.pinned = pin;
                need_resort = true;
            }
            if let Some(exp) = patch.get("mcpExposed").and_then(|v| v.as_bool()) {
                host.mcp_exposed = exp;
            }
            if let Some(role_val) = patch.get("role") {
                host.role = role_val.as_str().map(str::to_owned);
            }
            if let Some(tags_arr) = patch.get("tags").and_then(|v| v.as_array()) {
                host.tags = tags_arr
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
            }
        }
        if need_resort {
            self.sort_hosts();
        }
        self.api.send(ApiCommand::PatchMeta {
            host_id: host_id.to_owned(),
            patch,
        });
        crate::tray::update_hosts(&self.hosts);
        cx.notify();
    }

    pub fn toggle_host_mcp_exposed(&mut self, host_id: &str, cx: &mut Context<Self>) {
        let Some(host) = self.host_by_id(host_id) else {
            return;
        };
        let next = !host.mcp_exposed;
        self.patch_meta(host_id, serde_json::json!({ "mcpExposed": next }), cx);
    }

    pub fn set_all_hosts_mcp_exposed(&mut self, exposed: bool, cx: &mut Context<Self>) {
        for host in &mut self.hosts {
            host.mcp_exposed = exposed;
        }
        self.api.send(ApiCommand::SetMcpExposedBatch {
            exposed,
            host_ids: None,
        });
        self.show_toast(
            if exposed {
                "已将全部主机设为对 MCP 暴露"
            } else {
                "已隐藏全部主机的 MCP 暴露"
            },
            ToastKind::Ok,
            cx,
        );
        cx.notify();
    }

    pub fn toggle_favorite(&mut self, host_id: &str, cx: &mut Context<Self>) {
        let Some(host) = self.host_by_id(host_id) else {
            return;
        };
        let favorited = !host.favorited;
        self.patch_meta(host_id, serde_json::json!({ "favorited": favorited }), cx);
    }

    pub fn toggle_pinned(&mut self, host_id: &str, cx: &mut Context<Self>) {
        let Some(host) = self.host_by_id(host_id) else {
            return;
        };
        let pinned = !host.pinned;
        let name = host.name.clone();
        self.patch_meta(host_id, serde_json::json!({ "pinned": pinned }), cx);
        self.show_toast(
            if pinned {
                format!("已将「{name}」置顶到列表顶部")
            } else {
                format!("已取消「{name}」置顶")
            },
            ToastKind::Ok,
            cx,
        );
    }

    pub fn add_tag_from_input(
        &mut self,
        host_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let raw = self.tag_input.read(cx).value().trim().to_owned();
        if raw.is_empty() {
            return;
        }
        let Some(host) = self.host_by_id(host_id) else {
            return;
        };
        let mut tags = host.tags.clone();
        for part in raw
            .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '，' | '、'))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !tags.iter().any(|t| t == part) {
                tags.push(part.to_owned());
            }
        }
        self.tag_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.patch_meta(host_id, serde_json::json!({ "tags": tags }), cx);
    }

    pub fn remove_tag(&mut self, host_id: &str, tag: &str, cx: &mut Context<Self>) {
        let Some(host) = self.host_by_id(host_id) else {
            return;
        };
        let tags: Vec<String> = host
            .tags
            .iter()
            .filter(|t| t.as_str() != tag)
            .cloned()
            .collect();
        self.patch_meta(host_id, serde_json::json!({ "tags": tags }), cx);
    }

    pub fn reorder(&mut self, from_id: &str, to_id: &str, cx: &mut Context<Self>) {
        if from_id == to_id {
            return;
        }
        if self.host_by_id(from_id).is_some_and(|h| h.kind == "local")
            || self.host_by_id(to_id).is_some_and(|h| h.kind == "local")
        {
            return;
        }
        let mut ids: Vec<String> = self
            .hosts
            .iter()
            .filter(|host| host.kind != "local")
            .map(|host| host.id.clone())
            .collect();
        let (Some(from), Some(to)) = (
            ids.iter().position(|id| id == from_id),
            ids.iter().position(|id| id == to_id),
        ) else {
            return;
        };
        let moved = ids.remove(from);
        ids.insert(to, moved);

        if let (Some(f_idx), Some(t_idx)) = (
            self.hosts.iter().position(|h| h.id == from_id),
            self.hosts.iter().position(|h| h.id == to_id),
        ) {
            let item = self.hosts.remove(f_idx);
            self.hosts.insert(t_idx, item);
            for (idx, host) in self
                .hosts
                .iter_mut()
                .filter(|h| h.kind != "local" && h.id != "local")
                .enumerate()
            {
                host.sort_order = idx as i64;
            }
            self.sort_hosts();
        }

        self.api.send(ApiCommand::Reorder { ordered_ids: ids });
        cx.notify();
    }

    // ---------- 新建 / 编辑 / 删除主机弹窗 ----------

    pub fn open_create_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_open = false;
        self.host_modal = Some(HostModalState::new(None, window, cx));
        cx.notify();
    }

    pub fn open_edit_modal(
        &mut self,
        host_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host_by_id(host_id).cloned() else {
            return;
        };
        self.filter_open = false;
        self.host_modal = Some(HostModalState::new(Some(&host), window, cx));
        cx.notify();
    }

    pub fn close_host_modal(&mut self, cx: &mut Context<Self>) {
        self.host_modal = None;
        cx.notify();
    }

    pub fn submit_host_modal(&mut self, cx: &mut Context<Self>) {
        let Some(modal) = &mut self.host_modal else {
            return;
        };
        if modal.saving {
            return;
        }
        match modal.build_payload(cx) {
            Ok((payload, editing_id)) => {
                modal.saving = true;
                match editing_id {
                    Some(host_id) => self.api.send(ApiCommand::UpdateHost { host_id, payload }),
                    None => self.api.send(ApiCommand::CreateHost(payload)),
                }
                cx.notify();
            }
            Err(_) => {
                cx.notify();
            }
        }
    }

    pub fn request_delete_host(&mut self, host_id: &str, cx: &mut Context<Self>) {
        let Some(host) = self.host_by_id(host_id).cloned() else {
            return;
        };
        if host.kind == "local" {
            return;
        }
        self.deleting_host = Some(host);
        cx.notify();
    }

    pub fn cancel_delete_host(&mut self, cx: &mut Context<Self>) {
        self.deleting_host = None;
        cx.notify();
    }

    pub fn confirm_delete_host(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.deleting_host.take() else {
            return;
        };
        self.api.send(ApiCommand::DeleteHost {
            host_id: host.id,
            host_name: host.name,
        });
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.to_lowercase();
        let ctrl_or_cmd = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        if ctrl_or_cmd && key == "k" {
            self.page = Page::Workspace;
            self.search
                .update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
            cx.stop_propagation();
        } else if ctrl_or_cmd && key == "s" && self.files.editor_open {
            self.save_editor_file(cx);
            cx.stop_propagation();
        } else if key == "escape" {
            if self.filter_open {
                self.filter_open = false;
                cx.notify();
                cx.stop_propagation();
            } else if self.settings.host_dropdown_open
                || self.settings.tool_dropdown_open
                || self.settings.ok_dropdown_open
            {
                self.settings.host_dropdown_open = false;
                self.settings.tool_dropdown_open = false;
                self.settings.ok_dropdown_open = false;
                cx.notify();
                cx.stop_propagation();
            } else if self.deleting_host.is_some() {
                self.deleting_host = None;
                cx.notify();
                cx.stop_propagation();
            } else if self
                .host_modal
                .as_ref()
                .is_some_and(|m| m.jump_dropdown_open)
            {
                if let Some(m) = &mut self.host_modal {
                    m.jump_dropdown_open = false;
                }
                cx.notify();
                cx.stop_propagation();
            } else if self.host_modal.is_some() {
                self.host_modal = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.files.inline_prompt.is_some() {
                self.files.inline_prompt = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.files.editing_path {
                self.files.editing_path = false;
                cx.notify();
                cx.stop_propagation();
            }
        }
    }

    // ---------- 事件轮询 ----------

    fn poll(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let alive = this.update(cx, |this, cx| this.tick(cx));
            if alive.is_err() {
                break;
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for event in self.api.try_recv() {
            changed = true;
            match event {
                ApiEvent::Hosts { hosts, inspections } => {
                    self.hosts = hosts;
                    crate::tray::update_hosts(&self.hosts);
                    for (k, v) in inspections {
                        self.inspections.insert(k, v);
                    }
                    self.loading = false;
                }
                ApiEvent::HostsFailed(error) => {
                    self.loading = false;
                    self.toast = Some(Toast {
                        text: format!("读取主机列表失败: {error}"),
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::Inspection {
                    host_id,
                    inspection,
                    host,
                } => {
                    let history = self.net_history.entry(host_id.clone()).or_default();
                    history.push((inspection.net_rx_bps, inspection.net_tx_bps));
                    if history.len() > 20 {
                        history.remove(0);
                    }
                    self.inspection_errors.remove(&host_id);
                    self.inspections.insert(host_id, inspection);
                    if let Some(host) = host {
                        self.replace_host(host);
                        crate::tray::update_hosts(&self.hosts);
                    }
                    self.inspecting_host = None;
                }
                ApiEvent::InspectionFailed { host_id, error } => {
                    self.inspection_errors.insert(host_id, error);
                    self.inspecting_host = None;
                }
                ApiEvent::CheckedAll { hosts, inspections } => {
                    let online = hosts
                        .iter()
                        .filter(|host| !host.archived && host.status == "online")
                        .count();
                    self.hosts = hosts;
                    crate::tray::update_hosts(&self.hosts);
                    for (k, v) in inspections {
                        self.inspections.insert(k, v);
                    }
                    self.checking_all = false;
                    self.toast = Some(Toast {
                        text: format!("机队快照同步完成：{online} 台主机在线"),
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::HostChecked {
                    host_id,
                    host,
                    inspection,
                    error,
                } => {
                    self.checking.remove(&host_id);
                    if let Some(h) = host {
                        self.replace_host(h);
                        crate::tray::update_hosts(&self.hosts);
                    }
                    if let Some(ins) = inspection {
                        self.inspections.insert(host_id, ins);
                    }
                    if let Some(err) = error {
                        self.toast = Some(Toast {
                            text: err,
                            kind: ToastKind::Err,
                            at: Instant::now(),
                        });
                    }
                }
                ApiEvent::HostSaved { host, is_edit } => {
                    let host_id = host.id.clone();
                    let host_name = host.name.clone();
                    self.replace_host(host);
                    if !is_edit {
                        // 新建主机默认置顶到列表第一屏顶部（紧挨 local 本机之后），并同步保存排序
                        let ordered_ids: Vec<String> = self
                            .hosts
                            .iter()
                            .filter(|h| h.kind != "local")
                            .map(|h| h.id.clone())
                            .collect();
                        self.api.send(ApiCommand::Reorder { ordered_ids });
                        // 若被当前筛选条件遮挡，自动清除筛选以便立即可见
                        if !self.filtered_hosts().iter().any(|h| h.id == host_id) {
                            self.reset_filters();
                        }
                    }
                    // 新建或编辑保存主机后，立即触发一次在线状态与 OS 检测
                    self.checking.insert(host_id.clone());
                    self.api.send(ApiCommand::CheckHost(host_id.clone()));
                    crate::tray::update_hosts(&self.hosts);
                    self.host_modal = None;
                    self.detail_host_id = Some(host_id);
                    self.right_wing = Some(RightWing::Detail);
                    self.last_inspect = None;
                    self.toast = Some(Toast {
                        text: if is_edit {
                            format!("已更新主机「{host_name}」")
                        } else {
                            format!("已创建主机「{host_name}」（位于列表顶部，正在检测连接…）")
                        },
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::HostSaveFailed(error) => {
                    if let Some(m) = &mut self.host_modal {
                        m.saving = false;
                        m.error_msg = Some(error.clone());
                    }
                    self.toast = Some(Toast {
                        text: format!("保存失败: {error}"),
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::HostDeleted { host_id, host_name } => {
                    self.hosts.retain(|h| h.id != host_id);
                    crate::tray::update_hosts(&self.hosts);
                    self.close_tab(&host_id, cx);
                    if self.detail_host_id.as_deref() == Some(host_id.as_str()) {
                        self.detail_host_id = None;
                        self.right_wing = None;
                    }
                    self.api.send(ApiCommand::LoadHosts);
                    self.toast = Some(Toast {
                        text: format!("已删除主机「{host_name}」"),
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::HostDeleteFailed(error) => {
                    self.api.send(ApiCommand::LoadHosts);
                    self.toast = Some(Toast {
                        text: format!("删除失败: {error}"),
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::HostUpdated(host) => {
                    self.replace_host(host);
                    crate::tray::update_hosts(&self.hosts);
                }
                ApiEvent::FilesListed {
                    host_id,
                    requested_path,
                    path,
                    parent,
                    items,
                    prefetch_only,
                } => {
                    if let Some(h) = self.hosts.iter_mut().find(|h| h.id == host_id)
                        && h.status != "online"
                    {
                        h.status = "online".into();
                        crate::tray::update_hosts(&self.hosts);
                        self.api.send(ApiCommand::LoadHosts);
                    }
                    self.files.store_cache(
                        &host_id,
                        &requested_path,
                        path.clone(),
                        parent.clone(),
                        items.clone(),
                    );
                    if !prefetch_only && self.files.host_id.as_deref() == Some(host_id.as_str()) {
                        self.files.current_path = path;
                        self.files.parent_path = parent.clone();
                        self.files.items = items;
                        self.files.loading = false;
                        if let Some(p) = parent {
                            self.prefetch_files_dir(&host_id, &p);
                        }
                    }
                }
                ApiEvent::FilesListFailed { host_id, error } => {
                    self.api.send(ApiCommand::LoadHosts);
                    if self.files.host_id.as_deref() == Some(host_id.as_str()) {
                        self.files.loading = false;
                        self.toast = Some(Toast {
                            text: format!("读取目录失败: {error}"),
                            kind: ToastKind::Err,
                            at: Instant::now(),
                        });
                    }
                }
                ApiEvent::FileRead {
                    host_id,
                    path,
                    name,
                    content,
                } => {
                    if self.files.host_id.as_deref() == Some(host_id.as_str())
                        && self.files.editor_path == path
                    {
                        self.files.editor_loading = false;
                        self.files.pending_editor_load = Some((name, content));
                    }
                }
                ApiEvent::FileReadFailed(error) => {
                    self.files.editor_loading = false;
                    self.files.editor_open = false;
                    self.toast = Some(Toast {
                        text: format!("打开文件失败: {error}"),
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::FileWritten {
                    host_id,
                    path: _,
                    name,
                    from_editor,
                } => {
                    if from_editor {
                        self.files.editor_saving = false;
                        let cur = self.files.current_path.clone();
                        self.files.invalidate_cache(&host_id, &cur);
                        self.load_files_dir(&cur, true);
                        self.toast = Some(Toast {
                            text: format!("已保存 {name}"),
                            kind: ToastKind::Ok,
                            at: Instant::now(),
                        });
                    }
                }
                ApiEvent::FileWriteFailed(error) => {
                    self.files.editor_saving = false;
                    self.toast = Some(Toast {
                        text: format!("保存失败: {error}"),
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::FileActionDone {
                    host_id,
                    dir_to_reload,
                    highlight_path,
                    toast,
                } => {
                    self.files.busy_action_path = None;
                    if let Some(hl) = highlight_path {
                        self.files.highlighted = Some((hl, Instant::now()));
                    }
                    self.files.invalidate_cache(&host_id, &dir_to_reload);
                    if self.files.host_id.as_deref() == Some(host_id.as_str()) {
                        self.load_files_dir(&dir_to_reload, true);
                    }
                    self.toast = Some(Toast {
                        text: toast,
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::FileActionFailed { host_id: _, error } => {
                    self.files.busy_action_path = None;
                    self.toast = Some(Toast {
                        text: error,
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::TransfersListed { host_id, tasks } => {
                    if self.files.host_id.as_deref() == Some(host_id.as_str()) {
                        let was_running = self
                            .files
                            .transfers
                            .iter()
                            .any(|t| t.state == "running" && t.direction == "upload");
                        let now_running = tasks
                            .iter()
                            .any(|t| t.state == "running" && t.direction == "upload");
                        self.files.transfers = tasks;
                        // 上传刚结束时自动刷新当前目录
                        if was_running && !now_running {
                            let cur = self.files.current_path.clone();
                            self.files.invalidate_cache(&host_id, &cur);
                            self.load_files_dir(&cur, true);
                        }
                    }
                }
                ApiEvent::TransferStarted {
                    host_id,
                    dir_to_reload,
                    toast,
                } => {
                    self.files.last_transfer_poll = None;
                    self.api.send(ApiCommand::ListTransfers {
                        host_id: host_id.clone(),
                    });
                    if let Some(dir) = dir_to_reload
                        && self.files.host_id.as_deref() == Some(host_id.as_str())
                    {
                        self.files.invalidate_cache(&host_id, &dir);
                    }
                    self.toast = Some(Toast {
                        text: toast,
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::SettingsLoaded(bundle) => {
                    self.settings.apply_bundle(bundle);
                }
                ApiEvent::AutostartSaved {
                    autostart,
                    close_action,
                } => {
                    self.settings.autostart = autostart;
                    self.settings.close_action = close_action;
                    self.toast = Some(Toast {
                        text: "开机自启与后台常驻设置已生效".into(),
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::AutostartSaveFailed {
                    prev_autostart,
                    prev_close_action,
                    error,
                } => {
                    self.settings.autostart = prev_autostart;
                    self.settings.close_action = prev_close_action;
                    self.toast = Some(Toast {
                        text: format!("保存自启与运行设置失败: {error}"),
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::ProxySaved {
                    proxy_mode,
                    proxy_port,
                } => {
                    self.settings.proxy_mode = proxy_mode.clone();
                    self.settings.proxy_port = proxy_port;
                    self.settings.pending_proxy_port_sync = Some(proxy_port.to_string());
                    self.api.send(ApiCommand::LoadHosts);
                    let mode_label = match proxy_mode.as_str() {
                        "off" => "已关闭本机代理路由".to_owned(),
                        "global" => format!("已开启全局默认走本机代理 (127.0.0.1:{proxy_port})"),
                        _ => format!("本机代理已就绪 (127.0.0.1:{proxy_port}，按主机选择)"),
                    };
                    self.toast = Some(Toast {
                        text: mode_label,
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::McpSaved {
                    mcp_enabled,
                    mcp_require_api_key,
                    mcp_api_key,
                    mcp_enabled_tools,
                    regenerated,
                } => {
                    self.settings.mcp_enabled = mcp_enabled;
                    self.settings.mcp_require_api_key = mcp_require_api_key;
                    if !mcp_api_key.is_empty() {
                        self.settings.mcp_api_key = mcp_api_key;
                    }
                    self.settings.mcp_enabled_tools = mcp_enabled_tools;
                    self.toast = Some(Toast {
                        text: if regenerated {
                            "已生成新的 MCP API Key".into()
                        } else {
                            "MCP 设置已保存".into()
                        },
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::McpSaveFailed {
                    prev_enabled,
                    prev_require_key,
                    error,
                } => {
                    self.settings.mcp_enabled = prev_enabled;
                    self.settings.mcp_require_api_key = prev_require_key;
                    self.toast = Some(Toast {
                        text: format!("保存 MCP 设置失败: {error}"),
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::AuthSaved {
                    auth_enabled,
                    admin_username,
                    has_admin_password,
                } => {
                    self.settings.auth_enabled = auth_enabled;
                    self.settings.has_admin_password = has_admin_password;
                    self.settings.pending_username_sync = Some(admin_username);
                    self.toast = Some(Toast {
                        text: "访问安全与登录设置已保存".into(),
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::CliSynced {
                    claude_code_synced,
                    codex_synced,
                } => {
                    self.settings.claude_code_synced = claude_code_synced;
                    self.settings.codex_synced = codex_synced;
                    self.toast = Some(Toast {
                        text: "已同步写入本机 Claude Code 与 Codex 配置文件".into(),
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::GuardSaved => {
                    self.toast = Some(Toast {
                        text: "命令黑名单已保存，立即生效".into(),
                        kind: ToastKind::Ok,
                        at: Instant::now(),
                    });
                }
                ApiEvent::McpCallsLoaded {
                    calls,
                    hosts,
                    tools,
                } => {
                    self.settings.calls = calls;
                    self.settings.call_hosts = hosts;
                    self.settings.call_tools = tools;
                }
                ApiEvent::McpCallDetailLoaded {
                    call_id,
                    args,
                    result,
                } => {
                    if self.settings.expanded_call_id.as_deref() == Some(call_id.as_str()) {
                        self.settings.loading_detail = false;
                        self.settings.expanded_detail = Some((args, result));
                    }
                }
                ApiEvent::UpdateChecked(outcome) => {
                    self.settings.checking_update = false;
                    let toast = if !outcome.ok {
                        (
                            format!(
                                "检查更新失败: {}",
                                outcome.error.as_deref().unwrap_or("未知错误")
                            ),
                            ToastKind::Err,
                        )
                    } else if outcome.has_update {
                        (
                            format!("发现新版本 {}！", outcome.latest_version),
                            ToastKind::Ok,
                        )
                    } else {
                        ("当前已是最新版本".into(), ToastKind::Ok)
                    };
                    self.settings.update_outcome = Some(outcome);
                    self.toast = Some(Toast {
                        text: toast.0,
                        kind: toast.1,
                        at: Instant::now(),
                    });
                }
                ApiEvent::RevealTerminal {
                    host_id,
                    pane,
                    reason,
                } => {
                    self.reveal_terminal(&host_id, pane, reason, cx);
                }
                ApiEvent::ShowMainWindow => {
                    self.request_show_main_window(cx);
                }
                ApiEvent::HostsUpdated => {
                    self.api.send(ApiCommand::LoadHosts);
                }
                ApiEvent::OpenStandaloneTerminal { host_id, .. } => {
                    self.popout_terminal(&host_id, cx);
                }
                ApiEvent::Failed(error) => {
                    self.inspecting_host = None;
                    self.checking_all = false;
                    self.toast = Some(Toast {
                        text: error,
                        kind: ToastKind::Err,
                        at: Instant::now(),
                    });
                }
                ApiEvent::Json(_) => {}
            }
        }

        // 处理系统托盘事件（左键唤醒主窗口 / 右键主机秒开独立终端窗口 / 退出）
        for action in crate::tray::try_recv() {
            changed = true;
            match action {
                crate::tray::TrayAction::ShowMainWindow => {
                    self.request_show_main_window(cx);
                }
                crate::tray::TrayAction::OpenHostTerminal(host_id) => {
                    self.popout_terminal(&host_id, cx);
                }
                crate::tray::TrayAction::Quit => {
                    cx.quit();
                    return;
                }
            }
        }

        // 传输队列有运行中任务时每 350ms 轮询一次
        let files_visible = self.center_stage == Some(CenterStage::Files)
            || self.right_wing == Some(RightWing::Files);
        let has_running_transfer = self.files.transfers.iter().any(|t| t.state == "running");
        if files_visible
            && (has_running_transfer || self.files.last_transfer_poll.is_none())
            && let Some(hid) = self.files.host_id.clone()
        {
            let due = self
                .files
                .last_transfer_poll
                .map(|at| at.elapsed() >= Duration::from_millis(350))
                .unwrap_or(true);
            if due {
                self.files.last_transfer_poll = Some(Instant::now());
                self.api.send(ApiCommand::ListTransfers { host_id: hid });
            }
        }

        // 详情抽屉打开时每 2.8s 采样一次。
        if self.right_wing == Some(RightWing::Detail)
            && let Some(host_id) = self.detail_host_id.clone()
        {
            let due = self
                .last_inspect
                .map(|at| at.elapsed() >= Duration::from_millis(2800))
                .unwrap_or(true);
            if due && self.inspecting_host.is_none() {
                self.last_inspect = Some(Instant::now());
                self.inspecting_host = Some(host_id.clone());
                self.api.send(ApiCommand::Inspect(host_id));
                changed = true;
            }
        }

        // 4s 高亮自动过期
        if let Some((_, at)) = &self.files.highlighted
            && at.elapsed() >= Duration::from_secs(4)
        {
            self.files.highlighted = None;
            changed = true;
        }

        // Toast 2.8s 自动消失。
        if let Some(toast) = &self.toast
            && toast.at.elapsed() >= Duration::from_millis(2800)
        {
            self.toast = None;
            changed = true;
        }

        if changed {
            cx.notify();
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 若有刚读取完的文件内容，在拥有 window 引用时灌入 EditorState
        if let Some((name, content)) = self.files.pending_editor_load.take() {
            let lang = editor_language_for(&name);
            self.files.editor_state.update(cx, |editor, cx| {
                editor.set_value(content, window, cx);
                editor.set_highlighter(lang, cx);
                editor.focus(window, cx);
            });
        }
        if let Some(username) = self.settings.pending_username_sync.take() {
            self.settings
                .admin_username
                .update(cx, |input, cx| input.set_value(username, window, cx));
        }
        if let Some(guard_txt) = self.settings.pending_guard_sync.take() {
            self.settings
                .guard_editor
                .update(cx, |ta, cx| ta.set_value(guard_txt, window, cx));
        }
        if let Some(port_txt) = self.settings.pending_proxy_port_sync.take() {
            self.settings
                .proxy_port_input
                .update(cx, |input, cx| input.set_value(port_txt, window, cx));
        }

        let this = cx.entity();
        let palette = Palette::for_mode(self.is_dark);
        let has_stage = self.has_stage();

        let main = match self.page {
            Page::Settings => div()
                .flex_1()
                .w_full()
                .h_full()
                .min_h_0()
                .overflow_hidden()
                .child(settings_page::render(self, &this, &palette, cx))
                .into_any_element(),
            Page::Workspace => {
                let sidebar = if has_stage {
                    if self.show_sidebar_in_stage {
                        div()
                            .w(px(336.))
                            .flex_shrink_0()
                            .h_full()
                            .min_h_0()
                            .child(host_card::render_list(self, &this, &palette, cx))
                            .into_any_element()
                    } else {
                        div().w(px(0.)).into_any_element()
                    }
                } else {
                    div()
                        .flex_1()
                        .min_w_0()
                        .max_w(px(1152.))
                        .h_full()
                        .min_h_0()
                        .child(host_card::render_list(self, &this, &palette, cx))
                        .into_any_element()
                };

                let center = if has_stage {
                    let stage_el = match self.center_stage {
                        Some(CenterStage::Terminal) => {
                            terminal_stage::render(self, &this, &palette, cx)
                        }
                        Some(CenterStage::Files) => {
                            file_browser::render(self, &this, false, &palette, cx)
                        }
                        None => div().into_any_element(),
                    };
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .min_h_0()
                        .child(stage_el)
                        .into_any_element()
                } else {
                    div().w(px(0.)).into_any_element()
                };

                let right_visible = self.right_wing.is_some() && self.right_wing_host().is_some();
                let right = if right_visible {
                    let wing_el = match self.right_wing {
                        Some(RightWing::Detail) => {
                            detail_drawer::render(self, &this, &palette, cx).into_any_element()
                        }
                        Some(RightWing::Files) => {
                            file_browser::render(self, &this, true, &palette, cx)
                        }
                        None => div().into_any_element(),
                    };
                    div()
                        .w(px(336.))
                        .flex_shrink_0()
                        .h_full()
                        .min_h_0()
                        .child(wing_el)
                        .into_any_element()
                } else {
                    div().w(px(0.)).into_any_element()
                };

                let mut row = div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .items_stretch()
                    .justify_center()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(16.));
                if !has_stage || self.show_sidebar_in_stage {
                    row = row.child(sidebar);
                }
                if has_stage {
                    row = row.child(center);
                }
                if right_visible {
                    row = row.child(right);
                }
                row.into_any_element()
            }
        };

        let this_dismiss_filter = this.clone();
        let mut root = div()
            .id("workspace-root")
            .test_support()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                let _ = this_dismiss_filter.update(cx, |w, cx| {
                    let mut closed = false;
                    if w.filter_open {
                        w.filter_open = false;
                        closed = true;
                    }
                    if w.settings.host_dropdown_open
                        || w.settings.tool_dropdown_open
                        || w.settings.ok_dropdown_open
                    {
                        w.settings.host_dropdown_open = false;
                        w.settings.tool_dropdown_open = false;
                        w.settings.ok_dropdown_open = false;
                        closed = true;
                    }
                    if closed {
                        cx.notify();
                    }
                });
            })
            .bg(palette.page_background())
            .text_color(palette.text_primary)
            .child(top_bar::render(self, &this, &palette, cx))
            .child(main);

        if self.page == Page::Workspace && self.filter_open {
            root = root.child(top_bar::render_filter_overlay(self, &this, &palette));
        }

        if self.host_modal.is_some() {
            root = root.child(render_host_modal(self, &this, &palette, cx));
        }

        if self.deleting_host.is_some() {
            root = root.child(render_delete_confirm(self, &this, &palette, cx));
        }

        if let Some(toast) = &self.toast {
            let (bg, fg, border) = match toast.kind {
                ToastKind::Ok => (palette.primary_bg, palette.primary_fg, palette.border),
                ToastKind::Err => (
                    palette.card_solid_bg,
                    Hsla::from(rgb(0xdc2626)),
                    Hsla::from(rgb(0xfecaca)),
                ),
            };
            root = root.child(
                div()
                    .id("workspace-toast")
                    .test_support()
                    .absolute()
                    .bottom(px(20.))
                    .right(px(20.))
                    .rounded_xl()
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .text_color(fg)
                    .px(px(14.))
                    .py(px(8.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .shadow_lg()
                    .child(toast.text.clone()),
            );
        }

        root
    }
}

