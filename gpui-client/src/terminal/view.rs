//! 终端面板视图（对齐 `TerminalPaneView.vue`）：
//!
//! 1. GPUI `Element` 网格渲染：
//!    - 修复 `ShapedLine::paint` 的 `wrap_width`（传 `Some(bounds.size.width)`，不再在第 4 格截断）
//!    - 完整支持 16 基础 ANSI 色 + 216 立方体色 + 24 级灰度（256 色）+ TrueColor
//!    - 完整支持 `Flags::INVERSE`（反显，htop/vim 必用）、`Flags::DIM`、`Flags::BOLD`、`Flags::ITALIC`、`Flags::UNDERLINE`
//!    - 绝不在 `prepaint` 阶段重入 `view.update()`，尺寸同步与 IME 坐标更新放在 `paint` 阶段执行
//! 2. 中文 IME 输入（`EntityInputHandler` + `window.handle_input`）：
//!    - 光标处实时显示带下划线的 IME 预编辑文本 `marked_text`
//!    - 准确向系统报告光标屏幕坐标 `ime_cursor_bounds`，输入法候选框跟随光标
//! 3. 鼠标拖拽文本选区 + 智能 `Ctrl+C`（有选区复制并清选区，无选区发 `0x03`）+ `Ctrl+V`（安全 bracketed paste）
//! 4. 底部断线重连提示条 / 会话已结束提示条 / 多分屏右上角「关闭分屏 ✕」按钮

use std::ops::Range;
use std::time::Duration;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use gpui_kit::StatefulInteractiveElement as _;
use gpui_kit::component::*;
use gpui_kit::*;

use super::session::{PaneMeta, Session, SessionCommand, SessionEvent, Snapshot};

const FONT_SIZE: f32 = 13.5;
const LINE_HEIGHT: f32 = 1.32;

pub struct TerminalView {
    pub host_id: String,
    pub pane_id: Option<String>,
    pub meta: PaneMeta,
    session: Session,
    pub snapshot: Option<Snapshot>,
    pub focus_handle: FocusHandle,
    pub dark: bool,
    pub connecting: bool,
    pub create_error: Option<String>,
    pub disconnected: Option<(u32, bool)>,
    cols: u16,
    rows: u16,

    // 鼠标拖拽选区：((start_row, start_col), (end_row, end_col))
    pub selection: Option<((usize, usize), (usize, usize))>,
    dragging_selection: bool,
    last_cell_origin: Point<Pixels>,
    last_cell_size: gpui_kit::Size<Pixels>,
    last_viewport_size: gpui_kit::Size<Pixels>,

    // 中文 IME 预编辑状态
    pub marked_text: String,
    pub marked_selection: Range<usize>,
    pub ime_cursor_bounds: Option<Bounds<Pixels>>,

    // 终端右键菜单坐标（相对于终端视图左上角）
    pub context_menu_pos: Option<Point<Pixels>>,
    scroll_remainder: f32,

    // 刷新/重连计数与无头测试样本
    pub refresh_count: u32,
    pub status_dirty: bool,
    mock_sample_line: Option<String>,
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        self.session.send(SessionCommand::Shutdown);
    }
}

impl TerminalView {
    pub fn new(host_id: String, dark: bool, cx: &mut Context<Self>) -> Self {
        let (cols, rows) = (100, 30);
        let session = Session::spawn(host_id.clone(), cols, rows);
        Self::from_session(host_id, session, dark, cols, rows, None, cx)
    }

    pub fn new_attached(
        host_id: String,
        pane: PaneMeta,
        dark: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let cols = pane.cols.max(80);
        let rows = pane.rows.max(24);
        let session = Session::attach(host_id.clone(), pane, cols, rows);
        Self::from_session(host_id, session, dark, cols, rows, None, cx)
    }

    pub fn new_mock(
        host_id: String,
        dark: bool,
        sample_line: &str,
        cx: &mut Context<Self>,
    ) -> Self {
        let (cols, rows) = (80, 24);
        let session = Session::mock_with_text(cols, rows, sample_line);
        Self::from_session(
            host_id,
            session,
            dark,
            cols,
            rows,
            Some(sample_line.to_owned()),
            cx,
        )
    }

