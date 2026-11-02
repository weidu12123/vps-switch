//! 板块一（VPS 仓库）+ 板块二（终端）+ 板块三（文件浏览器）无头 UI 集成测试。

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::Root;
use gpui_kit::{
    App, AppContext as _, ElementId, Entity, InputEvent as _, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, SharedString,
    TestAppContext, TouchPhase, Window, WindowHandle, point, px, size,
};

use vps_switch_gpui::api::{Api, Host, HostLink};
use vps_switch_gpui::terminal::PaneMeta;
use vps_switch_gpui::theme;
use vps_switch_gpui::workspace::{CenterStage, RightWing, StatusFilter, Workspace};

fn make_host(
    id: &str,
    name: &str,
    kind: &str,
    status: &str,
    distro: &str,
    role: Option<&str>,
    favorited: bool,
) -> Host {
    Host {
        id: id.into(),
        name: name.into(),
        addr: if kind == "local" {
            "127.0.0.1".into()
        } else {
            "203.0.113.10".into()
        },
        port: 22,
        username: "root".into(),
        kind: kind.into(),
        auth_type: "password".into(),
        has_password: true,
        has_private_key: false,
        has_passphrase: false,
        jump_host_id: None,
        jump_host_name: None,
        status: status.into(),
        os: Some(if distro == "windows" {
            "windows".into()
        } else {
            "linux".into()
        }),
        pretty_name: Some(format!("{distro} server")),
        distro_id: Some(distro.into()),
        kernel: Some("6.8.0-generic".into()),
        arch: Some("x86_64".into()),
        cpu_cores: Some(4),
        cpu_model: Some("AMD EPYC".into()),
        pinned: false,
        favorited,
        archived: false,
        mcp_exposed: true,
        sort_order: match id {
            "local" => 0,
            "vip-hk" => 1,
            "us-node" => 2,
            _ => 10,
        },
        role: role.map(str::to_owned),
        tags: vec!["hk".into(), "bgp".into()],
        links: vec![HostLink {
            id: "l1".into(),
            name: "商家面板".into(),
            url: "https://example.com".into(),
            description: String::new(),
        }],
        latency_ms: Some(31),
        last_error: None,
    }
}

fn seed_hosts() -> Vec<Host> {
    vec![
        make_host("local", "本机", "local", "online", "windows", None, false),
        make_host(
            "vip-hk",
            "vip-hk",
            "ssh",
            "online",
            "debian",
            Some("primary"),
            true,
        ),
        make_host(
            "us-node",
            "us-node",
            "ssh",
            "offline",
            "ubuntu",
            Some("proxy"),
            false,
        ),
    ]
}

fn render_frame(window: &mut Window, cx: &mut App) {
    window.refresh();
    window.draw(cx).clear(cx);
}

fn has_element(window: &Window, id: &str) -> bool {
    let element_id = ElementId::Name(SharedString::from(id));
    gpui_base::test_support::find(window, &[], &element_id).is_some()
}

fn element_bounds(window: &Window, id: &str) -> gpui_kit::Bounds<Pixels> {
    let element_id = ElementId::Name(SharedString::from(id));
    gpui_base::test_support::find(window, &[], &element_id)
        .unwrap_or_else(|| panic!("element {id} not found in the last frame"))
        .bounds()
}

fn element_center(window: &Window, id: &str) -> Point<Pixels> {
    element_bounds(window, id).center()
}

fn click_element(window: &mut Window, id: &str, count: usize, cx: &mut App) {
    render_frame(window, cx);
    let position = element_center(window, id);
    window.dispatch_event(
        MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Default::default(),
        }
        .to_platform_input(),
        cx,
    );
    for click_count in 1..=count {
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count,
                first_mouse: false,
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count,
            }
            .to_platform_input(),
            cx,
        );
    }
    render_frame(window, cx);
}

fn open_workspace(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Workspace>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        theme::install(cx);
    });
    let holder: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
    let holder_clone = holder.clone();
    let handle = cx.open_window(size(px(1240.), px(780.)), |window, cx| {
        let ws = cx.new(|cx| Workspace::new(Api::disconnected(), window, cx));
        ws.update(cx, |w, cx| w.set_hosts_for_test(seed_hosts(), cx));
        *holder_clone.borrow_mut() = Some(ws.clone());
        Root::new(ws, window, cx)
    });
    let ws = holder.borrow().clone().unwrap();
    (handle, ws)
}

