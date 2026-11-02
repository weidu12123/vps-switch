#![cfg(windows)]

use crate::{api::AppState, terminal::UiEvent};
use anyhow::Result;
use std::{collections::HashMap, sync::Arc};
use tao::{
    dpi::{LogicalSize, Size},
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::{Icon as WindowIcon, Window, WindowBuilder, WindowId},
};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon as TrayIconImage, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use wry::WebViewBuilder;

#[derive(Debug, Clone)]
pub enum DesktopUserEvent {
    TrayIcon(TrayIconEvent),
    TrayMenu(MenuEvent),
    ShowWindow,
    OpenStandaloneTerminal { host_id: String },
}

struct StandaloneTerminal {
    window: Window,
    #[allow(dead_code)]
    webview: wry::WebView,
    host_id: String,
}

const MENU_ID_SHOW: &str = "app:show";
const MENU_ID_QUIT: &str = "app:quit";
const MENU_HOST_PREFIX: &str = "host:";

/// 纯代码光栅化生成 64x64 高清抗锯齿 VPS Switch 纯黑白拓扑互锁图标
pub fn render_brand_icon_rgba(size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let s = size as f32;

    for y in 0..size {
        for x in 0..size {
            let fx = (x as f32 + 0.5) / s * 64.0;
            let fy = (y as f32 + 0.5) / s * 64.0;

            // 1. 纯黑圆角方块基底 (64x64, rx=14)
            let bg_dist = sd_rounded_box(fx - 32.0, fy - 32.0, 29.0, 29.0, 14.0);
            if bg_dist > 1.0 {
                continue;
            }
            let bg_alpha = (0.5 - bg_dist).clamp(0.0, 1.0);

            let mut r = 0.0;
            let mut g = 0.0;
            let mut b = 0.0;

            // 外围 1px 微弱白光边框，在深色任务栏上保持轮廓分明
            if bg_dist > -1.2 && bg_dist < 0.2 {
                let border_w = (1.0 - (bg_dist + 0.5).abs()).clamp(0.0, 1.0) * 0.22;
                r += 255.0 * border_w;
                g += 255.0 * border_w;
                b += 255.0 * border_w;
            }

            // 2. 左上实心 L 型滑块 (M16 18 H38 V26 H24 V40 H16 Z)
            let d_b1_horiz = sd_rounded_box(fx - 27.0, fy - 22.0, 11.0, 4.0, 1.5);
            let d_b1_vert = sd_rounded_box(fx - 20.0, fy - 29.0, 4.0, 11.0, 1.5);
            let d_b1 = d_b1_horiz.min(d_b1_vert);

            // 3. 右下实心 L 型滑块 (M48 46 H26 V38 H40 V24 H48 Z)
            let d_b2_horiz = sd_rounded_box(fx - 37.0, fy - 42.0, 11.0, 4.0, 1.5);
            let d_b2_vert = sd_rounded_box(fx - 44.0, fy - 35.0, 4.0, 11.0, 1.5);
            let d_b2 = d_b2_horiz.min(d_b2_vert);

            // 4. 中央穿透的瞬切光核 (cx=32, cy=32, r=3.5)
            let d_dot = ((fx - 32.0).powi(2) + (fy - 32.0).powi(2)).sqrt() - 3.5;

            // 综合计算纯白图形距离场
            let d_glyph = d_b1.min(d_b2).min(d_dot);
            if d_glyph < 0.8 {
                let w = (0.5 - d_glyph).clamp(0.0, 1.0);
                r = r * (1.0 - w) + 255.0 * w;
                g = g * (1.0 - w) + 255.0 * w;
                b = b * (1.0 - w) + 255.0 * w;
            }

            let idx = ((y * size + x) * 4) as usize;
            buf[idx] = r.min(255.0) as u8;
            buf[idx + 1] = g.min(255.0) as u8;
            buf[idx + 2] = b.min(255.0) as u8;
            buf[idx + 3] = (bg_alpha * 255.0) as u8;
        }
    }
    buf
}

fn sd_rounded_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let qx = px.abs() - bx + r;
    let qy = py.abs() - by + r;
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

fn build_tray_menu(state: &Arc<AppState>) -> Result<Menu> {
    let menu = Menu::new();
    let show_item = MenuItem::with_id(MENU_ID_SHOW, "打开 VPS Switch 主界面", true, None);
    menu.append(&show_item)?;
    menu.append(&PredefinedMenuItem::separator())?;

    if let Ok(hosts) = state.db.list_public_hosts() {
        let mut active: Vec<_> = hosts.into_iter().filter(|h| !h.archived).collect();
        // 优先排：本机 -> 收藏 -> 置顶 -> 在线
        active.sort_by(|a, b| {
            (a.kind != "local")
                .cmp(&(b.kind != "local"))
                .then_with(|| b.favorited.cmp(&a.favorited))
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| (a.status != "online").cmp(&(b.status != "online")))
        });

        for h in active.into_iter().take(28) {
            let prefix = if h.favorited {
                "★ "
            } else if h.status == "online" {
                "● "
            } else {
                "○ "
            };
            let label = if h.kind == "local" {
                format!("{prefix}{} (PowerShell)", h.name)
            } else {
                format!("{prefix}{} ({})", h.name, h.addr)
            };
            let item = MenuItem::with_id(
                format!("{MENU_HOST_PREFIX}{}", h.id),
                label,
                true,
                None,
            );
            let _ = menu.append(&item);
        }
        menu.append(&PredefinedMenuItem::separator())?;
    }

    let quit_item = MenuItem::with_id(MENU_ID_QUIT, "退出 VPS Switch", true, None);
    menu.append(&quit_item)?;
    Ok(menu)
}