    fn from_session(
        host_id: String,
        session: Session,
        dark: bool,
        cols: u16,
        rows: u16,
        mock_sample_line: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            host_id: host_id.clone(),
            pane_id: None,
            meta: PaneMeta {
                host_id,
                cols,
                rows,
                ..PaneMeta::default()
            },
            session,
            snapshot: None,
            focus_handle: cx.focus_handle(),
            dark,
            connecting: true,
            create_error: None,
            disconnected: None,
            cols,
            rows,
            selection: None,
            dragging_selection: false,
            last_cell_origin: point(px(0.), px(0.)),
            last_cell_size: size(px(8.), px(18.)),
            last_viewport_size: size(px(800.), px(480.)),
            marked_text: String::new(),
            marked_selection: 0..0,
            ime_cursor_bounds: None,
            context_menu_pos: None,
            scroll_remainder: 0.0,
            refresh_count: 0,
            status_dirty: false,
            mock_sample_line,
        };
        this.drain_events();
        if this.mock_sample_line.is_some() {
            this.status_dirty = false;
        }
        Self::poll(cx);
        this
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark != dark {
            self.dark = dark;
            cx.notify();
        }
    }

    pub fn manual_reconnect(&mut self, cx: &mut Context<Self>) {
        self.disconnected = None;
        self.create_error = None;
        self.session.send(SessionCommand::ManualReconnect);
        cx.notify();
    }

    pub fn reconnect_fresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_count += 1;
        self.create_error = None;
        self.disconnected = None;
        self.meta.exited = false;
        self.selection = None;
        self.marked_text.clear();
        self.marked_selection = 0..0;
        self.context_menu_pos = None;

        if let Some(sample) = &self.mock_sample_line {
            let new_id = format!("mock-pane-{}", self.refresh_count + 1);
            self.pane_id = Some(new_id.clone());
            self.meta.id = new_id;
            self.connecting = false;
            self.session = Session::mock_with_text(self.cols, self.rows, sample);
            self.snapshot = self.session.snapshot();
        } else {
            self.connecting = true;
            self.snapshot = None;
            self.session.send(SessionCommand::ReconnectFresh);
        }
        cx.notify();
    }

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        self.context_menu_pos = None;
        if let Some(selected) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            self.selection = None;
            cx.notify();
            true
        } else {
            cx.notify();
            false
        }
    }

    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        self.context_menu_pos = None;
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
        {
            self.send_paste(text);
        }
        cx.notify();
    }

    pub fn clear_screen(&mut self, cx: &mut Context<Self>) {
        self.selection = None;
        self.context_menu_pos = None;
        if let Some(snap) = &mut self.snapshot {
            for cell in &mut snap.cells {
                *cell = super::session::CellSnapshot::default();
            }
            snap.cursor = Some((0, 0));
            snap.display_offset = 0;
            snap.history_size = 0;
        }
        self.session.send(SessionCommand::Clear);
        cx.notify();
    }

    fn drain_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.session.try_recv() {
            changed = true;
            match event {
                SessionEvent::Created { pane_id, meta } => {
                    self.pane_id = Some(pane_id);
                    self.meta = meta;
                    self.connecting = false;
                    self.create_error = None;
                    self.status_dirty = true;
                }
                SessionEvent::CreateFailed(reason) => {
                    self.connecting = false;
                    self.create_error = Some(reason);
                    self.status_dirty = true;
                }
                SessionEvent::Connected => {
                    self.connecting = false;
                    self.disconnected = None;
                    self.status_dirty = true;
                }
                SessionEvent::Updated => {
                    self.snapshot = self.session.snapshot();
                }
                SessionEvent::Meta(meta) => {
                    self.meta = meta;
                    if self.meta.exited {
                        self.disconnected = None;
                    }
                }
                SessionEvent::Disconnected { attempt, exhausted } => {
                    if !self.meta.exited {
                        self.disconnected = Some((attempt, exhausted));
                    }
                }
            }
        }
        changed
    }

    fn poll(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            let interval = if crate::rendering::is_software_rendering() {
                crate::rendering::SOFTWARE_FRAME_INTERVAL
            } else {
                Duration::from_millis(16)
            };
            cx.background_executor().timer(interval).await;
            let alive = this.update(cx, |this, cx| {
                if this.drain_events() {
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
    }

    pub fn sync_viewport(
        &mut self,
        cols: u16,
        rows: u16,
        cell_origin: Point<Pixels>,
        cell_size: gpui_kit::Size<Pixels>,
        viewport_size: gpui_kit::Size<Pixels>,
        ime_bounds: Option<Bounds<Pixels>>,
    ) {
        self.last_cell_origin = cell_origin;
        self.last_cell_size = cell_size;
        self.last_viewport_size = viewport_size;
        self.ime_cursor_bounds = ime_bounds;
        if cols >= 2 && rows >= 1 && (cols != self.cols || rows != self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.session.send(SessionCommand::Resize { cols, rows });
        }
    }

    pub fn send_input(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.session.send(SessionCommand::Input(bytes));
        }
    }

    pub fn send_paste(&mut self, text: String) {
        if !text.is_empty() {
            self.session.send(SessionCommand::Paste(text));
        }
    }

    fn scroll(&mut self, lines: i32) {
        self.session.send(SessionCommand::Scroll(lines));
    }

    fn scroll_by_delta(&mut self, delta_lines: f32) {
        if delta_lines == 0.0 {
            return;
        }
        if self.scroll_remainder != 0.0 && self.scroll_remainder.signum() != delta_lines.signum() {
            self.scroll_remainder = 0.0;
        }
        let total = self.scroll_remainder + delta_lines;
        let lines = total.trunc() as i32;
        self.scroll_remainder = total - lines as f32;
        if lines != 0 {
            self.scroll(lines);
        }
    }

    /// 提取当前鼠标选区内的纯文本。
    pub fn selected_text(&self) -> Option<String> {
        let ((r0, c0), (r1, c1)) = normalized_selection(self.selection)?;
        let snap = self.snapshot.as_ref()?;
        let mut out = String::new();
        for row in r0..=r1.min(snap.rows.saturating_sub(1)) {
            let start_col = if row == r0 { c0 } else { 0 };
            let end_col = if row == r1 {
                c1.min(snap.cols.saturating_sub(1))
            } else {
                snap.cols.saturating_sub(1)
            };
            if start_col > end_col {
                continue;
            }
            let mut line = String::new();
            for col in start_col..=end_col {
                let cell = &snap.cells[row * snap.cols + col];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                line.push(if cell.c.is_control() { ' ' } else { cell.c });
            }
            if row < r1 {
                out.push_str(line.trim_end());
                out.push('\n');
            } else {
                out.push_str(line.trim_end());
            }
        }
        let trimmed = out.trim_matches('\n').to_owned();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    fn point_to_cell(&self, position: Point<Pixels>) -> (usize, usize) {
        let cw = self.last_cell_size.width.as_f32().max(1.0);
        let ch = self.last_cell_size.height.as_f32().max(1.0);
        let dx = (position.x - self.last_cell_origin.x).as_f32().max(0.0);
        let dy = (position.y - self.last_cell_origin.y).as_f32().max(0.0);
        let col = (dx / cw).floor() as usize;
        let row = (dy / ch).floor() as usize;
        (
            row.min(self.rows.saturating_sub(1) as usize),
            col.min(self.cols.saturating_sub(1) as usize),
        )
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let key = ks.key.to_lowercase();
        let ctrl_or_cmd = ks.modifiers.control || ks.modifiers.platform;

        if key == "escape" && self.context_menu_pos.is_some() {
            self.context_menu_pos = None;
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // 1. Ctrl+C：有选区则复制并清空选区；无选区则发送 0x03 (SIGINT)
        if ctrl_or_cmd && !ks.modifiers.shift && !ks.modifiers.alt && key == "c" {
            if self.copy_selection(cx) {
                cx.stop_propagation();
                return;
            }
            if ks.modifiers.control {
                self.send_input(vec![0x03]);
                cx.stop_propagation();
                return;
            }
        }

        // 2. Ctrl+V / Cmd+V：从系统剪贴板读取并发送 bracketed paste
        if ctrl_or_cmd && !ks.modifiers.shift && !ks.modifiers.alt && key == "v" {
            self.paste_from_clipboard(cx);
            cx.stop_propagation();
            return;
        }

        // 2.5 Shift+PageUp / Shift+PageDown / Shift+Up / Shift+Down 翻看终端历史记录
        if ks.modifiers.shift && !ctrl_or_cmd && !ks.modifiers.alt {
            let page = (self.rows.saturating_sub(2)).max(1) as i32;
            match key.as_str() {
                "pageup" => {
                    self.scroll(page);
                    cx.stop_propagation();
                    return;
                }
                "pagedown" => {
                    self.scroll(-page);
                    cx.stop_propagation();
                    return;
                }
                "up" => {
                    self.scroll(1);
                    cx.stop_propagation();
                    return;
                }
                "down" => {
                    self.scroll(-1);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        // 3. 特殊控制键与 Ctrl/Alt 组合键由这里直接编码发送并停止冒泡；
        //    普通可打印字符与中文输入法留给 EntityInputHandler 处理，避免重复输入。
        let app_cursor = self.snapshot.as_ref().is_some_and(|s| s.app_cursor);
        if let Some(bytes) = encode_special_key(ks, app_cursor) {
            self.selection = None;
            self.context_menu_pos = None;
            self.send_input(bytes);
            cx.stop_propagation();
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let len = self.marked_text.encode_utf16().count();
        adjusted_range.replace(0..len);
        Some(self.marked_text.clone())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.marked_selection.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked_text.is_empty()).then(|| 0..self.marked_text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text.clear();
        self.marked_selection = 0..0;
        cx.notify();
    }

    fn paste(&mut self, item: ClipboardItem, _: &mut Window, _cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            self.send_paste(text);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text.clear();
        self.marked_selection = 0..0;
        self.selection = None;
        if !text.is_empty() {
            self.send_input(text.as_bytes().to_vec());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = new_text.to_owned();
        let len = new_text.encode_utf16().count();
        self.marked_selection = new_selected_range.unwrap_or(len..len);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.ime_cursor_bounds.or(Some(element_bounds))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.marked_selection.end)
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.marked_text.encode_utf16().count())
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        !self.meta.exited && self.create_error.is_none()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let focus = self.focus_handle.clone();
        let colors = term_palette(self.dark);

        let canvas_area = div()
            .flex_1()
            .min_h_0()
            .w_full()
            .px(px(12.))
            .py(px(8.))
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    this.context_menu_pos = None;
                    let cell = this.point_to_cell(event.position);
                    this.selection = Some((cell, cell));
                    this.dragging_selection = true;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    let max_x = (this.last_viewport_size.width - px(164.)).max(px(8.));
                    let max_y = (this.last_viewport_size.height - px(140.)).max(px(8.));
                    let rel_x = (event.position.x - this.last_cell_origin.x + px(12.))
                        .clamp(px(8.), max_x);
                    let rel_y = (event.position.y - this.last_cell_origin.y + px(8.))
                        .clamp(px(8.), max_y);
                    this.context_menu_pos = Some(point(rel_x, rel_y));
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.dragging_selection && event.pressed_button == Some(MouseButton::Left) {
                    let cell = this.point_to_cell(event.position);
                    if let Some((start, _)) = this.selection {
                        this.selection = Some((start, cell));
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    this.dragging_selection = false;
                    if let Some((start, end)) = this.selection
                        && start == end
                    {
                        this.selection = None;
                        cx.notify();
                    }
                }),
            )
            .child(TerminalElement {
                view: this.clone(),
                snapshot: self.snapshot.clone(),
                focus_handle: focus.clone(),
                dark: self.dark,
                selection: normalized_selection(self.selection),
                marked_text: self.marked_text.clone(),
            });

        let mut root = div()
            .id(SharedString::from(format!(
                "terminal-pane-{}",
                self.pane_id.as_deref().unwrap_or("pending")
            )))
            .test_support()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(Hsla::from(rgb(colors.background)))
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                let line_px = (FONT_SIZE * LINE_HEIGHT).max(1.0);
                let delta_lines = match event.delta {
                    ScrollDelta::Lines(p) => p.y * 3.0,
                    ScrollDelta::Pixels(p) => p.y.as_f32() / line_px,
                };
                this.scroll_by_delta(delta_lines);
                cx.stop_propagation();
            }))
            .child(canvas_area);

        // 0. 正在创建/刷新或创建失败时的内嵌提示（保障多分屏与独立终端窗口同样有反馈与重试按钮）
        if let Some(err) = self.create_error.clone() {
            let this_retry = this.clone();
            root = root.child(
                v_flex()
                    .id("terminal-pane-error-overlay")
                    .test_support()
                    .absolute()
                    .inset_0()
                    .items_center()
                    .justify_center()
                    .gap(px(10.))
                    .p(px(20.))
                    .bg(Hsla::from(rgb(colors.background)))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(0xdc2626))
                            .child("连接终端失败"),
                    )
                    .child(
                        div()
                            .max_w(px(420.))
                            .text_size(px(12.))
                            .text_color(rgb(0x71717a))
                            .child(err),
                    )
                    .child(
                        div()
                            .id("terminal-pane-retry")
                            .test_support()
                            .rounded_xl()
                            .border_1()
                            .border_color(rgb(if self.dark { 0x3f3f46 } else { 0xe4e4e7 }))
                            .bg(rgb(if self.dark { 0x18181b } else { 0xffffff }))
                            .px(px(14.))
                            .py(px(6.))
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(colors.foreground))
                            .cursor_pointer()
                            .on_click(move |_, _, cx| {
                                let _ = this_retry.update(cx, |v, cx| v.reconnect_fresh(cx));
                            })
                            .child("重试"),
                    ),
            );
        } else if self.connecting && self.snapshot.is_none() {
            root = root.child(
                v_flex()
                    .id("terminal-pane-connecting-overlay")
                    .test_support()
                    .absolute()
                    .inset_0()
                    .items_center()
                    .justify_center()
                    .bg(Hsla::from(rgb(colors.background)))
                    .text_size(px(12.))
                    .text_color(rgb(0x71717a))
                    .child("正在连接终端…"),
            );
        }

        // 1. 连接断开提示条（`disconnected && !exited`）
        if let Some((_, exhausted)) = self.disconnected
            && !self.meta.exited
        {
            let this_reconn = this.clone();
            let msg = if exhausted {
                "终端连接已断开"
            } else {
                "连接中断，正在重连…"
            };
            root = root.child(
                h_flex()
                    .id("terminal-disconnected-bar")
                    .test_support()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .py(px(6.))
                    .border_t_1()
                    .border_color(rgb(0xfde68a))
                    .bg(rgb(0xfffbeb))
                    .text_size(px(12.))
                    .text_color(rgb(0xb45309))
                    .child(msg)
                    .child(
                        div()
                            .id("terminal-manual-reconnect")
                            .test_support()
                            .rounded_md()
                            .bg(rgb(0xd97706))
                            .text_color(rgb(0xffffff))
                            .px(px(8.))
                            .py(px(2.))
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xb45309)))
                            .on_click(move |_, _, cx| {
                                let _ = this_reconn
                                    .update(cx, |view, cx| view.manual_reconnect(cx));
                            })
                            .child("立即重连"),
                    ),
            );
        }

        // 2. 会话已结束提示条（`exited`）
        if self.meta.exited {
            let this_fresh = this.clone();
            root = root.child(
                h_flex()
                    .id("terminal-exited-bar")
                    .test_support()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(6.))
                    .border_t_1()
                    .border_color(rgb(0xe4e4e7))
                    .bg(rgb(0xfafafa))
                    .text_size(px(12.))
                    .text_color(rgb(0x71717a))
                    .child("终端会话已结束（远端断开或进程退出）")
                    .child(
                        div()
                            .id("terminal-reconnect-fresh")
                            .test_support()
                            .rounded_md()
                            .bg(rgb(0x18181b))
                            .text_color(rgb(0xffffff))
                            .px(px(10.))
                            .py(px(3.))
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .on_click(move |_, _, cx| {
                                let _ = this_fresh
                                    .update(cx, |view, cx| view.reconnect_fresh(cx));
                            })
                            .child("重新连接"),
                    ),
            );
        }

        // 3. 终端右键菜单（复制 / 粘贴 / 清屏 / 重新连接）
        if let Some(pos) = self.context_menu_pos {
            let has_selection = self.selected_text().is_some();
            root = root.child(render_context_menu(
                pos,
                self.dark,
                has_selection,
                this,
                focus,
            ));
        }

        root
    }
}