#[gpui_kit::test]
fn renders_host_cards_and_official_os_badges(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        render_frame(window, cx);
        assert!(has_element(window, "host-card-local"));
        assert!(has_element(window, "host-card-vip-hk"));
        assert!(has_element(window, "host-card-us-node"));
        assert!(has_element(window, "os-badge-local"));
        assert!(has_element(window, "os-badge-vip-hk"));
        assert!(has_element(window, "os-badge-us-node"));

        // 初始顺序：local -> vip-hk -> us-node
        let y_local = element_bounds(window, "host-card-local").origin.y;
        let y_vip = element_bounds(window, "host-card-vip-hk").origin.y;
        let y_us = element_bounds(window, "host-card-us-node").origin.y;
        assert!(y_local < y_vip && y_vip < y_us);

        // 点击 us-node 置顶按钮：立即移到 vip-hk 之上（紧随 local 之后）
        click_element(window, "pin-us-node", 1, cx);
        assert!(ws.read(cx).host_by_id("us-node").unwrap().pinned);
        let y_local_after = element_bounds(window, "host-card-local").origin.y;
        let y_us_after = element_bounds(window, "host-card-us-node").origin.y;
        let y_vip_after = element_bounds(window, "host-card-vip-hk").origin.y;
        assert!(y_local_after < y_us_after && y_us_after < y_vip_after);

        // 再次点击取消置顶：恢复到 vip-hk 之下
        click_element(window, "pin-us-node", 1, cx);
        assert!(!ws.read(cx).host_by_id("us-node").unwrap().pinned);
        let y_vip_restored = element_bounds(window, "host-card-vip-hk").origin.y;
        let y_us_restored = element_bounds(window, "host-card-us-node").origin.y;
        assert!(y_vip_restored < y_us_restored);
    })
    .unwrap();
}