/// 把 WebView2 切到低内存档 / 还原。
///
/// 低内存档（`COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW`）是 WebView2 官方给"不显示时省内存"
/// 用的接口：渲染进程会主动收缩缓存与工作集，代价是重绘变慢——所以只该在窗口隐藏时用，
/// 重新显示前必须还原。比"把 WebView 拆了重建"好得多：页面不重载，标签页与终端现场都还在。
///
/// 老版本 WebView2 Runtime 没有这个接口，`cast` 会失败，此时静默跳过（不降级、不报错）。
#[cfg(windows)]
fn set_webview_memory_level(webview: &wry::WebView, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows_core::Interface as _;
    use wry::WebViewExtWindows;

    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else {
        return;
    };
    let Ok(core19) = core.cast::<ICoreWebView2_19>() else {
        return;
    };
    let level = if low {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
    } else {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
    };
    let _ = unsafe { core19.SetMemoryUsageTargetLevel(level) };
}

/// 主窗口 WebView 的创建。刻意不在启动时无条件调用：一个隐藏窗口同样会把整套 Chromium
/// 拉起来（实测约 145 MB），而开机自启的常态就是"躺在托盘里没人点"。
fn build_main_webview(window: &Window, url: &str) -> Option<wry::WebView> {
    match WebViewBuilder::new().with_url(url).build(window) {
        Ok(wv) => Some(wv),
        Err(e) => {
            tracing::error!("创建主窗口 WebView 失败: {e}");
            None
        }
    }
}

/// 显示主窗口：没建过 WebView 就建，建过就还原内存档再显示。
fn show_main_window(window: &Window, webview: &mut Option<wry::WebView>, url: &str) {
    if webview.is_none() {
        *webview = build_main_webview(window, url);
    }
    if let Some(wv) = webview.as_ref() {
        set_webview_memory_level(wv, false);
        let _ = wv.set_visible(true);
    }
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
}

/// 最小化到托盘：WebView 留着（页面不重载，标签页与终端现场都保住），但降到低内存档。
fn hide_main_window(window: &Window, webview: &Option<wry::WebView>) {
    if let Some(wv) = webview.as_ref() {
        let _ = wv.set_visible(false);
        set_webview_memory_level(wv, true);
    }
    window.set_visible(false);
}

