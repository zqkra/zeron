//! Durable parent notifications (spec D9): when an agent-spawned child chat
//! settles — completes, errors, is interrupted, or parks on a question — the
//! engine hosting its PARENT writes a `role: System` ChildUpdate card into
//! the parent's doc and delivers one combined agent-only prompt, exactly
//! once, without ever interrupting the parent's turn.
//!
//! Exactly-once is built in two layers, both keyed on the stable turn key
//! from [`zeron_proto::orchestration::child_update`]:
//!
//! 1. the `child_notifications` ledger in [`DocsStore`] — claim-before-send
//!    (`INSERT OR IGNORE`), so a restart between detection and delivery
//!    re-arms from `pending` rows instead of double-sending, and an
//!    `AckChildUpdates` row that beat the claim suppresses it entirely;
//! 2. the card's fixed entry id `child:<childId>:<turnKey>` — the doc append
//!    is check-before-write, so a crash between card write and `delivered`
//!    stamp can never duplicate the visible card either.
//!
//! The notifier watches the MERGED session view ([`WorkspaceHost::
//! merged_sessions_watch`]): registry session rows of every device overlaid
//! with this engine's in-memory truth. Children can live anywhere — the
//! registry rows are the cross-device signal — while a local child still
//! reports its settle before its registry row round-trips. Chat rows come
//! from `watch_chats`, which is also what wakes the loop when a parent is
//! archived, unarchived, or re-homed here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_proto::entities::{Chat, Session, SessionStatus};
use zeron_proto::orchestration::{ChildOutcome, ChildUpdate, child_update};
use zeron_sync::DocsStore;

use crate::doc_host::DocHost;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;

/// Post-claim quiet window: settles that land within it coalesce into the
/// ONE prompt a flush delivers (two children finishing a second apart read
/// as a single "Child chat updates" message).
const FLUSH_DEBOUNCE: Duration = Duration::from_secs(2);

/// Delivery failure backoff: 1 s, 5 s, 30 s, then every 5 minutes — a dead
/// doc-host path must never loop hot.
const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];
const RETRY_CAP: Duration = Duration::from_secs(300);

/// Re-check cadence for a parent that is not currently ours to notify
/// (missing row, or hosted on another device). Pending rows wait for the
/// chat rows to make the parent local — this is only the belt to that
/// suspenders.
const NOT_LOCAL_RECHECK: Duration = Duration::from_secs(60);

/// The prompt budget for a completed child's last reply.
const EXCERPT_PROMPT_LIMIT: usize = 4000;
/// The card's excerpt budget (the part, not the prompt).
const EXCERPT_CARD_LIMIT: usize = 600;
/// Needs-help detail block: up to 4 lines and 700 chars.
const BLOCKER_MAX_LINES: usize = 4;
const BLOCKER_MAX_CHARS: usize = 700;
const TRUNCATED_SUFFIX: &str = "\n\n[... output truncated ...]";

