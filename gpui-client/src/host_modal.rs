//! 新建 / 编辑主机弹窗（`HostModal.vue`）与删除确认弹窗（`App.vue` 2987-3018 行）。

use gpui_kit::StatefulInteractiveElement as _;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::*;

use crate::api::{Host, HostLink};
use crate::assets::CatalogIcon;
use crate::palette::Palette;
use crate::widgets::lucide_icon;
use crate::workspace::Workspace;

#[derive(Clone)]
pub struct LinkRowInput {
    pub id: String,
    pub name: Entity<InputState>,
    pub url: Entity<InputState>,
    pub description: String,
}

pub struct HostModalState {
    pub editing_id: Option<String>,
    pub editing_name: String,
    pub is_local: bool,
    pub has_saved_password: bool,
    pub has_saved_private_key: bool,

    pub name: Entity<InputState>,
    pub addr: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub auth_type: String, // "password" | "privateKey" | "none"
    pub password: Entity<InputState>,
    pub private_key: Entity<TextareaState>,
    pub passphrase: Entity<InputState>,
    pub jump_host_id: String,
    pub jump_dropdown_open: bool,
    pub role: String, // "" | "primary" | "proxy" | "test" | "archive"
    pub tags: Entity<InputState>,
    pub pinned: bool,
    pub archived: bool,
    pub mcp_exposed: bool,
    pub links: Vec<LinkRowInput>,
    pub saving: bool,
    pub error_msg: Option<String>,
}