fn render_context_menu(
    pos: Point<Pixels>,
    dark: bool,
    has_selection: bool,
    this: Entity<TerminalView>,
    focus: FocusHandle,
) -> impl IntoElement {
    let menu_bg = if dark { rgb(0x18181b) } else { rgb(0xffffff) };
    let border_col = if dark { rgb(0x27272a) } else { rgb(0xe4e4e7) };
    let hover_bg = if dark { rgb(0x27272a) } else { rgb(0xf4f4f5) };
    let fg = if dark { rgb(0xf4f4f5) } else { rgb(0x18181b) };
    let muted_fg = if dark { rgb(0x71717a) } else { rgb(0xa1a1aa) };

    let copy_entity = this.clone();
    let copy_focus = focus.clone();
    let paste_entity = this.clone();
    let paste_focus = focus.clone();
    let clear_entity = this.clone();
    let clear_focus = focus.clone();
    let reconn_entity = this;
    let reconn_focus = focus;

    v_flex()
        .id("terminal-context-menu")
        .test_support()
        .absolute()
        .left(pos.x)
        .top(pos.y)
        .w(px(168.))
        .p(px(4.))
        .gap(px(2.))
        .rounded_xl()
        .border_1()
        .border_color(border_col)
        .bg(menu_bg)
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_mouse_down(MouseButton::Right, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            h_flex()
                .id("terminal-ctx-copy")
                .test_support()
                .items_center()
                .justify_between()
                .px(px(10.))
                .py(px(6.))
                .rounded_lg()
                .text_size(px(12.))
                .text_color(if has_selection { fg } else { muted_fg })
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .on_click(move |_, window, cx| {
                    window.focus(&copy_focus, cx);
                    let _ = copy_entity.update(cx, |v, cx| {
                        v.copy_selection(cx);
                    });
                    cx.stop_propagation();
                })
                .child("复制")
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted_fg)
                        .child("Ctrl+C"),
                ),
        )
        .child(
            h_flex()
                .id("terminal-ctx-paste")
                .test_support()
                .items_center()
                .justify_between()
                .px(px(10.))
                .py(px(6.))
                .rounded_lg()
                .text_size(px(12.))
                .text_color(fg)
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .on_click(move |_, window, cx| {
                    window.focus(&paste_focus, cx);
                    let _ = paste_entity.update(cx, |v, cx| {
                        v.paste_from_clipboard(cx);
                    });
                    cx.stop_propagation();
                })
                .child("粘贴")
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted_fg)
                        .child("Ctrl+V"),
                ),
        )
        .child(
            h_flex()
                .id("terminal-ctx-clear")
                .test_support()
                .items_center()
                .justify_between()
                .px(px(10.))
                .py(px(6.))
                .rounded_lg()
                .text_size(px(12.))
                .text_color(fg)
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .on_click(move |_, window, cx| {
                    window.focus(&clear_focus, cx);
                    let _ = clear_entity.update(cx, |v, cx| {
                        v.clear_screen(cx);
                    });
                    cx.stop_propagation();
                })
                .child("清屏"),
        )
        .child(div().h(px(1.)).my(px(2.)).bg(border_col))
        .child(
            h_flex()
                .id("terminal-ctx-reconnect")
                .test_support()
                .items_center()
                .justify_between()
                .px(px(10.))
                .py(px(6.))
                .rounded_lg()
                .text_size(px(12.))
                .text_color(fg)
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .on_click(move |_, window, cx| {
                    window.focus(&reconn_focus, cx);
                    let _ = reconn_entity.update(cx, |v, cx| {
                        v.reconnect_fresh(cx);
                    });
                    cx.stop_propagation();
                })
                .child("刷新 / 重新连接"),
        )
}