/// Spawn the notifier loop on the doc host's worker tracker (engine
/// assembly). All state lives inside the task: `last_seen` session rows for
/// edge detection, `due` per-parent flush times, `attempts` per-parent
/// delivery backoff, and `awaiting` — updates detected before the child's
/// (or parent's) chat row was visible, re-evaluated on every wake so a
/// registry-row ordering race can't lose a notification.
pub(crate) fn start(
    doc_host: &DocHost,
    workspace: &WorkspaceHost,
    sessions: &SessionsEngine,
    store: Arc<DocsStore>,
    device_id: String,
) {
    // Bare sync contexts (unit-test assembly without a runtime) get no
    // notifier rather than a spawn panic — same guard as DocHost::new.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let host = doc_host.clone();
    let workspace = workspace.clone();
    let sessions = sessions.clone();
    doc_host.spawn_worker(async move {
        let doc_host = host;
        let mut sessions_rx = workspace.merged_sessions_watch(sessions.watch_sessions());
        let mut chats_rx = workspace.watch_chats();
        let mut last_seen: HashMap<String, Session> = HashMap::new();
        let mut due: HashMap<String, Instant> = HashMap::new();
        let mut attempts: HashMap<String, u32> = HashMap::new();
        let mut awaiting: HashMap<String, ChildUpdate> = HashMap::new();
        // Restart re-arm: rows still pending from before this boot flush on
        // the same debounce as a fresh claim.
        for parent in store
            .pending_child_notification_parents()
            .unwrap_or_default()
        {
            due.insert(parent, Instant::now() + FLUSH_DEBOUNCE);
        }
        loop {
            let session_rows = sessions_rx.borrow_and_update().clone();
            let chat_rows = chats_rx.borrow_and_update().clone();

            detect(
                &session_rows,
                &chat_rows,
                &store,
                &device_id,
                &mut last_seen,
                &mut awaiting,
                &mut due,
            );
            flush_due(
                &FlushCtx {
                    doc_host: &doc_host,
                    workspace: &workspace,
                    sessions: &sessions,
                    store: &store,
                    device_id: &device_id,
                    session_rows: &session_rows,
                },
                &mut due,
                &mut attempts,
            )
            .await;
            let next = due.values().min().copied();
            let wait = async move {
                match next {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                changed = sessions_rx.changed() => if changed.is_err() { break },
                changed = chats_rx.changed() => if changed.is_err() { break },
                _ = wait => {}
            }
        }
    });
}

/// Diff the merged session rows against the last observation; claim every
/// detected settle for eligible children and schedule the parent's flush.
/// `awaiting` holds updates that fired while the child's eligibility was not
/// yet decidable (chat row or parent row not yet synced) — they claim on a
/// later wake instead of re-detecting.
fn detect(
    sessions: &[Session],
    chats: &[Chat],
    store: &DocsStore,
    device_id: &str,
    last_seen: &mut HashMap<String, Session>,
    awaiting: &mut HashMap<String, ChildUpdate>,
    due: &mut HashMap<String, Instant>,
) {
    let chats_by_id: HashMap<&str, &Chat> = chats.iter().map(|c| (c.id.as_str(), c)).collect();
    let present: HashSet<&str> = sessions.iter().map(|s| s.chat_id.as_str()).collect();
    last_seen.retain(|id, _| present.contains(id.as_str()));

    let eligible = |child_id: &str| -> Option<String> {
        let child = chats_by_id.get(child_id)?;
        if !child.spawned_by_agent {
            return None;
        }
        let parent_id = child.parent_chat_id.as_deref()?;
        let parent = chats_by_id.get(parent_id)?;
        (parent.device_id == device_id).then(|| parent_id.to_string())
    };

    // First: retry updates parked on rows that were not visible yet.
    let parked: Vec<(String, ChildUpdate)> = awaiting
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (child_id, update) in parked {
        match eligible(&child_id) {
            Some(parent_id) => {
                awaiting.remove(&child_id);
                claim(store, &child_id, &update, &parent_id, due);
            }
            // Once the child exists and is plainly not ours to report
            // (non-agent child, remote parent), stop re-checking it.
            None if chats_by_id.contains_key(child_id.as_str()) => {
                awaiting.remove(&child_id);
            }
            None => {}
        }
    }

    for next in sessions {
        let prev = last_seen.get(&next.chat_id);
        let Some(update) = child_update(prev, next) else {
            last_seen.insert(next.chat_id.clone(), next.clone());
            continue;
        };
        last_seen.insert(next.chat_id.clone(), next.clone());
        match eligible(&next.chat_id) {
            Some(parent_id) => claim(store, &next.chat_id, &update, &parent_id, due),
            // Park it when the chat row isn't here yet: spawn and settle
            // routinely race the registry row landing.
            None if !chats_by_id.contains_key(&next.chat_id.as_str()) => {
                awaiting.insert(next.chat_id.clone(), update);
            }
            None => {}
        }
    }
}

/// Claim-before-send: the ledger decides whether this transition still owes
/// the parent a notification (prior pending claim, delivered, or an ack that
/// beat us all read as "not owed"). On a fresh claim the child's older
/// pending keys supersede — only the newest settle per child is delivered —
/// and the parent's flush re-arms at the trailing edge of the debounce.
fn claim(
    store: &DocsStore,
    child_id: &str,
    update: &ChildUpdate,
    parent_id: &str,
    due: &mut HashMap<String, Instant>,
) {
    match store.claim_child_notification(
        child_id,
        &update.key,
        parent_id,
        outcome_name(update.outcome),
    ) {
        Ok(true) => {
            if let Err(err) = store.supersede_pending_child_notifications(child_id, &update.key) {
                tracing::warn!(chat = %child_id, error = %err, "child-notification supersede failed");
            }
            due.insert(parent_id.to_string(), Instant::now() + FLUSH_DEBOUNCE);
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(chat = %child_id, error = %err, "child-notification claim failed");
        }
    }
}

/// The shared context a flush needs — bundled so `flush_due` stays readable.
struct FlushCtx<'a> {
    doc_host: &'a DocHost,
    workspace: &'a WorkspaceHost,
    sessions: &'a SessionsEngine,
    store: &'a DocsStore,
    device_id: &'a str,
    session_rows: &'a [Session],
}