impl HostModalState {
    pub fn new(editing: Option<&Host>, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let is_local = editing.is_some_and(|h| h.kind == "local");
        let name_val = editing.map(|h| h.name.clone()).unwrap_or_default();
        let addr_val = editing.map(|h| h.addr.clone()).unwrap_or_default();
        let port_val = editing
            .map(|h| h.port.to_string())
            .unwrap_or_else(|| "22".into());
        let user_val = editing
            .map(|h| h.username.clone())
            .unwrap_or_else(|| "root".into());
        let auth_type = editing
            .map(|h| match h.auth_type.as_str() {
                "privateKey" => "privateKey".to_owned(),
                "none" => "none".to_owned(),
                _ => "password".to_owned(),
            })
            .unwrap_or_else(|| "password".into());
        let jump_host_id = editing
            .and_then(|h| h.jump_host_id.clone())
            .unwrap_or_default();
        let role = editing.and_then(|h| h.role.clone()).unwrap_or_default();
        let tags_val = editing.map(|h| h.tags.join(", ")).unwrap_or_default();
        let pinned = editing.is_some_and(|h| h.pinned);
        let archived = editing.is_some_and(|h| h.archived);
        let mcp_exposed = editing.map(|h| h.mcp_exposed).unwrap_or(true);

        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("如 hk-web-1 / 主力机")
                .default_value(name_val)
        });
        let addr = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("1.2.3.4 或 example.com")
                .default_value(addr_val)
        });
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("22")
                .default_value(port_val)
        });
        let username = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("root")
                .default_value(user_val)
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入 SSH 密码")
                .masked(true)
        });
        let private_key = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder("-----BEGIN OPENSSH PRIVATE KEY-----")
        });
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("无口令可留空")
                .masked(true)
        });
        let tags = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("如 hk, bgp, docker")
                .default_value(tags_val)
        });

        for field in [&name, &addr, &port, &username, &password, &passphrase, &tags] {
            cx.subscribe_in(field, window, |ws, _, event: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    ws.submit_host_modal(cx);
                }
            })
            .detach();
        }

        let source_links = editing
            .map(|h| h.links.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| vec![HostLink::default()]);
        let links = source_links
            .into_iter()
            .map(|item| Self::make_link_row(item, window, cx))
            .collect();

        Self {
            editing_id: editing.map(|h| h.id.clone()),
            editing_name: editing.map(|h| h.name.clone()).unwrap_or_default(),
            is_local,
            has_saved_password: editing.is_some_and(|h| h.has_password),
            has_saved_private_key: editing.is_some_and(|h| h.has_private_key),
            name,
            addr,
            port,
            username,
            auth_type,
            password,
            private_key,
            passphrase,
            jump_host_id,
            jump_dropdown_open: false,
            role,
            tags,
            pinned,
            archived,
            mcp_exposed,
            links,
            saving: false,
            error_msg: None,
        }
    }

    pub fn make_link_row(item: HostLink, window: &mut Window, cx: &mut App) -> LinkRowInput {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("名称")
                .default_value(item.name)
        });
        let url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://...")
                .default_value(item.url)
        });
        LinkRowInput {
            id: item.id,
            name,
            url,
            description: item.description,
        }
    }

    pub fn build_payload(&mut self, cx: &App) -> Result<(serde_json::Value, Option<String>), String> {
        let name = self.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            let err = "请填写主机名称".to_owned();
            self.error_msg = Some(err.clone());
            return Err(err);
        }
        let addr = if self.is_local {
            "127.0.0.1".to_owned()
        } else {
            let raw = self.addr.read(cx).value().trim().to_owned();
            if raw.is_empty() {
                let err = "请填写主机 IP 或域名".to_owned();
                self.error_msg = Some(err.clone());
                return Err(err);
            }
            raw
        };
        self.error_msg = None;

        let port: u16 = self
            .port
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p >= 1)
            .unwrap_or(22);
        let username_raw = self.username.read(cx).value().trim().to_owned();
        let username = if username_raw.is_empty() {
            "root".to_owned()
        } else {
            username_raw
        };

        let tags_raw = self.tags.read(cx).value().to_string();
        let parsed_tags: Vec<String> = tags_raw
            .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '，' | '、'))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();

        let valid_links: Vec<HostLink> = self
            .links
            .iter()
            .filter_map(|row| {
                let n = row.name.read(cx).value().trim().to_owned();
                let u = row.url.read(cx).value().trim().to_owned();
                if n.is_empty() || u.is_empty() {
                    None
                } else {
                    Some(HostLink {
                        id: row.id.clone(),
                        name: n,
                        url: u,
                        description: row.description.trim().to_owned(),
                    })
                }
            })
            .collect();

        let jump = if self.jump_host_id.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(self.jump_host_id.clone())
        };
        let role = if self.role.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(self.role.clone())
        };

        let mut payload = serde_json::json!({
            "name": name,
            "addr": addr,
            "port": port,
            "username": username,
            "authType": self.auth_type,
            "jumpHostId": jump,
            "role": role,
            "tags": parsed_tags,
            "links": valid_links,
            "pinned": self.pinned,
            "archived": self.archived,
            "mcpExposed": self.mcp_exposed,
        });

        if self.auth_type == "password" {
            let pwd = self.password.read(cx).value().to_string();
            if !pwd.is_empty() {
                payload["password"] = serde_json::Value::String(pwd);
            }
        } else if self.auth_type == "privateKey" {
            let pk = self.private_key.read(cx).value().to_string();
            if !pk.trim().is_empty() {
                payload["privateKey"] = serde_json::Value::String(pk);
            }
            let pass = self.passphrase.read(cx).value().to_string();
            if !pass.is_empty() {
                payload["passphrase"] = serde_json::Value::String(pass);
            }
        }

        Ok((payload, self.editing_id.clone()))
    }
}

