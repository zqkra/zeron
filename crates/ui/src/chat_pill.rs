//! A chat reference as a compact inline chip: harness brand mark, live title,
//! live status glyph. Shared by the transcript's spawn chips and child-update
//! cards — resolution always happens at render time (rows are prepared off
//! the UI thread), so a rename or a status flip is a repaint, not a rebuild.

use std::hash::{Hash, Hasher};

use chrono::Utc;
use gpui::{AnyElement, EntityId, SharedString, div, prelude::*, px};

use zeron_proto::{ChatIndicator, HarnessId};

use crate::icons::{self, icon};
use crate::state::AppState;
use crate::theme::Theme;

/// A chat resolved for display. `known == false` means the id has no registry
/// row — the chip renders the placeholder rather than guessing.
#[derive(Debug, Clone)]
pub(crate) struct ChatRef {
    pub chat_id: String,
    pub title: SharedString,
    pub harness: Option<HarnessId>,
    pub model: Option<SharedString>,
    pub indicator: ChatIndicator,
    pub known: bool,
}

impl ChatRef {
    /// Resolve by full chat id only. Unknown ids degrade to a muted
    /// "Unavailable chat" chip that never opens anything.
    pub(crate) fn resolve(state: &AppState, chat_id: &str) -> ChatRef {
        match state.chats.iter().find(|chat| chat.id == chat_id) {
            Some(chat) => ChatRef {
                chat_id: chat.id.clone(),
                title: chat_title(chat),
                harness: chat.config.as_ref().map(|config| config.harness),
                model: chat
                    .config
                    .as_ref()
                    .and_then(|config| config.model.clone())
                    .map(SharedString::from),
                indicator: state.display_status_for(chat, Utc::now()),
                known: true,
            },
            None => ChatRef {
                chat_id: chat_id.to_owned(),
                title: "Unavailable chat".into(),
                harness: None,
                model: None,
                indicator: ChatIndicator::Idle,
                known: false,
            },
        }
    }

    /// Resolve a CLI-style reference — full id, unique id prefix, or exact
    /// title — the same inputs `zeron chat` accepts. `None` when nothing
    /// matches, so callers can fall back to the raw text.
    pub(crate) fn resolve_target(state: &AppState, target: &str) -> Option<ChatRef> {
        let target = target.trim();
        if target.is_empty() {
            return None;
        }
        if let Some(chat) = state.chats.iter().find(|chat| chat.id == target) {
            return Some(Self::resolve(state, &chat.id));
        }
        let mut prefix = state
            .chats
            .iter()
            .filter(|chat| chat.id.starts_with(target));
        // A unique prefix resolves; zero or several matches fall through to
        // the exact-title check.
        if let (Some(chat), None) = (prefix.next(), prefix.next()) {
            return Some(Self::resolve(state, &chat.id));
        }
        state
            .chats
            .iter()
            .find(|chat| chat.title.as_deref() == Some(target))
            .map(|chat| Self::resolve(state, &chat.id))
    }
}

/// An untitled chat reads like the sidebar's rows ("New session"); the
/// message preview stands in when a title hasn't been generated yet.
fn chat_title(chat: &zeron_proto::Chat) -> SharedString {
    chat.title
        .clone()
        .or_else(|| chat.last_message_preview.clone())
        .unwrap_or_else(|| "New session".into())
        .into()
}

