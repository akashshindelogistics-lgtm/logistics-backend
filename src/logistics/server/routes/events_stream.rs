//! Server-Sent Events feed of an org's events, so open dashboards refresh
//! without a reload. See `docs/async-events.md`.

use super::common::*;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::events::queue::{self, Event, now_unix};
use actix_web::{HttpResponse, Responder, get, web};
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// A comment line this often keeps proxies from closing an idle stream.
const KEEPALIVE_EVERY: u32 = 7;
const BATCH: u32 = 100;
/// Each poll re-reads this many seconds before the newest event seen.
/// Ids are random and `created_at` is whole seconds, so a later event can
/// sort before one already sent in the same second; an event can also
/// commit after a poll that already passed its second. `seen` skips the
/// re-read events that were already sent.
const LOOKBACK_SECS: i64 = 5;

/// One SSE frame: `event` is the event type, `data` its JSON payload.
fn frame(event: &Event) -> String {
    format!(
        "id: {}\nevent: {}\ndata: {}\n\n",
        event.id, event.event_type, event.payload
    )
}

fn fetch(org_id: Uuid, since: i64, after_id: &str) -> Result<Vec<Event>, String> {
    let mut conn = DbConnection::from_env()
        .get_connection()
        .map_err(|e| e.to_string())?;
    queue::ensure_tables(&mut conn).map_err(|e| e.to_string())?;
    queue::list_for_org_after(&mut conn, org_id, since, after_id, BATCH).map_err(|e| e.to_string())
}

struct StreamState {
    org_id: Uuid,
    /// The connect second: nothing older is sent.
    floor: i64,
    /// The newest `created_at` read so far; each poll starts
    /// `LOOKBACK_SECS` before it.
    newest: i64,
    /// Keyset cursor `(created_at, id)` for paging within one poll. An
    /// empty `last_id` includes every event at second `since`.
    since: i64,
    last_id: String,
    /// Ids already sent, with their `created_at`. Pruned to the look-back
    /// window, so it holds only a few seconds of events.
    seen: HashMap<Uuid, i64>,
    /// The token's expiry; the stream ends once it passes, so the client
    /// reconnects with a fresh token.
    exp: i64,
    /// The last page was full, so read the next page without waiting.
    more: bool,
    idle_polls: u32,
    first: bool,
}

#[utoipa::path(
    get,
    path = "/api/events/stream",
    tag = "Events",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "text/event-stream of the org's new events (`event:` is the type, `data:` the JSON payload). Only events from after connecting are sent. The stream ends when the token expires; reconnect with a fresh one.", content_type = "text/event-stream", body = String),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/events/stream")]
pub async fn stream_events(auth: AuthenticatedOrg) -> impl Responder {
    let connected_at = now_unix();
    let state = StreamState {
        org_id: auth.org_id,
        floor: connected_at,
        newest: connected_at,
        since: connected_at,
        last_id: String::new(),
        seen: HashMap::new(),
        exp: auth.exp,
        more: false,
        idle_polls: 0,
        first: true,
    };
    let body = futures_util::stream::unfold(state, |mut st| async move {
        // Tell the client the stream is open right away.
        if st.first {
            st.first = false;
            return Some((
                Ok::<_, actix_web::Error>(web::Bytes::from_static(b": connected\n\n")),
                st,
            ));
        }
        loop {
            if now_unix() >= st.exp {
                return None;
            }
            if !st.more {
                tokio::time::sleep(POLL_INTERVAL).await;
                if now_unix() >= st.exp {
                    return None;
                }
                // A new poll: start back from the newest second, so events
                // that sort or commit late are still read.
                st.since = st.floor.max(st.newest - LOOKBACK_SECS);
                st.last_id.clear();
                let since = st.since;
                st.seen.retain(|_, created_at| *created_at >= since);
            }
            // Inline like the other handlers: one small indexed query.
            let events = fetch(st.org_id, st.since, &st.last_id).unwrap_or_else(|err| {
                eprintln!("events stream: {err}");
                Vec::new()
            });
            st.more = events.len() >= BATCH as usize;
            if let Some(last) = events.last() {
                st.since = last.created_at;
                st.last_id = last.id.to_string();
                st.newest = st.newest.max(last.created_at);
            }
            let mut out = String::new();
            for event in &events {
                if st.seen.insert(event.id, event.created_at).is_none() {
                    out.push_str(&frame(event));
                }
            }
            if out.is_empty() {
                if st.more {
                    continue;
                }
                st.idle_polls += 1;
                if st.idle_polls >= KEEPALIVE_EVERY {
                    st.idle_polls = 0;
                    return Some((Ok(web::Bytes::from_static(b": keepalive\n\n")), st));
                }
                continue;
            }
            st.idle_polls = 0;
            return Some((Ok(web::Bytes::from(out)), st));
        }
    });
    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(body)
}