/// Deliver every parent whose flush time arrived. Holds: parent missing or
/// hosted elsewhere (recheck periodically), archived (pending rows drop —
/// sending would unarchive it), or mid-turn (held until the session settles;
/// the merged watch wakes us when it does).
async fn flush_due(
    ctx: &FlushCtx<'_>,
    due: &mut HashMap<String, Instant>,
    attempts: &mut HashMap<String, u32>,
) {
    let doc_host = ctx.doc_host;
    let workspace = ctx.workspace;
    let sessions = ctx.sessions;
    let store = ctx.store;
    let device_id = ctx.device_id;
    let session_rows = ctx.session_rows;
    let now = Instant::now();
    let ready: Vec<String> = due
        .iter()
        .filter(|(_, at)| **at <= now)
        .map(|(p, _)| p.clone())
        .collect();
    for parent_id in ready {
        due.remove(&parent_id);
        let parent = match workspace.chat(&parent_id) {
            Ok(parent) => parent,
            Err(err) => {
                tracing::warn!(chat = %parent_id, error = %err, "child-notification parent read failed");
                due.insert(parent_id, Instant::now() + NOT_LOCAL_RECHECK);
                continue;
            }
        };
        let Some(parent) = parent else {
            due.insert(parent_id, Instant::now() + NOT_LOCAL_RECHECK);
            continue;
        };
        if parent.device_id != device_id {
            // Not ours to notify; rows stay pending for whoever hosts it.
            due.insert(parent_id, Instant::now() + NOT_LOCAL_RECHECK);
            continue;
        }
        if parent.archived {
            if let Err(err) = store.drop_pending_child_notifications(&parent_id) {
                tracing::warn!(chat = %parent_id, error = %err, "child-notification drop failed");
                due.insert(parent_id, Instant::now() + NOT_LOCAL_RECHECK);
            }
            continue;
        }
        // AwaitingInput is still a turn — the parent is owed its answer, not
        // a steer. Idle/Errored/no session are deliverable.
        let in_flight = sessions.turn_in_flight(&parent_id)
            || session_rows
                .iter()
                .find(|s| s.chat_id == parent_id)
                .is_some_and(|s| {
                    matches!(
                        s.status,
                        SessionStatus::Working | SessionStatus::AwaitingInput
                    )
                });
        if in_flight {
            due.insert(parent_id, Instant::now() + FLUSH_DEBOUNCE);
            continue;
        }
        let pending = match store.pending_child_notifications(&parent_id) {
            Ok(pending) => pending,
            Err(err) => {
                tracing::warn!(chat = %parent_id, error = %err, "child-notification read failed");
                due.insert(parent_id, Instant::now() + RETRY_CAP);
                continue;
            }
        };
        if pending.is_empty() {
            attempts.remove(&parent_id);
            continue;
        }
        let mut cards = Vec::with_capacity(pending.len());
        let mut lines = Vec::with_capacity(pending.len());
        for row in &pending {
            cards.push(card_entry(doc_host, workspace, device_id, row));
            lines.push(prompt_line(doc_host, workspace, row));
        }
        let prompt = combined_prompt(&lines);
        match doc_host
            .deliver_child_updates(&parent_id, &cards, &prompt)
            .await
        {
            Ok(()) => {
                attempts.remove(&parent_id);
                for row in &pending {
                    if let Err(err) = store.settle_child_notification(
                        &row.child_chat_id,
                        &row.turn_key,
                        "delivered",
                    ) {
                        tracing::warn!(chat = %parent_id, error = %err, "child-notification settle failed");
                    }
                }
            }
            Err(err) => {
                let attempt = attempts.entry(parent_id.clone()).or_insert(0);
                let wait = RETRY_BACKOFF
                    .get(*attempt as usize)
                    .copied()
                    .unwrap_or(RETRY_CAP);
                *attempt += 1;
                tracing::warn!(chat = %parent_id, error = %err,
                    backoff_ms = wait.as_millis() as u64,
                    "child-notification delivery failed; retrying");
                due.insert(parent_id, Instant::now() + wait);
            }
        }
    }
}

fn outcome_name(outcome: ChildOutcome) -> &'static str {
    match outcome {
        ChildOutcome::Completed => "completed",
        ChildOutcome::Errored => "errored",
        ChildOutcome::Interrupted => "interrupted",
        ChildOutcome::NeedsInput => "needsInput",
    }
}