/// 渲染新建 / 编辑主机弹窗浮层。
pub fn render_host_modal(
    workspace: &Workspace,
    this: &Entity<Workspace>,
    palette: &Palette,
    _cx: &App,
) -> AnyElement {
    let Some(modal) = &workspace.host_modal else {
        return div().into_any_element();
    };

    let title = if modal.editing_id.is_some() {
        format!("编辑主机 · {}", modal.editing_name)
    } else {
        "新增 VPS 主机".to_owned()
    };

    let this_backdrop = this.clone();
    let this_close = this.clone();
    let this_cancel = this.clone();
    let this_save = this.clone();

    let header = h_flex()
        .items_center()
        .justify_between()
        .px(px(20.))
        .py(px(14.))
        .border_b_1()
        .border_color(palette.divider)
        .child(
            div()
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(palette.text_primary)
                .child(title),
        )
        .child(
            div()
                .id("modal-close-x")
                .test_support()
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.))
                .rounded_lg()
                .text_color(palette.text_faint)
                .cursor_pointer()
                .hover(|s| s.bg(palette.subtle_bg).text_color(palette.text_primary))
                .on_click(move |_, _, cx| {
                    let _ = this_close.update(cx, |w, cx| w.close_host_modal(cx));
                })
                .child(lucide_icon(CatalogIcon::X, 14., palette.text_muted)),
        );

    let mut form = v_flex().w_full().gap(px(14.));

    if let Some(err) = &modal.error_msg {
        form = form.child(
            div()
                .id("modal-error")
                .test_support()
                .rounded_xl()
                .border_1()
                .border_color(rgb(0xfecaca))
                .bg(rgb(0xfef2f2))
                .px(px(12.))
                .py(px(8.))
                .text_size(px(12.))
                .text_color(rgb(0xb91c1c))
                .child(err.clone()),
        );
    }

    // 第一行：主机名称 + 角色分类
    let role_options: [(&str, &str); 5] = [
        ("", "未标记"),
        ("primary", "主力"),
        ("proxy", "代理"),
        ("test", "测试"),
        ("archive", "归档"),
    ];
    let mut role_buttons = h_flex()
        .mt(px(4.))
        .gap(px(4.))
        .p(px(3.))
        .rounded_xl()
        .bg(palette.subtle_bg);
    for (val, label) in role_options {
        let active = modal.role == val;
        let this_role = this.clone();
        let role_str = val.to_owned();
        let btn = div()
            .id(SharedString::from(format!(
                "modal-role-{}",
                if val.is_empty() { "none" } else { val }
            )))
            .test_support()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .py(px(5.))
            .rounded_lg()
            .text_size(px(11.))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                let r = role_str.clone();
                let _ = this_role.update(cx, |w, cx| {
                    if let Some(m) = &mut w.host_modal {
                        m.role = r;
                        cx.notify();
                    }
                });
            })
            .child(label);
        role_buttons = role_buttons.child(if active {
            btn.bg(palette.card_solid_bg)
                .text_color(palette.text_primary)
                .shadow_sm()
        } else {
            btn.text_color(palette.text_muted)
        });
    }

    form = form.child(
        h_flex()
            .items_start()
            .gap(px(12.))
            .child(
                v_flex()
                    .flex_1()
                    .gap(px(4.))
                    .child(field_label("主机名称", palette))
                    .child(Input::new(&modal.name)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .gap(px(4.))
                    .child(field_label("角色分类", palette))
                    .child(role_buttons),
            ),
    );

    if !modal.is_local {
        // 第二行：IP/域名 + 端口 + 用户名
        form = form.child(
            h_flex()
                .items_start()
                .gap(px(10.))
                .child(
                    v_flex()
                        .flex_1()
                        .gap(px(4.))
                        .child(field_label("IP / 域名", palette))
                        .child(Input::new(&modal.addr)),
                )
                .child(
                    v_flex()
                        .w(px(96.))
                        .gap(px(4.))
                        .child(field_label("端口", palette))
                        .child(Input::new(&modal.port)),
                )
                .child(
                    v_flex()
                        .w(px(110.))
                        .gap(px(4.))
                        .child(field_label("用户名", palette))
                        .child(Input::new(&modal.username)),
                ),
        );

        // 第三行：认证方式 + 跳板机
        let is_pwd = modal.auth_type == "password";
        let is_key = modal.auth_type == "privateKey";
        let is_none = modal.auth_type == "none";
        let this_auth_pwd = this.clone();
        let this_auth_key = this.clone();
        let this_auth_none = this.clone();
        let auth_switch = h_flex()
            .mt(px(4.))
            .gap(px(4.))
            .p(px(4.))
            .rounded_xl()
            .bg(palette.subtle_bg)
            .child({
                let b = div()
                    .id("modal-auth-password")
                    .test_support()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .py(px(5.))
                    .rounded_lg()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        let _ = this_auth_pwd.update(cx, |w, cx| {
                            if let Some(m) = &mut w.host_modal {
                                m.auth_type = "password".into();
                                cx.notify();
                            }
                        });
                    })
                    .child("密码");
                if is_pwd {
                    b.bg(palette.card_solid_bg)
                        .text_color(palette.text_primary)
                        .shadow_sm()
                } else {
                    b.text_color(palette.text_muted)
                }
            })
            .child({
                let b = div()
                    .id("modal-auth-key")
                    .test_support()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .py(px(5.))
                    .rounded_lg()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        let _ = this_auth_key.update(cx, |w, cx| {
                            if let Some(m) = &mut w.host_modal {
                                m.auth_type = "privateKey".into();
                                cx.notify();
                            }
                        });
                    })
                    .child("SSH 私钥");
                if is_key {
                    b.bg(palette.card_solid_bg)
                        .text_color(palette.text_primary)
                        .shadow_sm()
                } else {
                    b.text_color(palette.text_muted)
                }
            })
            .child({
                let b = div()
                    .id("modal-auth-none")
                    .test_support()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .py(px(5.))
                    .rounded_lg()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        let _ = this_auth_none.update(cx, |w, cx| {
                            if let Some(m) = &mut w.host_modal {
                                m.auth_type = "none".into();
                                cx.notify();
                            }
                        });
                    })
                    .child("免凭据");
                if is_none {
                    b.bg(palette.card_solid_bg)
                        .text_color(palette.text_primary)
                        .shadow_sm()
                } else {
                    b.text_color(palette.text_muted)
                }
            });

        // 跳板机下拉展开选择（直连 + 本机代理端口 + 各候选 SSH 主机，可滚动点选）
        let proxy_port = workspace.settings.proxy_port.max(1);
        let candidates: Vec<(String, String)> = vec![
            (String::new(), "直连（不使用跳板）".to_owned()),
            (
                "local-proxy".to_owned(),
                format!("本机代理 (127.0.0.1:{proxy_port})"),
            ),
        ]
        .into_iter()
        .chain(
            workspace
                .hosts
                .iter()
                .filter(|h| h.kind == "ssh" && Some(h.id.as_str()) != modal.editing_id.as_deref())
                .map(|h| (h.id.clone(), format!("{} ({}:{})", h.name, h.addr, h.port))),
        )
        .collect();
        let current_jump_label = candidates
            .iter()
            .find(|(id, _)| *id == modal.jump_host_id)
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| "直连（不使用跳板）".to_owned());
        let dropdown_open = modal.jump_dropdown_open;
        let this_jump_toggle = this.clone();
        let jump_trigger = div()
            .id("modal-jump-cycle")
            .test_support()
            .mt(px(4.))
            .h(px(32.))
            .px(px(12.))
            .rounded_xl()
            .border_1()
            .border_color(if dropdown_open {
                palette.text_secondary
            } else {
                palette.border
            })
            .bg(if dropdown_open {
                palette.card_solid_bg
            } else {
                palette.muted_surface_bg
            })
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(12.))
            .text_color(palette.text_primary)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(move |_, _, cx| {
                let _ = this_jump_toggle.update(cx, |w, cx| {
                    if let Some(m) = &mut w.host_modal {
                        m.jump_dropdown_open = !m.jump_dropdown_open;
                        cx.notify();
                    }
                });
            })
            .child(div().truncate().child(current_jump_label))
            .child(lucide_icon(
                if dropdown_open {
                    CatalogIcon::ChevronUp
                } else {
                    CatalogIcon::ChevronDown
                },
                13.,
                palette.text_faint,
            ));

        let mut jump_col = v_flex()
            .flex_1()
            .gap(px(4.))
            .child(field_label("跳板机（可选）", palette))
            .child(jump_trigger);

        if dropdown_open {
            let mut list_inner = v_flex().w_full().gap(px(2.));
            for (idx, (cand_id, cand_label)) in candidates.into_iter().enumerate() {
                let selected = cand_id == modal.jump_host_id;
                let this_pick = this.clone();
                let picked_id = cand_id.clone();
                let item = h_flex()
                    .id(SharedString::from(format!("modal-jump-opt-{idx}")))
                    .test_support()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded_lg()
                    .text_size(px(12.))
                    .cursor_pointer()
                    .hover(|s| s.bg(palette.subtle_bg))
                    .on_click(move |_, _, cx| {
                        let next_id = picked_id.clone();
                        let _ = this_pick.update(cx, |w, cx| {
                            if let Some(m) = &mut w.host_modal {
                                m.jump_host_id = next_id;
                                m.jump_dropdown_open = false;
                                cx.notify();
                            }
                        });
                    })
                    .child(div().truncate().child(cand_label))
                    .child(if selected {
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.text_primary)
                            .child("✓")
                    } else {
                        div()
                    });
                list_inner = list_inner.child(if selected {
                    item.bg(palette.subtle_bg)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.text_primary)
                } else {
                    item.text_color(palette.text_secondary)
                });
            }

            let dropdown_panel = div()
                .id("modal-jump-dropdown")
                .test_support()
                .mt(px(2.))
                .max_h(px(208.))
                .overflow_y_scroll()
                .on_scroll_wheel(|_, _, cx| {
                    cx.stop_propagation();
                })
                .rounded_xl()
                .border_1()
                .border_color(palette.border)
                .bg(palette.card_solid_bg)
                .p(px(4.))
                .shadow_lg()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(list_inner);
            jump_col = jump_col.child(dropdown_panel);
        }

        form = form.child(
            h_flex()
                .items_start()
                .gap(px(12.))
                .child(
                    v_flex()
                        .flex_1()
                        .gap(px(4.))
                        .child(field_label("认证方式", palette))
                        .child(auth_switch),
                )
                .child(jump_col),
        );

        // 第四行：密码 或 私钥 + 口令（免凭据模式无需输入）
        if is_pwd {
            let label = if modal.editing_id.is_some() && modal.has_saved_password {
                "SSH 密码（已保存加密密码，留空则保持不变）"
            } else {
                "SSH 密码"
            };
            form = form.child(
                v_flex()
                    .gap(px(4.))
                    .child(field_label(label, palette))
                    .child(Input::new(&modal.password)),
            );
        } else if is_key {
            let key_label = if modal.editing_id.is_some() && modal.has_saved_private_key {
                "SSH 私钥 (PEM / OpenSSH)（已保存加密私钥，留空则保持不变）"
            } else {
                "SSH 私钥 (PEM / OpenSSH)"
            };
            form = form.child(
                v_flex()
                    .gap(px(10.))
                    .child(
                        v_flex()
                            .gap(px(4.))
                            .child(field_label(key_label, palette))
                            .child(Textarea::new(&modal.private_key)),
                    )
                    .child(
                        v_flex()
                            .gap(px(4.))
                            .child(field_label("私钥口令 Passphrase（可选）", palette))
                            .child(Input::new(&modal.passphrase)),
                    ),
            );
        }
    }

    // 第五行：标签
    form = form.child(
        v_flex()
            .gap(px(4.))
            .child(field_label("标签（逗号或空格分隔）", palette))
            .child(Input::new(&modal.tags)),
    );

    // 第六行：快捷链接
    let this_add_link = this.clone();
    let mut links_section = v_flex().gap(px(6.)).child(
        h_flex()
            .items_center()
            .justify_between()
            .child(field_label("快捷链接（商家后台 / 面板）", palette))
            .child(
                div()
                    .id("modal-add-link")
                    .test_support()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(palette.text_secondary)
                    .cursor_pointer()
                    .hover(|s| s.text_color(palette.text_primary))
                    .on_click(move |_, window, cx| {
                        let _ = this_add_link.update(cx, |w, cx| {
                            if let Some(m) = &mut w.host_modal {
                                let row =
                                    HostModalState::make_link_row(HostLink::default(), window, cx);
                                m.links.push(row);
                                cx.notify();
                            }
                        });
                    })
                    .child("+ 添加一行"),
            ),
    );

    for (idx, row) in modal.links.iter().enumerate() {
        let this_remove = this.clone();
        links_section = links_section.child(
            h_flex()
                .items_center()
                .gap(px(6.))
                .child(div().w(px(120.)).child(Input::new(&row.name)))
                .child(div().flex_1().child(Input::new(&row.url)))
                .child(
                    div()
                        .id(SharedString::from(format!("modal-remove-link-{idx}")))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(28.))
                        .rounded_lg()
                        .text_color(palette.text_faint)
                        .cursor_pointer()
                        .hover(|s| s.bg(palette.subtle_bg).text_color(palette.text_primary))
                        .on_click(move |_, window, cx| {
                            let _ = this_remove.update(cx, |w, cx| {
                                if let Some(m) = &mut w.host_modal {
                                    if idx < m.links.len() {
                                        m.links.remove(idx);
                                    }
                                    if m.links.is_empty() {
                                        let empty = HostModalState::make_link_row(
                                            HostLink::default(),
                                            window,
                                            cx,
                                        );
                                        m.links.push(empty);
                                    }
                                    cx.notify();
                                }
                            });
                        })
                        .child("×"),
                ),
        );
    }
    form = form.child(links_section);

    // 第七行：置顶显示 / 归档隐藏 / 允许 MCP 访问
    {
        let pinned = modal.pinned;
        let archived = modal.archived;
        let mcp_exposed = modal.mcp_exposed;
        let this_pin = this.clone();
        let this_arch = this.clone();
        let this_mcp = this.clone();
        let mut checks_row = h_flex().pt(px(4.)).items_center().gap(px(20.));
        if !modal.is_local {
            checks_row = checks_row
                .child(checkbox_toggle(
                    "modal-check-pinned",
                    "置顶显示",
                    pinned,
                    palette,
                    move |_, _, cx| {
                        let _ = this_pin.update(cx, |w, cx| {
                            if let Some(m) = &mut w.host_modal {
                                m.pinned = !m.pinned;
                                cx.notify();
                            }
                        });
                    },
                ))
                .child(checkbox_toggle(
                    "modal-check-archived",
                    "归档隐藏",
                    archived,
                    palette,
                    move |_, _, cx| {
                        let _ = this_arch.update(cx, |w, cx| {
                            if let Some(m) = &mut w.host_modal {
                                m.archived = !m.archived;
                                cx.notify();
                            }
                        });
                    },
                ));
        }
        checks_row = checks_row.child(checkbox_toggle(
            "modal-check-mcp-exposed",
            "允许 MCP 访问",
            mcp_exposed,
            palette,
            move |_, _, cx| {
                let _ = this_mcp.update(cx, |w, cx| {
                    if let Some(m) = &mut w.host_modal {
                        m.mcp_exposed = !m.mcp_exposed;
                        cx.notify();
                    }
                });
            },
        ));
        form = form.child(checks_row);
    }

    let body = div()
        .id("modal-body-scroll")
        .max_h(px(468.))
        .min_h_0()
        .overflow_y_scroll()
        .on_scroll_wheel(|_, _, cx| {
            cx.stop_propagation();
        })
        .px(px(20.))
        .py(px(16.))
        .child(form);

    let footer = h_flex()
        .items_center()
        .justify_end()
        .gap(px(8.))
        .px(px(20.))
        .py(px(12.))
        .border_t_1()
        .border_color(palette.divider)
        .bg(palette.muted_surface_bg)
        .child(
            div()
                .id("modal-cancel")
                .test_support()
                .rounded_xl()
                .border_1()
                .border_color(palette.border)
                .bg(palette.card_solid_bg)
                .px(px(16.))
                .py(px(6.))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(palette.text_secondary)
                .cursor_pointer()
                .hover(|s| s.bg(palette.subtle_bg))
                .on_click(move |_, _, cx| {
                    let _ = this_cancel.update(cx, |w, cx| w.close_host_modal(cx));
                })
                .child("取消"),
        )
        .child({
            let saving = modal.saving;
            div()
                .id("modal-save")
                .test_support()
                .rounded_xl()
                .bg(palette.primary_bg)
                .text_color(palette.primary_fg)
                .opacity(if saving { 0.65 } else { 1.0 })
                .px(px(16.))
                .py(px(6.))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    if !saving {
                        let _ = this_save.update(cx, |w, cx| w.submit_host_modal(cx));
                    }
                })
                .child(if saving { "保存中..." } else { "保存" })
        });

    div()
        .id("host-modal-backdrop")
        .test_support()
        .occlude()
        .on_scroll_wheel(|_, _, cx| {
            cx.stop_propagation();
        })
        .absolute()
        .inset_0()
        .bg(Hsla::from(rgb(0x18181b)).opacity(0.32))
        .flex()
        .items_center()
        .justify_center()
        .p(px(16.))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            let _ = this_backdrop.update(cx, |w, cx| w.close_host_modal(cx));
        })
        .child({
            let this_card = this.clone();
            div()
                .id("host-modal-card")
                .test_support()
                .w(px(520.))
                .max_h(px(620.))
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded_2xl()
                .border_1()
                .border_color(palette.border)
                .bg(palette.card_solid_bg)
                .shadow_xl()
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    let _ = this_card.update(cx, |w, cx| {
                        if let Some(m) = &mut w.host_modal
                            && m.jump_dropdown_open
                        {
                            m.jump_dropdown_open = false;
                            cx.notify();
                        }
                    });
                    cx.stop_propagation();
                })
                .child(header)
                .child(body)
                .child(footer)
        })
        .into_any_element()
}