pub fn run_desktop_window(
    state: Arc<AppState>,
    rt_handle: tokio::runtime::Handle,
    port: u16,
) -> Result<()> {
    let event_loop = EventLoopBuilder::<DesktopUserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // 注册外部单实例唤醒回调（当用户第二次双击 vps-switch.exe 时直接唤醒当前窗口）
    {
        let proxy_show = proxy.clone();
        let mut rx = state.terminals.subscribe_ui_events();
        rt_handle.spawn(async move {
            while let Ok(ev) = rx.recv().await {
                match ev {
                    UiEvent::RevealTerminal { host_id, .. } => {
                        let _ = proxy_show.send_event(DesktopUserEvent::OpenStandaloneTerminal { host_id });
                    }
                    UiEvent::ShowMainWindow => {
                        let _ = proxy_show.send_event(DesktopUserEvent::ShowWindow);
                    }
                    UiEvent::OpenStandaloneTerminal { host_id, .. } => {
                        let _ = proxy_show.send_event(DesktopUserEvent::OpenStandaloneTerminal { host_id });
                    }
                    UiEvent::HostsUpdated => {}
                }
            }
        });
    }

    let rgba64 = render_brand_icon_rgba(64);
    let win_icon = WindowIcon::from_rgba(rgba64.clone(), 64, 64).ok();
    let tray_icon_img = TrayIconImage::from_rgba(rgba64, 64, 64)?;

    let start_minimized = std::env::args().any(|a| a == "--minimized" || a == "--hidden");

    let window = WindowBuilder::new()
        .with_title("VPS Switch")
        .with_inner_size(Size::Logical(LogicalSize::new(1280.0, 800.0)))
        .with_min_inner_size(Size::Logical(LogicalSize::new(920.0, 600.0)))
        .with_window_icon(win_icon.clone())
        .with_visible(!start_minimized)
        .build(&event_loop)?;

    let url = format!("http://127.0.0.1:{port}");
    // 启动即最小化（开机自启的常态）时先不建 WebView，等第一次真正显示再建。
    let mut main_webview: Option<wry::WebView> = if start_minimized {
        None
    } else {
        build_main_webview(&window, &url)
    };

    // 系统托盘事件转发到主事件循环
    let proxy_tray = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = proxy_tray.send_event(DesktopUserEvent::TrayIcon(event));
    }));

    let proxy_menu = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy_menu.send_event(DesktopUserEvent::TrayMenu(event));
    }));

    let mut tray_icon = None;
    let mut standalone_windows: HashMap<WindowId, StandaloneTerminal> = HashMap::new();
    let mut host_to_window: HashMap<String, WindowId> = HashMap::new();

    event_loop.run(move |event, event_loop_target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Ok(menu) = build_tray_menu(&state) {
                    tray_icon = TrayIconBuilder::new()
                        .with_menu(Box::new(menu))
                        .with_menu_on_left_click(false)
                        .with_tooltip("VPS Switch · 右键快速连接 VPS 终端")
                        .with_icon(tray_icon_img.clone())
                        .build()
                        .ok();
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if window_id == window.id() {
                    let close_action = state
                        .db
                        .get_setting("close_action")
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "tray".to_string());
                    if close_action == "exit" {
                        standalone_windows.clear();
                        host_to_window.clear();
                        let _ = tray_icon.take();
                        *control_flow = ControlFlow::Exit;
                    } else {
                        // 默认最小化驻留到系统托盘，保持后台不断连
                        hide_main_window(&window, &main_webview);
                    }
                } else if let Some(removed) = standalone_windows.remove(&window_id) {
                    // 独立终端窗口点击 ×：完全关闭并销毁该窗口
                    host_to_window.remove(&removed.host_id);
                }
            }
            Event::UserEvent(DesktopUserEvent::ShowWindow) => {
                show_main_window(&window, &mut main_webview, &url);
            }
            Event::UserEvent(DesktopUserEvent::OpenStandaloneTerminal { host_id }) => {
                // 如果该主机已有打开的独立终端窗口，直接唤醒并聚焦
                if let Some(&win_id) = host_to_window.get(&host_id) {
                    if let Some(st_win) = standalone_windows.get(&win_id) {
                        st_win.window.set_minimized(false);
                        st_win.window.set_visible(true);
                        st_win.window.set_focus();
                        return;
                    }
                }

                // 读取主机名作为窗口标题
                let host_name = state
                    .db
                    .get_public_host(&host_id)
                    .ok()
                    .flatten()
                    .map(|h| h.name)
                    .unwrap_or_else(|| host_id.clone());

                let title = format!("{host_name} · 终端 · VPS Switch");
                let win_builder = WindowBuilder::new()
                    .with_title(&title)
                    .with_inner_size(Size::Logical(LogicalSize::new(1080.0, 680.0)))
                    .with_min_inner_size(Size::Logical(LogicalSize::new(600.0, 380.0)))
                    .with_window_icon(win_icon.clone());

                if let Ok(new_win) = win_builder.build(event_loop_target) {
                    let terminal_url = format!("http://127.0.0.1:{port}/?standalone=1&hostId={host_id}");
                    if let Ok(wv) = WebViewBuilder::new().with_url(&terminal_url).build(&new_win) {
                        let win_id = new_win.id();
                        new_win.set_visible(true);
                        new_win.set_focus();
                        host_to_window.insert(host_id.clone(), win_id);
                        standalone_windows.insert(
                            win_id,
                            StandaloneTerminal {
                                window: new_win,
                                webview: wv,
                                host_id,
                            },
                        );
                    }
                }
            }
            Event::UserEvent(DesktopUserEvent::TrayIcon(tray_ev)) => {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = tray_ev
                {
                    show_main_window(&window, &mut main_webview, &url);
                }
            }
            Event::UserEvent(DesktopUserEvent::TrayMenu(menu_ev)) => {
                let id_str = menu_ev.id.0.as_str();
                if id_str == MENU_ID_SHOW {
                    show_main_window(&window, &mut main_webview, &url);
                } else if id_str == MENU_ID_QUIT {
                    standalone_windows.clear();
                    host_to_window.clear();
                    let _ = tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                } else if let Some(host_id) = id_str.strip_prefix(MENU_HOST_PREFIX) {
                    // 托盘右键点选某台 VPS：打开专属独立终端窗口！
                    let hid = host_id.to_string();
                    let _ = proxy.send_event(DesktopUserEvent::OpenStandaloneTerminal { host_id: hid.clone() });

                    let st = state.clone();
                    rt_handle.spawn(async move {
                        if st.terminals.find_active_pane_for_host(&hid).is_none() {
                            let _ = st.terminals.create_pane(&st.ssh_pool, &st.db, &hid, 100, 30, None, false).await;
                        }
                    });
                }
            }
            _ => {}
        }
    });
}