fn normalized_selection(
    sel: Option<((usize, usize), (usize, usize))>,
) -> Option<((usize, usize), (usize, usize))> {
    let (a, b) = sel?;
    if a == b {
        return None;
    }
    if a <= b {
        Some((a, b))
    } else {
        Some((b, a))
    }
}

fn is_cell_selected(
    sel: Option<((usize, usize), (usize, usize))>,
    row: usize,
    col: usize,
) -> bool {
    let Some(((r0, c0), (r1, c1))) = sel else {
        return false;
    };
    if row < r0 || row > r1 {
        return false;
    }
    if r0 == r1 {
        col >= c0 && col <= c1
    } else if row == r0 {
        col >= c0
    } else if row == r1 {
        col <= c1
    } else {
        true
    }
}

struct TerminalElement {
    view: Entity<TerminalView>,
    snapshot: Option<Snapshot>,
    focus_handle: FocusHandle,
    dark: bool,
    selection: Option<((usize, usize), (usize, usize))>,
    marked_text: String,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct TerminalPaintState {
    segments: Vec<Segment>,
    backgrounds: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    scrollbar_thumb: Option<PaintQuad>,
    preedit_line: Option<(Point<Pixels>, ShapedLine, PaintQuad)>,
    ime_cursor_bounds: Option<Bounds<Pixels>>,
    cell_width: Pixels,
    line_height: Pixels,
    computed_cols: u16,
    computed_rows: u16,
}

struct Segment {
    origin: Point<Pixels>,
    line: ShapedLine,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let font_size = px(FONT_SIZE);
        let mono_family = cx.theme().mono_font_family.clone();
        let mono = font(mono_family);
        let measure = TextRun {
            len: 1,
            font: mono.clone(),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let cell_width = window
            .text_system()
            .shape_line("M".into(), font_size, &[measure], None)
            .width()
            .max(px(1.));
        let line_height = window.pixel_snap(font_size * LINE_HEIGHT).max(px(1.));
        let cols = ((bounds.size.width / cell_width).floor() as usize).max(2);
        let rows = ((bounds.size.height / line_height).floor() as usize).max(1);

        let mut state = TerminalPaintState {
            segments: Vec::new(),
            backgrounds: Vec::new(),
            cursor: None,
            scrollbar_thumb: None,
            preedit_line: None,
            ime_cursor_bounds: None,
            cell_width,
            line_height,
            computed_cols: cols.min(u16::MAX as usize) as u16,
            computed_rows: rows.min(u16::MAX as usize) as u16,
        };

        let Some(snapshot) = &self.snapshot else {
            return state;
        };

        let colors = term_palette(self.dark);
        let default_fg: Hsla = Hsla::from(rgb(colors.foreground));
        let default_bg: Hsla = Hsla::from(rgb(colors.background));
        let selection_bg: Hsla = if self.dark {
            Hsla::from(rgb(0xffffff)).opacity(0.18)
        } else {
            Hsla::from(rgb(0x18181b)).opacity(0.14)
        };
        let palette: [Hsla; 16] = colors.ansi.map(|hex| Hsla::from(rgb(hex)));

        let rows_to_draw = rows.min(snapshot.rows);
        let cols_to_draw = cols.min(snapshot.cols);

        for row in 0..rows_to_draw {
            let y = bounds.top() + line_height * row as f32;
            let mut col = 0;

            // 1. 背景色区间合并（含 INVERSE 反显与鼠标选区高亮）
            while col < cols_to_draw {
                let cell = &snapshot.cells[row * snapshot.cols + col];
                let selected = is_cell_selected(self.selection, row, col);
                let cell_bg = effective_bg(cell, default_fg, default_bg, &palette);
                let paint_bg = if selected {
                    Some(selection_bg)
                } else {
                    cell_bg
                };
                let Some(color) = paint_bg else {
                    col += 1;
                    continue;
                };
                let start = col;
                col += 1;
                while col < cols_to_draw {
                    let next = &snapshot.cells[row * snapshot.cols + col];
                    let next_sel = is_cell_selected(self.selection, row, col);
                    let next_bg = if next_sel {
                        Some(selection_bg)
                    } else {
                        effective_bg(next, default_fg, default_bg, &palette)
                    };
                    if next_bg != Some(color) {
                        break;
                    }
                    col += 1;
                }
                let rect = Bounds {
                    origin: point(bounds.left() + cell_width * start as f32, y),
                    size: size(cell_width * (col - start) as f32, line_height),
                };
                state.backgrounds.push(fill(rect, color));
            }

            // 2. 文本按段塑形
            col = 0;
            while col < cols_to_draw {
                let cell = &snapshot.cells[row * snapshot.cols + col];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    || (cell.c == ' ' && !cell.flags.contains(Flags::UNDERLINE))
                {
                    col += 1;
                    continue;
                }
                let wide = cell.flags.contains(Flags::WIDE_CHAR);
                let start_col = col;
                if wide {
                    let text = cell.c.to_string();
                    let fg = effective_fg(cell, default_fg, default_bg, &palette);
                    let run = TextRun {
                        len: text.len(),
                        font: styled_font(&mono, cell.flags),
                        color: fg,
                        background_color: None,
                        underline: cell.flags.contains(Flags::UNDERLINE).then_some(
                            UnderlineStyle {
                                thickness: px(1.),
                                color: Some(fg),
                                wavy: false,
                            },
                        ),
                        strikethrough: cell.flags.contains(Flags::STRIKEOUT).then_some(
                            StrikethroughStyle {
                                thickness: px(1.),
                                color: Some(fg),
                            },
                        ),
                    };
                    let line = window.text_system().shape_line(
                        SharedString::from(text),
                        font_size,
                        &[run],
                        None,
                    );
                    state.segments.push(Segment {
                        origin: point(bounds.left() + cell_width * start_col as f32, y),
                        line,
                    });
                    col += 1;
                    continue;
                }

                // 连续单宽字符合并为一次 shape_line（带 force_width = Some(cell_width)）
                let mut text = String::new();
                let mut runs: Vec<TextRun> = Vec::new();
                while col < cols_to_draw {
                    let next = &snapshot.cells[row * snapshot.cols + col];
                    if next.flags.contains(Flags::WIDE_CHAR_SPACER)
                        || next.flags.contains(Flags::WIDE_CHAR)
                    {
                        break;
                    }
                    let ch = if next.c.is_control() { ' ' } else { next.c };
                    let len = ch.len_utf8();
                    let fg = effective_fg(next, default_fg, default_bg, &palette);
                    let fnt = styled_font(&mono, next.flags);
                    let ul = next.flags.contains(Flags::UNDERLINE).then_some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(fg),
                        wavy: false,
                    });
                    let st = next
                        .flags
                        .contains(Flags::STRIKEOUT)
                        .then_some(StrikethroughStyle {
                            thickness: px(1.),
                            color: Some(fg),
                        });
                    text.push(ch);
                    match runs.last_mut() {
                        Some(last)
                            if last.font == fnt
                                && last.color == fg
                                && last.underline == ul
                                && last.strikethrough == st =>
                        {
                            last.len += len;
                        }
                        _ => runs.push(TextRun {
                            len,
                            font: fnt,
                            color: fg,
                            background_color: None,
                            underline: ul,
                            strikethrough: st,
                        }),
                    }
                    col += 1;
                }
                if text.trim().is_empty() && runs.iter().all(|r| r.underline.is_none()) {
                    continue;
                }
                let line = window.text_system().shape_line(
                    SharedString::from(text),
                    font_size,
                    &runs,
                    Some(cell_width),
                );
                state.segments.push(Segment {
                    origin: point(bounds.left() + cell_width * start_col as f32, y),
                    line,
                });
            }
        }