/// 渲染删除确认弹窗浮层（`App.vue` 2987-3018 行）。
pub fn render_delete_confirm(
    workspace: &Workspace,
    this: &Entity<Workspace>,
    palette: &Palette,
    _cx: &App,
) -> AnyElement {
    let Some(host) = &workspace.deleting_host else {
        return div().into_any_element();
    };
    let this_backdrop = this.clone();
    let this_cancel = this.clone();
    let this_confirm = this.clone();

    div()
        .id("delete-modal-backdrop")
        .test_support()
        .occlude()
        .on_scroll_wheel(|_, _, cx| {
            cx.stop_propagation();
        })
        .absolute()
        .inset_0()
        .bg(Hsla::from(rgb(0x18181b)).opacity(0.32))
        .flex()
        .items_center()
        .justify_center()
        .p(px(16.))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            let _ = this_backdrop.update(cx, |w, cx| w.cancel_delete_host(cx));
        })
        .child(
            div()
                .id("delete-modal-card")
                .test_support()
                .w(px(384.))
                .rounded_2xl()
                .border_1()
                .border_color(palette.border)
                .bg(palette.card_solid_bg)
                .p(px(20.))
                .shadow_xl()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.text_primary)
                        .child("确认删除 VPS 主机"),
                )
                .child(
                    div()
                        .mt(px(8.))
                        .text_size(px(12.))
                        .text_color(palette.text_muted)
                        .child(format!(
                            "确定从仓库中删除「{}」({}) 吗？此操作不可撤销。",
                            host.name, host.addr
                        )),
                )
                .child(
                    h_flex()
                        .mt(px(16.))
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("delete-modal-cancel")
                                .test_support()
                                .rounded_xl()
                                .border_1()
                                .border_color(palette.border)
                                .bg(palette.card_solid_bg)
                                .px(px(14.))
                                .py(px(6.))
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(palette.text_secondary)
                                .cursor_pointer()
                                .hover(|s| s.bg(palette.subtle_bg))
                                .on_click(move |_, _, cx| {
                                    let _ = this_cancel
                                        .update(cx, |w, cx| w.cancel_delete_host(cx));
                                })
                                .child("取消"),
                        )
                        .child(
                            div()
                                .id("delete-modal-confirm")
                                .test_support()
                                .rounded_xl()
                                .bg(rgb(0xdc2626))
                                .text_color(rgb(0xffffff))
                                .px(px(14.))
                                .py(px(6.))
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0xb91c1c)))
                                .on_click(move |_, _, cx| {
                                    let _ = this_confirm
                                        .update(cx, |w, cx| w.confirm_delete_host(cx));
                                })
                                .child("确认删除"),
                        ),
                ),
        )
        .into_any_element()
}

fn field_label(text: &'static str, palette: &Palette) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(palette.text_secondary)
        .child(text)
}

fn checkbox_toggle(
    id: &'static str,
    label: &'static str,
    checked: bool,
    palette: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let box_el = div()
        .size(px(16.))
        .rounded(px(4.))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.));
    let box_el = if checked {
        box_el
            .border_color(palette.primary_bg)
            .bg(palette.primary_bg)
            .text_color(palette.primary_fg)
            .child("✓")
    } else {
        box_el
            .border_color(palette.border)
            .bg(palette.card_solid_bg)
    };
    h_flex()
        .id(id)
        .test_support()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .text_color(palette.text_secondary)
        .cursor_pointer()
        .on_click(on_click)
        .child(box_el)
        .child(label)
}