#[gpui_kit::test]
fn clicking_card_toggles_detail_drawer_and_delete_confirm(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        assert!(ws.read(cx).right_wing.is_none());
        click_element(window, "host-card-vip-hk", 1, cx);
        assert_eq!(ws.read(cx).right_wing, Some(RightWing::Detail));
        assert!(has_element(window, "detail-drawer"));
        assert!(has_element(window, "drawer-delete"));

        click_element(window, "drawer-delete", 1, cx);
        assert!(has_element(window, "delete-modal-card"));
        click_element(window, "delete-modal-cancel", 1, cx);
        assert!(!has_element(window, "delete-modal-card"));

        click_element(window, "host-card-vip-hk", 1, cx);
        assert!(ws.read(cx).right_wing.is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn filter_dropdown_and_modals_work_end_to_end(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        click_element(window, "nav-filter", 1, cx);
        assert!(has_element(window, "filter-dropdown-panel"));

        // 再次点击筛选按钮：必须直接关闭（不能先被 workspace-root 关掉又被 on_click 重新打开）
        click_element(window, "nav-filter", 1, cx);
        assert!(!has_element(window, "filter-dropdown-panel"));
        assert!(!ws.read(cx).filter_open);

        // 重新展开筛选面板测试过滤与重置
        click_element(window, "nav-filter", 1, cx);
        assert!(has_element(window, "filter-dropdown-panel"));

        click_element(window, "filter-status-online", 1, cx);
        assert_eq!(ws.read(cx).status_filter, StatusFilter::Online);
        assert!(has_element(window, "host-card-vip-hk"));
        assert!(!has_element(window, "host-card-us-node"));

        click_element(window, "filter-reset", 1, cx);
        assert_eq!(ws.read(cx).status_filter, StatusFilter::All);
        assert!(has_element(window, "host-card-us-node"));

        click_element(window, "nav-add", 1, cx);
        assert!(has_element(window, "host-modal-card"));
        click_element(window, "modal-save", 1, cx);
        assert!(has_element(window, "modal-error"));
        click_element(window, "modal-cancel", 1, cx);
        assert!(!has_element(window, "host-modal-card"));

        click_element(window, "edit-vip-hk", 1, cx);
        assert!(has_element(window, "host-modal-card"));
        assert_eq!(
            ws.read(cx)
                .host_modal
                .as_ref()
                .and_then(|m| m.editing_id.as_deref()),
            Some("vip-hk")
        );

        // 点击跳板机选择框：展开下拉列表（含「本机代理」与各 SSH 候选），点选后立即选中并收起
        assert!(!has_element(window, "modal-jump-dropdown"));
        click_element(window, "modal-jump-cycle", 1, cx);
        assert!(has_element(window, "modal-jump-dropdown"));
        assert!(has_element(window, "modal-jump-opt-1"));
        assert!(has_element(window, "modal-jump-opt-2"));
        click_element(window, "modal-jump-opt-1", 1, cx);
        assert!(!has_element(window, "modal-jump-dropdown"));
        assert_eq!(
            ws.read(cx)
                .host_modal
                .as_ref()
                .map(|m| m.jump_host_id.as_str()),
            Some("local-proxy")
        );

        click_element(window, "modal-jump-cycle", 1, cx);
        click_element(window, "modal-jump-opt-2", 1, cx);
        assert_eq!(
            ws.read(cx)
                .host_modal
                .as_ref()
                .map(|m| m.jump_host_id.as_str()),
            Some("us-node")
        );

        // 认证方式支持切换为「免凭据 (none)」
        assert!(has_element(window, "modal-auth-none"));
        click_element(window, "modal-auth-none", 1, cx);
        assert_eq!(
            ws.read(cx)
                .host_modal
                .as_ref()
                .map(|m| m.auth_type.as_str()),
            Some("none")
        );

        // 主机编辑弹窗底部「允许 MCP 访问」复选框默认勾选，点击可切换
        assert!(has_element(window, "modal-check-mcp-exposed"));
        assert!(
            ws.read(cx)
                .host_modal
                .as_ref()
                .is_some_and(|m| m.mcp_exposed)
        );
        click_element(window, "modal-check-mcp-exposed", 1, cx);
        assert!(
            ws.read(cx)
                .host_modal
                .as_ref()
                .is_some_and(|m| !m.mcp_exposed)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn terminal_stage_opens_splits_toggles_dark_and_collapses(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        click_element(window, "host-card-vip-hk", 2, cx);
        assert_eq!(ws.read(cx).center_stage, Some(CenterStage::Terminal));
        assert!(has_element(window, "terminal-stage"));
        assert!(has_element(window, "stage-cwd-pill"));
        assert!(has_element(window, "stage-exit-code-badge"));
        assert!(has_element(window, "pane-cell-0"));
        assert!(!has_element(window, "pane-cell-1"));

        click_element(window, "split-row", 1, cx);
        assert_eq!(ws.read(cx).open_tabs[0].panes.len(), 2);
        assert!(has_element(window, "pane-cell-1"));
        assert!(has_element(window, "close-pane-1"));

        assert!(!ws.read(cx).terminal_dark);
        click_element(window, "toggle-terminal-dark", 1, cx);
        assert!(ws.read(cx).terminal_dark);

        click_element(window, "close-pane-1", 1, cx);
        assert_eq!(ws.read(cx).open_tabs[0].panes.len(), 1);
        assert!(!has_element(window, "pane-cell-1"));

        // 点击舞台顶栏「刷新」按钮，原地重置当前聚焦 pane
        assert!(has_element(window, "refresh-terminal"));
        assert_eq!(ws.read(cx).open_tabs[0].panes[0].read(cx).refresh_count, 0);
        click_element(window, "refresh-terminal", 1, cx);
        assert_eq!(ws.read(cx).open_tabs[0].panes[0].read(cx).refresh_count, 1);
        assert_eq!(
            ws.read(cx).open_tabs[0].panes[0].read(cx).pane_id.as_deref(),
            Some("mock-pane-2")
        );

        click_element(window, "collapse-stage", 1, cx);
        assert!(ws.read(cx).center_stage.is_none());
        assert!(has_element(window, "session-vip-hk"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn file_browser_renders_breadcrumbs_entries_inline_prompt_and_editor(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        // 点击卡片「文件」按钮进入全宽文件舞台（命中预置缓存 /root）
        click_element(window, "open-files-vip-hk", 1, cx);
        assert_eq!(ws.read(cx).center_stage, Some(CenterStage::Files));
        assert!(has_element(window, "file-browser-panel"));
        assert!(has_element(window, "crumb-0"));
        assert!(has_element(window, "crumb-1"));
        assert!(has_element(window, "file-row-parent"));
        assert!(has_element(window, "file-row-app"));
        assert!(has_element(window, "file-row-nginx.conf"));
        assert!(has_element(window, "file-row-backup.tar.gz"));
        assert!(has_element(window, "file-extract-2"));

        // 点击「+目录」展开行内输入条，再点「取消」收起
        click_element(window, "files-btn-mkdir", 1, cx);
        assert!(has_element(window, "files-inline-prompt"));
        click_element(window, "files-prompt-cancel", 1, cx);
        assert!(!has_element(window, "files-inline-prompt"));

        // 点击文件打开右侧在线代码编辑器
        click_element(window, "file-row-nginx.conf", 1, cx);
        assert!(ws.read(cx).files.editor_open);
        assert!(has_element(window, "files-editor-pane"));
        assert!(has_element(window, "editor-save-btn"));

        // 关闭编辑器
        click_element(window, "editor-close-btn", 1, cx);
        assert!(!ws.read(cx).files.editor_open);
        assert!(!has_element(window, "files-editor-pane"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn settings_page_switches_all_five_tabs_and_expands_call_log(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        // 点击顶栏设置圆钮进入设置页（默认「常规与自启」）
        click_element(window, "nav-settings", 1, cx);
        assert!(has_element(window, "settings-panel-general"));
        assert!(has_element(window, "settings-toggle-autostart"));
        assert!(has_element(window, "settings-proxy-off"));
        assert!(has_element(window, "settings-proxy-per-host"));
        assert!(has_element(window, "settings-proxy-global"));
        assert!(has_element(window, "settings-proxy-save"));
        click_element(window, "settings-proxy-global", 1, cx);
        assert_eq!(ws.read(cx).settings.proxy_mode, "global");

        // 切换到「访问安全」
        click_element(window, "settings-tab-security", 1, cx);
        assert!(has_element(window, "settings-panel-security"));
        assert!(has_element(window, "settings-save-auth"));

        // 切换到「MCP Server」
        click_element(window, "settings-tab-mcp", 1, cx);
        assert!(has_element(window, "settings-panel-mcp"));
        assert!(has_element(window, "settings-toggle-mcp"));
        assert!(has_element(window, "settings-mcp-expose-card"));
        assert!(has_element(window, "settings-mcp-host-vip-hk"));
        assert!(ws.read(cx).host_by_id("vip-hk").unwrap().mcp_exposed);
        click_element(window, "settings-mcp-host-vip-hk", 1, cx);
        assert!(!ws.read(cx).host_by_id("vip-hk").unwrap().mcp_exposed);
        click_element(window, "settings-mcp-expose-none", 1, cx);
        assert!(ws.read(cx).hosts.iter().all(|h| !h.mcp_exposed));
        click_element(window, "settings-mcp-expose-all", 1, cx);
        assert!(ws.read(cx).hosts.iter().all(|h| h.mcp_exposed));

        // 7 大 MCP 工具开放范围：默认全开（7 个），支持单项切换、全不选与全选
        assert!(has_element(window, "settings-mcp-tools-card"));
        assert!(has_element(window, "settings-mcp-tool-vps_exec"));
        assert_eq!(ws.read(cx).settings.mcp_enabled_tools.len(), 7);
        click_element(window, "settings-mcp-tool-vps_exec", 1, cx);
        assert!(
            !ws.read(cx)
                .settings
                .mcp_enabled_tools
                .iter()
                .any(|t| t == "vps_exec")
        );
        click_element(window, "settings-mcp-tools-none", 1, cx);
        assert!(ws.read(cx).settings.mcp_enabled_tools.is_empty());
        click_element(window, "settings-mcp-tools-all", 1, cx);
        assert_eq!(ws.read(cx).settings.mcp_enabled_tools.len(), 7);
        assert!(has_element(window, "settings-save-guard"));

        // 切换到「调用记录」：未展开时多行参数、空摘要与单行记录的行高必须完全一致
        click_element(window, "settings-tab-calls", 1, cx);
        assert!(has_element(window, "settings-panel-calls"));
        assert!(has_element(window, "mcp-call-row-call-1"));
        assert!(has_element(window, "mcp-call-row-call-2"));
        assert!(has_element(window, "mcp-call-row-call-3"));
        let h1 = element_bounds(window, "mcp-call-row-call-1").size.height;
        let h2 = element_bounds(window, "mcp-call-row-call-2").size.height;
        let h3 = element_bounds(window, "mcp-call-row-call-3").size.height;
        assert_eq!(h1, px(48.));
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);

        // 点击展开第一条记录
        click_element(window, "mcp-call-row-call-1", 1, cx);
        assert_eq!(
            ws.read(cx).settings.expanded_call_id.as_deref(),
            Some("call-1")
        );

        // 切换到「关于与更新」
        click_element(window, "settings-tab-about", 1, cx);
        assert!(has_element(window, "settings-panel-about"));
        assert!(has_element(window, "settings-check-update"));

        // 点击「返回仓库」回到工作台
        click_element(window, "settings-back-home", 1, cx);
        assert!(has_element(window, "host-card-vip-hk"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn terminal_right_click_opens_context_menu_and_clears_screen(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        click_element(window, "host-card-vip-hk", 2, cx);
        assert!(has_element(window, "terminal-stage"));
        assert!(!has_element(window, "terminal-context-menu"));

        // 确认初始 mock 快照首格有字符 'r' ("root@vip-hk:~# ")
        let pane = ws.read(cx).open_tabs[0].panes[0].clone();
        assert_eq!(pane.read(cx).snapshot.as_ref().unwrap().cells[0].c, 'r');

        // 在终端区域右键按下，弹出终端右键菜单
        let position = element_center(window, "pane-cell-0");
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Right,
                position,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }
            .to_platform_input(),
            cx,
        );
        render_frame(window, cx);

        assert!(has_element(window, "terminal-context-menu"));
        assert!(has_element(window, "terminal-ctx-copy"));
        assert!(has_element(window, "terminal-ctx-paste"));
        assert!(has_element(window, "terminal-ctx-clear"));
        assert!(has_element(window, "terminal-ctx-reconnect"));

        // 点击「清屏」菜单项：菜单收起，终端屏幕清空且光标归位
        click_element(window, "terminal-ctx-clear", 1, cx);
        assert!(!has_element(window, "terminal-context-menu"));
        let snap = pane.read(cx).snapshot.as_ref().unwrap();
        assert_eq!(snap.cells[0].c, ' ');
        assert_eq!(snap.cursor, Some((0, 0)));
    })
    .unwrap();
}

#[gpui_kit::test]
fn sse_reveal_terminal_event_switches_to_terminal_stage_and_shows_toast(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        assert!(ws.read(cx).center_stage.is_none());

        ws.update(cx, |w, cx| {
            w.reveal_terminal(
                "vip-hk",
                PaneMeta {
                    id: "agent-pane-99".into(),
                    host_id: "vip-hk".into(),
                    cols: 100,
                    rows: 30,
                    cwd: Some("/etc/nginx".into()),
                    title: None,
                    running: true,
                    last_exit_code: None,
                    bracketed_paste: true,
                    headless: false,
                    exited: false,
                },
                Some("请检查 nginx 配置".into()),
                cx,
            );
        });
        render_frame(window, cx);

        assert_eq!(ws.read(cx).center_stage, Some(CenterStage::Terminal));
        assert_eq!(ws.read(cx).stage_host_id.as_deref(), Some("vip-hk"));
        assert!(has_element(window, "terminal-stage"));
        assert!(has_element(window, "stage-running-badge"));
        assert!(has_element(window, "workspace-toast"));
        assert_eq!(
            ws.read(cx).toast.as_ref().map(|t| t.text.as_str()),
            Some("Agent 唤起「vip-hk」终端: 请检查 nginx 配置")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn nested_scroll_containers_scroll_independently_without_moving_outer_page(cx: &mut TestAppContext) {
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        let mut many_hosts = vec![make_host(
            "local", "本机", "local", "online", "windows", None, false,
        )];
        for i in 1..=30 {
            let id = format!("node-{i}");
            many_hosts.push(make_host(
                &id,
                &id,
                "ssh",
                "online",
                "debian",
                Some("primary"),
                false,
            ));
        }
        ws.update(cx, |w, cx| {
            w.set_hosts_for_test(many_hosts, cx);
            w.settings.call_hosts = (1..=30).map(|i| format!("node-{i}")).collect();
        });
        render_frame(window, cx);

        // 1. 主机弹窗跳板机下拉列表：30 台节点时高度受限且滚轮只滚动下拉列表内部，不动外层弹窗和底层仓库
        click_element(window, "edit-node-1", 1, cx);
        click_element(window, "modal-jump-cycle", 1, cx);
        let jump_bounds = element_bounds(window, "modal-jump-dropdown");
        let opt0_before = element_bounds(window, "modal-jump-opt-0").origin.y;
        let trigger_before = element_bounds(window, "modal-jump-cycle").origin.y;
        let bg_card_before = element_bounds(window, "host-card-local").origin.y;

        let pos = jump_bounds.center();
        window.dispatch_event(
            MouseMoveEvent {
                position: pos,
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            ScrollWheelEvent {
                position: pos,
                delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
                modifiers: Default::default(),
                touch_phase: TouchPhase::Moved,
            }
            .to_platform_input(),
            cx,
        );
        render_frame(window, cx);

        let opt0_after = element_bounds(window, "modal-jump-opt-0").origin.y;
        let trigger_after = element_bounds(window, "modal-jump-cycle").origin.y;
        let bg_card_after = element_bounds(window, "host-card-local").origin.y;
        assert!(
            opt0_after < opt0_before,
            "inner jump dropdown must scroll up"
        );
        assert_eq!(
            trigger_after, trigger_before,
            "outer modal body must not scroll"
        );
        assert_eq!(
            bg_card_after, bg_card_before,
            "background host list must not scroll"
        );
        click_element(window, "modal-cancel", 1, cx);

        // 2. 设置 → MCP Server 的「MCP 暴露主机范围」：滚轮只滚内部主机网格，不动外层设置页
        click_element(window, "nav-settings", 1, cx);
        click_element(window, "settings-tab-mcp", 1, cx);
        let expose_bounds = element_bounds(window, "settings-mcp-expose-scroll");
        let host0_before = element_bounds(window, "settings-mcp-host-local").origin.y;
        let card_before = element_bounds(window, "settings-mcp-expose-card").origin.y;

        let pos2 = expose_bounds.center();
        window.dispatch_event(
            MouseMoveEvent {
                position: pos2,
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            ScrollWheelEvent {
                position: pos2,
                delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
                modifiers: Default::default(),
                touch_phase: TouchPhase::Moved,
            }
            .to_platform_input(),
            cx,
        );
        render_frame(window, cx);

        let host0_after = element_bounds(window, "settings-mcp-host-local").origin.y;
        let card_after = element_bounds(window, "settings-mcp-expose-card").origin.y;
        assert!(
            host0_after < host0_before,
            "inner mcp expose grid must scroll up"
        );
        assert_eq!(
            card_after, card_before,
            "outer settings page must not scroll"
        );

        // 3. 设置 → 调用记录 的「主机」筛选下拉面板：滚轮只滚内部芯片区，不动外层设置页
        click_element(window, "settings-tab-calls", 1, cx);
        click_element(window, "calls-filter-host", 1, cx);
        let calls_drop_bounds = element_bounds(window, "calls-host-dropdown");
        let calls_trigger_before = element_bounds(window, "calls-filter-host").origin.y;
        let pos3 = calls_drop_bounds.center();
        window.dispatch_event(
            MouseMoveEvent {
                position: pos3,
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            ScrollWheelEvent {
                position: pos3,
                delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
                modifiers: Default::default(),
                touch_phase: TouchPhase::Moved,
            }
            .to_platform_input(),
            cx,
        );
        render_frame(window, cx);
        let calls_trigger_after = element_bounds(window, "calls-filter-host").origin.y;
        assert_eq!(
            calls_trigger_after, calls_trigger_before,
            "outer calls settings page must not scroll when scrolling inside calls host dropdown"
        );
    })
    .unwrap();
}