        // 3. 光标与中文 IME 预编辑串
        if let Some((row, col)) = snapshot.cursor.filter(|(r, _)| *r < rows_to_draw) {
            let is_wide_cursor = col < snapshot.cols
                && snapshot.cells[row * snapshot.cols + col]
                    .flags
                    .contains(Flags::WIDE_CHAR);
            let cursor_w = if is_wide_cursor {
                cell_width * 2.0
            } else {
                cell_width
            };
            let cursor_origin = point(
                bounds.left() + cell_width * col as f32,
                bounds.top() + line_height * row as f32,
            );
            let cursor_rect = Bounds {
                origin: cursor_origin,
                size: size(cursor_w, line_height),
            };
            state.ime_cursor_bounds = Some(cursor_rect);

            if !self.marked_text.is_empty() {
                let run = TextRun {
                    len: self.marked_text.len(),
                    font: mono,
                    color: default_fg,
                    background_color: None,
                    underline: Some(UnderlineStyle {
                        thickness: px(1.5),
                        color: Some(Hsla::from(rgb(0xf97316))),
                        wavy: false,
                    }),
                    strikethrough: None,
                };
                let shaped = window.text_system().shape_line(
                    SharedString::from(self.marked_text.clone()),
                    font_size,
                    &[run],
                    None,
                );
                let bg_rect = Bounds {
                    origin: cursor_origin,
                    size: size(shaped.width().max(cursor_w), line_height),
                };
                state.preedit_line = Some((cursor_origin, shaped, fill(bg_rect, default_bg)));
            } else if snapshot.cursor_shape != CursorShape::Hidden {
                let cursor_color = Hsla::from(rgb(colors.cursor)).opacity(
                    if self.focus_handle.is_focused(window) {
                        0.72
                    } else {
                        0.28
                    },
                );
                let shape_rect = match snapshot.cursor_shape {
                    CursorShape::Beam => Bounds {
                        origin: cursor_origin,
                        size: size(px(2.), line_height),
                    },
                    CursorShape::Underline => Bounds {
                        origin: point(cursor_origin.x, cursor_origin.y + line_height - px(2.)),
                        size: size(cursor_w, px(2.)),
                    },
                    _ => cursor_rect,
                };
                state.cursor = Some(fill(shape_rect, cursor_color));
            }
        }

