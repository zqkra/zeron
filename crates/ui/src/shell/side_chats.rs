use super::*;

pub(super) struct SideChatTab {
    pub state: Entity<AppState>,
    transcript: Entity<Transcript>,
    pub(super) composer: Entity<Composer>,
    _events: Vec<Subscription>,
}

impl Shell {
    pub(super) fn close_empty_right_pane(&mut self, key: &str, cx: &mut Context<Self>) {
        let empty = if key == self.panel_key(cx) {
            self.right_surface_rows(cx).is_empty()
        } else {
            self.right_tabs.get(key).is_none_or(Vec::is_empty)
        };
        if empty {
            if key == self.panel_key(cx) && self.right_pane_open(cx) {
                self.toggle_right_pane(cx);
            } else {
                self.panels.update(key, |p| p.changes_open = false);
            }
        }
    }

    /// The surface picker's "Side chat": fork the current conversation
    /// through its latest completed response, as a child of it.
    pub(super) fn create_side_chat(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.state.read(cx).selected_chat_row().cloned() else {
            self.side_chat_error = Some("Start a conversation before creating a side chat.".into());
            cx.notify();
            return;
        };
        let parent = source.id.clone();
        self.fork_chat(source, parent, cx);
    }

    /// Fork `source` through its latest completed response into a new chat
    /// hanging under `parent_id`, and open it in the right pane. A side
    /// chat's own fork button passes its parent so the copy lists as a
    /// sibling; the picker passes the source itself.
    pub(super) fn fork_chat(
        &mut self,
        source: zeron_proto::Chat,
        parent_id: String,
        cx: &mut Context<Self>,
    ) {
        if self.side_chat_creating {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self.panel_key(cx);
        self.side_chat_creating = true;
        self.side_chat_error = None;
        let params = serde_json::json!({
            "chatId": uuid::Uuid::new_v4().to_string(),
            "sourceChatId": source.id,
            "parentChatId": parent_id,
            "targetDeviceId": source.device_id,
        });
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<zeron_proto::Chat>(methods::FORK_SIDE_CHAT, params)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.side_chat_creating = false;
                match result {
                    Ok(chat) => {
                        if this
                            .state
                            .read(cx)
                            .chats
                            .iter()
                            .any(|c| Some(&c.id) == chat.parent_chat_id.as_ref())
                        {
                            this.open_side_chat(chat, key, cx);
                        }
                    }
                    Err(error) => this.side_chat_error = Some(error.to_string().into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A fresh, empty side chat under `parent_id` (the active chat when
    /// None), opened in the right pane ready for its first message. Same
    /// shape the Zeron MCP server's `create_chat` mints, so agent-spawned
    /// and hand-started side chats list together.
    pub(super) fn create_child_chat(&mut self, parent_id: Option<String>, cx: &mut Context<Self>) {
        if self.side_chat_creating {
            return;
        }
        let state = self.state.read(cx);
        let parent = match parent_id {
            Some(id) => state.chats.iter().find(|c| c.id == id).cloned(),
            None => state.selected_chat_row().cloned(),
        };
        let Some(parent) = parent else {
            self.side_chat_error = Some("Start a conversation before creating a side chat.".into());
            cx.notify();
            return;
        };
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let key = self.panel_key(cx);
        let mut chat = parent.clone();
        chat.id = uuid::Uuid::new_v4().to_string();
        chat.parent_chat_id = Some(parent.id.clone());
        chat.title = None;
        chat.archived = false;
        chat.created_at = Utc::now();
        chat.last_message_at = None;
        chat.last_message_preview = None;
        chat.last_seen_at = None;
        chat.harness_session_id = None;
        chat.harness_session_cwd = None;
        chat.room_gen = Some(2);
        let params = serde_json::json!({
            "op": "createChat",
            "chatId": chat.id,
            "deviceId": chat.device_id,
            "spaceId": chat.space_id,
            "config": chat.config,
            "branch": chat.branch,
            "cwd": chat.cwd,
            "parentChatId": parent.id,
        });
        self.side_chat_creating = true;
        self.side_chat_error = None;
        cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            let _ = this.update(cx, |this, cx| {
                this.side_chat_creating = false;
                match result {
                    Ok(_) => this.open_side_chat(chat, key, cx),
                    Err(error) => this.side_chat_error = Some(error.to_string().into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Open an existing side chat (a footer row) in the right pane.
    pub(super) fn open_child_chat_tab(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .cloned()
        else {
            return;
        };
        let key = self.panel_key(cx);
        self.open_side_chat(chat, key, cx);
    }

    /// Reveal the surface host of the pane keyed `key`: the live one through
    /// the user-visible path, a background conversation's by flag, so a
    /// fork that finishes after the user switched away opens ITS pane, not
    /// whichever is on screen.
    fn open_surfaces_for(&mut self, key: &str, cx: &mut Context<Self>) {
        if key == self.panel_key(cx) {
            // Programmatic, so an already-open pane is left alone.
            self.set_surfaces_open(true, cx);
        } else {
            self.panels.update(key, |p| p.changes_open = true);
        }
    }

    pub(super) fn open_side_chat(
        &mut self,
        chat: zeron_proto::Chat,
        key: String,
        cx: &mut Context<Self>,
    ) {
        // A footer row or header button may land while the surface host is
        // closed (explorer-only pane, or hidden with its tabs kept): open it
        // beside the explorer first, for an existing tab too.
        self.open_surfaces_for(&key, cx);
        if let Some((&id, _)) = self
            .side_chats
            .iter()
            .find(|(_, tab)| tab.state.read(cx).selected_chat.as_deref() == Some(&chat.id))
        {
            if key == self.panel_key(cx) {
                self.set_right_active(RightSurface::SideChat(id), cx);
            } else {
                self.panels
                    .update(&key, |p| p.right_active = RightSurface::SideChat(id));
            }
            cx.notify();
            return;
        }
        let chat_id = chat.id.clone();
        let parent = self.state.clone();
        let state = cx.new(|cx| AppState::side_chat_state(&parent, chat, cx));
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        // Workspace file links open the editor and web links honor the
        // in-app preference, resolved in this side chat's context.
        let links = Self::session_links(Some(chat_id), cx);
        transcript.update(cx, |transcript, _| {
            transcript.set_workspace_link_handler(links)
        });
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        let events = vec![
            cx.subscribe(&transcript, Self::on_transcript_event),
            cx.subscribe(&composer, {
                let transcript = transcript.clone();
                move |this: &mut Self, _, event, cx| {
                    match event {
                        ComposerEvent::WorkspaceCommand(command) => {
                            this.pending_workspace_command = Some(*command);
                            cx.notify();
                        }
                        // A side chat is already minted before its composer mounts,
                        // and it inherits its parent's checkout, so it never runs
                        // worktree setup of its own.
                        ComposerEvent::NewThreadTransitionStarted
                        | ComposerEvent::WorktreeSetup { .. } => {}
                        ComposerEvent::Sent {
                            chat_id,
                            message_id,
                        } => transcript.update(cx, |t, cx| {
                            t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                        }),
                        ComposerEvent::Queued {
                            chat_id,
                            message_id,
                        } => transcript.update(cx, |t, cx| {
                            t.on_own_queued_send(chat_id.clone(), message_id.clone(), cx)
                        }),
                    }
                }
            }),
            cx.observe(&state, |this, state, cx| {
                if state.read(cx).selected_chat.is_none() {
                    this.remove_deleted_side_chats(cx);
                }
                cx.notify();
            }),
        ];
        self.side_chat_seq += 1;
        let id = self.side_chat_seq;
        self.side_chats.insert(
            id,
            SideChatTab {
                state,
                transcript,
                composer,
                _events: events,
            },
        );
        self.right_tabs
            .entry(key.clone())
            .or_default()
            .push(RightSurface::SideChat(id));
        self.panels
            .update(&key, |p| p.right_active = RightSurface::SideChat(id));
        cx.notify();
    }

    fn remove_deleted_side_chats(&mut self, cx: &mut Context<Self>) {
        let removed: Vec<_> = self
            .side_chats
            .iter()
            .filter(|(_, tab)| tab.state.read(cx).selected_chat.is_none())
            .map(|(&id, _)| id)
            .collect();
        for id in removed {
            self.side_chats.remove(&id);
            let surface = RightSurface::SideChat(id);
            let keys: Vec<_> = self
                .right_tabs
                .iter_mut()
                .filter_map(|(key, tabs)| {
                    let contained = tabs.contains(&surface);
                    tabs.retain(|tab| *tab != surface);
                    contained.then(|| key.clone())
                })
                .collect();
            for key in keys {
                let fallback = self
                    .right_tabs
                    .get(&key)
                    .and_then(|tabs| tabs.first())
                    .copied()
                    .unwrap_or(RightSurface::Picker);
                self.panels.update(&key, |panel| {
                    if panel.right_active == surface {
                        panel.right_active = fallback;
                    }
                });
                self.close_empty_right_pane(&key, cx);
            }
        }
    }

    pub(super) fn render_side_chat(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self
            .side_chats
            .get(&id)
            .filter(|tab| tab.state.read(cx).selected_chat.is_some())
        else {
            return self.render_surface_picker(cx);
        };
        let transcript = tab.transcript.clone();
        let composer = tab.composer.clone();
        // The main composer is driven by the shell's dock (settled, docked)
        // and fed the column width; give the side chat's the same inputs so
        // both take the same height branch.
        let width = self.right_visible_width(cx);
        composer.update(cx, |composer, cx| {
            composer.set_dock_frame(crate::composer_dock::DockFrame::settled(true), cx);
            composer.set_available_width(width, cx);
        });
        let pill = transcript.read(cx).jump_button_shown().then(|| {
            div()
                .absolute()
                .bottom(px(12.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(self.jump_pill(
                    "side-chat-jump",
                    "side-chat-jump-pill",
                    transcript.clone(),
                    cx,
                ))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        crate::edge_fade::edge_faded(
                            Theme::TRANSCRIPT_FADE_BAND,
                            true,
                            false,
                            div().size_full().child(transcript),
                        )
                        .inset_top(Theme::TITLEBAR_HEIGHT),
                    )
                    .children(pill),
            )
            .child(div().flex_none().child(composer))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[gpui::test]
    fn deleted_side_chat_removes_tab_and_closes_empty_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                shell.active_chat = "main".into();
                shell.toggle_right_pane(cx);
                let chat = serde_json::from_value(serde_json::json!({
                    "id": "side", "parentChatId": "main", "deviceId": "local",
                    "archived": false, "createdAt": Utc::now(),
                }))
                .unwrap();
                shell.open_side_chat(chat, shell.panel_key(cx), cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, _, cx| {
                let id = shell.side_chat_seq;
                let side = shell.side_chats[&id].state.clone();
                side.update(cx, |state, cx| state.select_chat(None, cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, _, cx| {
                assert!(shell.side_chats.is_empty());
                assert!(shell.right_surface_rows(cx).is_empty());
                assert!(!shell.right_pane_open(cx));
                assert_eq!(shell.resolved_right_active(cx), RightSurface::Picker);
            })
            .unwrap();
    }
}