/// Hash the resolved identity of every referenced chat: title, harness, model
/// and display indicator — the same inputs [`crate::chat_activity::fingerprint`]
/// tracks, so a chip repaints exactly when the menu row would. References may
/// be full ids, unique prefixes or exact titles (CLI-style), so resolution
/// goes through [`ChatRef::resolve_target`].
pub(crate) fn chat_refs_fingerprint<'a>(
    state: &AppState,
    ids: impl IntoIterator<Item = &'a str>,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for id in ids {
        id.hash(&mut hasher);
        match ChatRef::resolve_target(state, id) {
            Some(chat) => {
                chat.chat_id.hash(&mut hasher);
                chat.title.as_ref().hash(&mut hasher);
                chat.harness.map(|h| h as u8).hash(&mut hasher);
                chat.model
                    .as_ref()
                    .map(SharedString::as_str)
                    .hash(&mut hasher);
                (chat.indicator as u8).hash(&mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
    }
    hasher.finish()
}

/// The chip itself: 12px harness mark (BOT until the harness is known), a
/// truncating title, and the sidebar's status glyph. Sized to ride inline in
/// markdown text — the inline `@chat:` mention pills render with it; tool
/// rows deliberately do NOT (they keep the native chip look).
/// It is a label, not a button; callers that want a click wrap it.
#[allow(dead_code)]
pub(crate) fn chat_chip(
    chat: &ChatRef,
    theme: &Theme,
    view: EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    let (mark, tint) = chat
        .harness
        .map(crate::pickers::harness_brand_icon)
        .unwrap_or((icons::BOT, None));
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(5.0))
        .child(
            icon(mark)
                .size(px(12.0))
                .flex_none()
                .text_color(tint.unwrap_or(theme.text_muted)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(if chat.known {
                    theme.text
                } else {
                    theme.text_muted
                })
                .child(chat.title.clone()),
        )
        .when(chat.known, |chip| {
            chip.child(status_glyph(
                format!("chat-pill-{}", chat.chat_id),
                chat.indicator,
                view,
                theme,
                cx,
            ))
        })
        .into_any_element()
}

/// The status slot every chat row ends in: spinner while working, check when
/// finished-unseen, a plain status dot otherwise. Shared by the activity
/// menu, the transcript's chat chips and the child-update cards.
pub(crate) fn status_glyph(
    key: String,
    status: ChatIndicator,
    view: EntityId,
    theme: &Theme,
    cx: &mut gpui::App,
) -> AnyElement {
    let color = crate::shell::spaces::status_dot_color(status, theme);
    let glyph: AnyElement = match status {
        ChatIndicator::Completed => icon(icons::CHECK)
            .size(px(11.0))
            .flex_none()
            .text_color(color)
            .into_any_element(),
        ChatIndicator::Working => {
            crate::loaders::mini_glyph_spinner(format!("{key}-working"), 2.0, theme.glyph, view, cx)
                .into_any_element()
        }
        _ => div()
            .size(px(6.0))
            .flex_none()
            .rounded_full()
            .bg(color)
            .into_any_element(),
    };
    div()
        .flex_none()
        .size(px(13.0))
        .flex()
        .items_center()
        .justify_center()
        .child(glyph)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::Chat;

    fn chat(id: &str, title: Option<&str>) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": "dev",
            "archived": false,
            "createdAt": Utc::now(),
            "title": title,
            "config": {
                "harness": "claude-code",
                "model": "opus",
                "sandbox": "workspace-write"
            },
        }))
        .unwrap()
    }

    #[test]
    fn resolve_marks_unknown_and_untitled_chats() {
        let mut state = AppState::new();
        state.chats.push(chat("a1b2c3", None));
        let missing = ChatRef::resolve(&state, "nope");
        assert!(!missing.known);
        assert_eq!(missing.title.as_ref(), "Unavailable chat");
        let untitled = ChatRef::resolve(&state, "a1b2c3");
        assert!(untitled.known);
        assert_eq!(untitled.title.as_ref(), "New session");
        assert_eq!(untitled.harness, Some(HarnessId::ClaudeCode));
        assert_eq!(untitled.model.as_deref(), Some("opus"));
    }

    #[test]
    fn resolve_target_accepts_full_id_unique_prefix_and_exact_title() {
        let mut state = AppState::new();
        let mut named = chat("3f6b2a18-x", None);
        named.id = "3f6b2a18-9c4d".into();
        named.title = Some("Refactor".into());
        state.chats.push(named);
        state.chats.push(chat("3f6b2a18-y", None));
        state.chats.last_mut().unwrap().id = "3f6b2a18-zzzz".into();
        assert_eq!(
            ChatRef::resolve_target(&state, "3f6b2a18-9c4d").map(|r| r.title.to_string()),
            Some("Refactor".into())
        );
        assert_eq!(
            ChatRef::resolve_target(&state, "Refactor").map(|r| r.chat_id),
            Some("3f6b2a18-9c4d".into())
        );
        // An ambiguous prefix resolves nothing (the raw text renders muted).
        assert!(ChatRef::resolve_target(&state, "3f6b2a18").is_none());
        assert!(ChatRef::resolve_target(&state, "nope").is_none());
        assert!(ChatRef::resolve_target(&state, "  ").is_none());
    }

    #[test]
    fn fingerprint_tracks_title_and_live_status() {
        let mut state = AppState::new();
        state.chats.push(chat("c1", Some("Child")));
        let base = chat_refs_fingerprint(&state, ["c1"]);
        assert_eq!(base, chat_refs_fingerprint(&state, ["c1"]));
        state.chats[0].title = Some("Renamed".into());
        let renamed = chat_refs_fingerprint(&state, ["c1"]);
        assert_ne!(base, renamed);
        // A send in flight reads as Working — the same flip the live chip shows.
        state.begin_pending_send("c1", "m1", Utc::now());
        assert_ne!(renamed, chat_refs_fingerprint(&state, ["c1"]));
        // Membership changes fingerprint too.
        assert_ne!(renamed, chat_refs_fingerprint(&state, ["c1", "other"]));
    }
}