fn outcome_from_name(name: &str) -> ChildOutcome {
    match name {
        "errored" => ChildOutcome::Errored,
        "interrupted" => ChildOutcome::Interrupted,
        "needsInput" => ChildOutcome::NeedsInput,
        _ => ChildOutcome::Completed,
    }
}

fn chat_title(chat: Option<&Chat>, chat_id: &str) -> String {
    chat.and_then(|c| c.title.clone())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            // Untitled chats label by their id prefix, like agent_message.
            chat_id.chars().take(8).collect()
        })
}

/// Truncate at a char boundary to at most `max` chars; adds the truncation
/// suffix when anything was cut.
fn cap_chars(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut = trimmed
        .char_indices()
        .nth(max)
        .map_or(trimmed.len(), |(i, _)| i);
    format!("{}{}", trimmed[..cut].trim_end(), TRUNCATED_SUFFIX)
}

/// The child's last assistant reply: text parts of the latest settled
/// (Complete, non-empty) assistant entry in the child's local doc.
fn last_assistant_reply(doc_host: &DocHost, child_chat_id: &str) -> Option<String> {
    let handle = doc_host.open_local(child_chat_id).ok()?;
    let entries = handle.doc().read_entries().ok()?;
    let entry = entries.iter().rev().find(|e| {
        e.role == MessageRole::Assistant
            && e.status == Some(MessageStatus::Complete)
            && e.parts.iter().any(|p| match p {
                MessagePart::Text { text, .. } => !text.trim().is_empty(),
                _ => false,
            })
    })?;
    let text = entry
        .parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

/// The child's still-open input request: labels + detail lines for the
/// "needs help" prompt, read from the child's local doc.
fn pending_input_details(doc_host: &DocHost, child_chat_id: &str) -> Option<(String, String)> {
    let handle = doc_host.open_local(child_chat_id).ok()?;
    let entries = handle.doc().read_entries().ok()?;
    let questions = entries.iter().rev().find_map(|e| {
        e.parts.iter().find_map(|p| match p {
            MessagePart::Input {
                questions,
                resolved: false,
                ..
            } => Some(questions.clone()),
            _ => None,
        })
    })?;
    let first = questions.first()?;
    let label = if !first.header.trim().is_empty() {
        first.header.trim().to_string()
    } else {
        first.question.trim().to_string()
    };
    let mut lines: Vec<String> = Vec::new();
    for q in &questions {
        if !q.question.trim().is_empty() {
            lines.push(q.question.trim().to_string());
        }
        if !q.options.is_empty() {
            lines.push(format!("options: {}", q.options.join(", ")));
        }
        if lines.len() >= BLOCKER_MAX_LINES {
            break;
        }
    }
    lines.truncate(BLOCKER_MAX_LINES);
    Some((label, cap_chars(&lines.join("\n"), BLOCKER_MAX_CHARS)))
}

/// The excerpt stored on the card AND expanded in the prompt body for a
/// completed child: the doc's last reply when the doc is readable here, else
/// the registry preview with a pointer at `zeron chat output`.
fn completion_excerpt(
    doc_host: &DocHost,
    workspace: &WorkspaceHost,
    child_chat_id: &str,
) -> Option<String> {
    if let Some(reply) = last_assistant_reply(doc_host, child_chat_id) {
        return Some(cap_chars(&reply, EXCERPT_PROMPT_LIMIT));
    }
    let preview = workspace
        .chat(child_chat_id)
        .ok()
        .flatten()
        .and_then(|c| c.last_message_preview)
        .filter(|p| !p.trim().is_empty());
    preview.map(|preview| {
        let id8: String = child_chat_id.chars().take(8).collect();
        format!(
            "{}\n\nRead it with `zeron chat output {}`.",
            preview.trim(),
            id8
        )
    })
}

/// What the ChildUpdate card carries: the prompt's own excerpt, capped for
/// display — the completed child's reply, or the blocker a needs-help child
/// parked on. Failed/interrupted updates have no excerpt.
fn card_excerpt(
    doc_host: &DocHost,
    workspace: &WorkspaceHost,
    row: &zeron_sync::ChildNotification,
) -> Option<String> {
    match outcome_from_name(row.outcome.as_deref().unwrap_or("")) {
        ChildOutcome::Completed => completion_excerpt(doc_host, workspace, &row.child_chat_id)
            .map(|e| cap_chars(&e, EXCERPT_CARD_LIMIT)),
        ChildOutcome::NeedsInput => {
            pending_input_details(doc_host, &row.child_chat_id).map(|(label, detail)| {
                cap_chars(
                    &format!("Blocked on {label}:\n{detail}"),
                    EXCERPT_CARD_LIMIT,
                )
            })
        }
        ChildOutcome::Errored | ChildOutcome::Interrupted => None,
    }
}

/// The `role: System` entry for one delivered update — fixed id so the
/// append is idempotent across delivery retries.
fn card_entry(
    doc_host: &DocHost,
    workspace: &WorkspaceHost,
    device_id: &str,
    row: &zeron_sync::ChildNotification,
) -> SessionMessageEntry {
    let child = workspace.chat(&row.child_chat_id).ok().flatten();
    SessionMessageEntry {
        duration_ms: None,
        id: format!("child:{}:{}", row.child_chat_id, row.turn_key),
        role: MessageRole::System,
        parts: vec![MessagePart::ChildUpdate {
            id: "cu0".into(),
            child_chat_id: row.child_chat_id.clone(),
            child_title: chat_title(child.as_ref(), &row.child_chat_id),
            outcome: outcome_from_name(row.outcome.as_deref().unwrap_or("")),
            excerpt: card_excerpt(doc_host, workspace, row),
        }],
        created_at: crate::now_ms(),
        device_id: device_id.to_string(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    }
}

enum PromptLine {
    Completed(String, Option<String>),
    Failed(String),
    Interrupted(String),
    NeedsHelp(String, Option<(String, String)>),
}

/// Build the per-update prompt content (kept as an enum so the single-update
/// and batch templates share the same classification).
fn prompt_line(
    doc_host: &DocHost,
    workspace: &WorkspaceHost,
    row: &zeron_sync::ChildNotification,
) -> PromptLine {
    let mention = format!("@chat:{}", row.child_chat_id);
    match outcome_from_name(row.outcome.as_deref().unwrap_or("")) {
        ChildOutcome::Completed => PromptLine::Completed(
            mention,
            completion_excerpt(doc_host, workspace, &row.child_chat_id),
        ),
        ChildOutcome::Errored => PromptLine::Failed(mention),
        ChildOutcome::Interrupted => PromptLine::Interrupted(mention),
        ChildOutcome::NeedsInput => {
            PromptLine::NeedsHelp(mention, pending_input_details(doc_host, &row.child_chat_id))
        }
    }
}

/// The full agent-only prompt: `[Zeron system]` + the D9 template. A single
/// update expands inline; several collapse to the batch bullet list.
fn combined_prompt(lines: &[PromptLine]) -> String {
    let body = match lines {
        [PromptLine::Completed(mention, excerpt)] => format!(
            "{mention} completed:\n\n{}",
            excerpt
                .as_deref()
                .unwrap_or("No final output was recorded.")
        ),
        [PromptLine::Failed(mention)] => {
            format!("{mention} failed.\n\nReview the chat before deciding next steps.")
        }
        [PromptLine::Interrupted(mention)] => format!(
            "{mention} was interrupted.\n\nReview the chat before deciding next steps.\n\nIf the user stopped it manually, do not resume, restart, retry, replace, or continue the work unless the user explicitly asks."
        ),
        [PromptLine::NeedsHelp(mention, details)] => {
            let blocked = details
                .as_ref()
                .map(|(label, detail)| format!("Blocked on {label}:\n{detail}"))
                .unwrap_or_else(|| "Blocked on input.".to_string());
            format!(
                "{mention} needs help.\n{blocked}\n\nReview the blocker. If you can resolve it from existing context, reply to the chat with guidance. Otherwise, ask the user for the missing decision."
            )
        }
        _ => {
            let mut bullets = Vec::with_capacity(lines.len());
            let mut any_interrupted = false;
            for line in lines {
                bullets.push(match line {
                    PromptLine::Completed(m, _) => format!("- {m} completed."),
                    PromptLine::Failed(m) => format!("- {m} failed."),
                    PromptLine::Interrupted(m) => {
                        any_interrupted = true;
                        format!("- {m} was interrupted.")
                    }
                    PromptLine::NeedsHelp(m, _) => format!("- {m} needs help."),
                });
            }
            let mut body = format!("Child chat updates:\n\n{}", bullets.join("\n"));
            if any_interrupted {
                body.push_str("\n\nIf the user stopped a chat manually, do not resume, restart, retry, replace, or continue its work unless the user explicitly asks.");
            }
            body
        }
    };
    format!("[Zeron system]\n\n{body}")
}