        // 4. 历史滚动条指示器（有 scrollback 历史时在右侧边缘绘制 4px 滑块）
        if snapshot.history_size > 0 && bounds.size.height > px(16.) {
            let total_lines = (snapshot.history_size + snapshot.rows).max(1) as f32;
            let track_h = bounds.size.height.as_f32();
            let ratio = (snapshot.rows as f32 / total_lines).clamp(0.05, 1.0);
            let thumb_h = (track_h * ratio).max(20.0).min(track_h);
            let top_lines = snapshot
                .history_size
                .saturating_sub(snapshot.display_offset) as f32;
            let progress = (top_lines / (snapshot.history_size as f32)).clamp(0.0, 1.0);
            let thumb_y = bounds.top().as_f32() + (track_h - thumb_h) * progress;
            let thumb_rect = Bounds {
                origin: point(bounds.right() - px(5.), px(thumb_y)),
                size: size(px(4.), px(thumb_h)),
            };
            let thumb_color = if self.dark {
                Hsla::from(rgb(0xffffff)).opacity(if snapshot.display_offset > 0 { 0.36 } else { 0.16 })
            } else {
                Hsla::from(rgb(0x18181b)).opacity(if snapshot.display_offset > 0 { 0.30 } else { 0.14 })
            };
            state.scrollbar_thumb = Some(fill(thumb_rect, thumb_color).corner_radii(px(2.)));
        }

        state
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 挂载 IME 输入处理器
        window.handle_input(
            &self.focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        // 在 paint 阶段安全同步终端网格尺寸与光标位置（不在 prepaint 重入）
        let cols = state.computed_cols;
        let rows = state.computed_rows;
        let cell_origin = bounds.origin;
        let cell_size = size(state.cell_width, state.line_height);
        let viewport_size = bounds.size;
        let ime_bounds = state.ime_cursor_bounds;
        self.view.update(cx, |view, _| {
            view.sync_viewport(
                cols,
                rows,
                cell_origin,
                cell_size,
                viewport_size,
                ime_bounds,
            );
        });

        let mask = ContentMask { bounds };
        window.with_content_mask(Some(mask), |window| {
            for quad in state.backgrounds.drain(..) {
                window.paint_quad(quad);
            }
            for segment in &state.segments {
                let _ = segment.line.paint(
                    segment.origin,
                    state.line_height,
                    TextAlign::Left,
                    Some(bounds.size.width),
                    window,
                    cx,
                );
            }
            if let Some((origin, line, bg_quad)) = state.preedit_line.take() {
                window.paint_quad(bg_quad);
                let _ = line.paint(
                    origin,
                    state.line_height,
                    TextAlign::Left,
                    Some(bounds.size.width),
                    window,
                    cx,
                );
            }
            if let Some(cursor) = state.cursor.take() {
                window.paint_quad(cursor);
            }
            if let Some(thumb) = state.scrollbar_thumb.take() {
                window.paint_quad(thumb);
            }
        });
    }
}

struct TermPalette {
    background: u32,
    foreground: u32,
    cursor: u32,
    ansi: [u32; 16],
}

fn term_palette(dark: bool) -> TermPalette {
    if dark {
        TermPalette {
            background: 0x0c0d10,
            foreground: 0xe4e4e7,
            cursor: 0xf4f4f5,
            ansi: [
                0x27272a, 0xf87171, 0x4ade80, 0xfacc15, 0x60a5fa, 0xc084fc, 0x22d3ee, 0xe4e4e7,
                0x71717a, 0xfca5a5, 0x86efac, 0xfde047, 0x93c5fd, 0xd8b4fe, 0x67e8f9, 0xffffff,
            ],
        }
    } else {
        TermPalette {
            background: 0xffffff,
            foreground: 0x18181b,
            cursor: 0x18181b,
            ansi: [
                0x18181b, 0xdc2626, 0x16a34a, 0xca8a04, 0x2563eb, 0x9333ea, 0x0891b2, 0xe4e4e7,
                0x71717a, 0xef4444, 0x22c55e, 0xeab308, 0x3b82f6, 0xa855f7, 0x06b6d4, 0xfafafa,
            ],
        }
    }
}

fn effective_bg(
    cell: &super::session::CellSnapshot,
    default_fg: Hsla,
    default_bg: Hsla,
    palette: &[Hsla; 16],
) -> Option<Hsla> {
    if cell.flags.contains(Flags::INVERSE) {
        Some(resolve_color(cell.fg, default_fg, default_bg, palette))
    } else if matches!(cell.bg, Color::Named(NamedColor::Background)) {
        None
    } else {
        Some(resolve_color(cell.bg, default_fg, default_bg, palette))
    }
}

fn effective_fg(
    cell: &super::session::CellSnapshot,
    default_fg: Hsla,
    default_bg: Hsla,
    palette: &[Hsla; 16],
) -> Hsla {
    let base = if cell.flags.contains(Flags::INVERSE) {
        resolve_color(cell.bg, default_fg, default_bg, palette)
    } else {
        resolve_color(cell.fg, default_fg, default_bg, palette)
    };
    if cell.flags.contains(Flags::DIM) {
        base.opacity(0.66)
    } else {
        base
    }
}

fn resolve_color(color: Color, default_fg: Hsla, default_bg: Hsla, palette: &[Hsla; 16]) -> Hsla {
    match color {
        Color::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground => default_fg,
            NamedColor::Background => default_bg,
            NamedColor::DimForeground => default_fg.opacity(0.66),
            NamedColor::Cursor => default_fg,
            other => named_index(other)
                .map(|index| palette[index])
                .unwrap_or(default_fg),
        },
        Color::Indexed(index) => match index {
            0..=15 => palette[index as usize],
            16..=231 => {
                // 6x6x6 RGB 立方体
                let idx = index - 16;
                let r_idx = (idx / 36) % 6;
                let g_idx = (idx / 6) % 6;
                let b_idx = idx % 6;
                const LEVELS: [u32; 6] = [0, 95, 135, 175, 215, 255];
                let hex =
                    (LEVELS[r_idx as usize] << 16) | (LEVELS[g_idx as usize] << 8) | LEVELS[b_idx as usize];
                Hsla::from(rgb(hex))
            }
            232..=255 => {
                // 24 级灰度
                let v = u32::from(8 + (index - 232) * 10);
                Hsla::from(rgb((v << 16) | (v << 8) | v))
            }
        },
        Color::Spec(rgb_color) => Hsla::from(rgb(
            (u32::from(rgb_color.r) << 16)
                | (u32::from(rgb_color.g) << 8)
                | u32::from(rgb_color.b),
        )),
    }
}

fn named_index(color: NamedColor) -> Option<usize> {
    use NamedColor::*;
    Some(match color {
        Black | DimBlack => 0,
        Red | DimRed => 1,
        Green | DimGreen => 2,
        Yellow | DimYellow => 3,
        Blue | DimBlue => 4,
        Magenta | DimMagenta => 5,
        Cyan | DimCyan => 6,
        White | DimWhite => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        _ => return None,
    })
}

fn styled_font(font: &Font, flags: Flags) -> Font {
    let mut font = font.clone();
    if flags.contains(Flags::BOLD) {
        font.weight = FontWeight::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        font.style = FontStyle::Italic;
    }
    font
}

/// 只编码特殊控制键与 Ctrl/Alt 组合键；普通字符与 IME 交给 `EntityInputHandler`。
fn encode_special_key(keystroke: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let key = keystroke.key.as_str();
    let ctrl = keystroke.modifiers.control;
    let alt = keystroke.modifiers.alt;
    let shift = keystroke.modifiers.shift;
    let sequence: Vec<u8> = match key {
        "enter" => b"\r".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        "escape" => b"\x1b".to_vec(),
        "up" => {
            if ctrl {
                return Some(b"\x1b[1;5A".to_vec());
            } else if app_cursor && !alt && !shift {
                return Some(b"\x1bOA".to_vec());
            } else {
                b"\x1b[A".to_vec()
            }
        }
        "down" => {
            if ctrl {
                return Some(b"\x1b[1;5B".to_vec());
            } else if app_cursor && !alt && !shift {
                return Some(b"\x1bOB".to_vec());
            } else {
                b"\x1b[B".to_vec()
            }
        }
        "right" => {
            if ctrl {
                return Some(b"\x1b[1;5C".to_vec());
            } else if app_cursor && !alt && !shift {
                return Some(b"\x1bOC".to_vec());
            } else {
                b"\x1b[C".to_vec()
            }
        }
        "left" => {
            if ctrl {
                return Some(b"\x1b[1;5D".to_vec());
            } else if app_cursor && !alt && !shift {
                return Some(b"\x1bOD".to_vec());
            } else {
                b"\x1b[D".to_vec()
            }
        }
        "home" => {
            if app_cursor && !ctrl && !alt && !shift {
                return Some(b"\x1bOH".to_vec());
            } else {
                b"\x1b[H".to_vec()
            }
        }
        "end" => {
            if app_cursor && !ctrl && !alt && !shift {
                return Some(b"\x1bOF".to_vec());
            } else {
                b"\x1b[F".to_vec()
            }
        }
        "insert" => b"\x1b[2~".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" => b"\x1b[24~".to_vec(),
        "space" if ctrl => vec![0x00],
        _ => {
            if ctrl {
                let ch = key.chars().next()?.to_ascii_lowercase();
                if ch.is_ascii_lowercase() {
                    vec![(ch as u8) & 0x1f]
                } else if key == "[" {
                    vec![0x1b]
                } else if key == "\\" {
                    vec![0x1c]
                } else if key == "]" {
                    vec![0x1d]
                } else if key == "/" || key == "_" {
                    vec![0x1f]
                } else {
                    return None;
                }
            } else if alt {
                let text = keystroke.key_char.clone().unwrap_or_else(|| key.to_owned());
                if text.chars().count() == 1 {
                    text.into_bytes()
                } else {
                    return None;
                }
            } else {
                // 普通可打印字符返回 None，交给 EntityInputHandler::replace_text_in_range
                return None;
            }
        }
    };
    if alt {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(&sequence);
        Some(bytes)
    } else {
        Some(sequence)
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::vte::ansi::Color;
    use gpui_kit::{Hsla, rgb};

    use super::{resolve_color, term_palette};

    #[test]
    fn resolves_256_color_cube_and_grayscale_distinct_from_base_16() {
        let p = term_palette(false);
        let default_fg = Hsla::from(rgb(p.foreground));
        let default_bg = Hsla::from(rgb(p.background));
        let ansi = p.ansi.map(|hex| Hsla::from(rgb(hex)));

        let idx_196 = resolve_color(Color::Indexed(196), default_fg, default_bg, &ansi);
        let idx_4 = resolve_color(Color::Indexed(4), default_fg, default_bg, &ansi);
        assert_ne!(
            idx_196, idx_4,
            "Indexed(196) 必须是 6x6x6 红色立方体，不能模 16 变成蓝色 Indexed(4)"
        );

        let gray_240 = resolve_color(Color::Indexed(240), default_fg, default_bg, &ansi);
        assert!(
            (gray_240.s).abs() < 0.01,
            "Indexed(240) 必须是无饱和度的灰度色"
        );
    }
}
