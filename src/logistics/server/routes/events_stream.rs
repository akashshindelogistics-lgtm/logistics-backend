//! Server-Sent Events feed of an org's events, so open dashboards refresh
//! without a reload. See `docs/async-events.md`.

use super::common::*;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::events::queue::{self, Event, now_unix};
use actix_web::{HttpResponse, Responder, get, web};
use std::time::Duration;
use uuid::Uuid;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// A comment line this often keeps proxies from closing an idle stream.
const KEEPALIVE_EVERY: u32 = 7;
const BATCH: u32 = 100;

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
    /// Keyset cursor: the last sent event's `(created_at, id)`. `last_id`
    /// starts empty so events in the connect second are included.
    since: i64,
    last_id: String,
    /// The token's expiry; the stream ends once it passes, so the client
    /// reconnects with a fresh token.
    exp: i64,
    /// The last page was full, so poll again without waiting.
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
    let state = StreamState {
        org_id: auth.org_id,
        since: now_unix(),
        last_id: String::new(),
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
            }
            // Inline like the other handlers: one small indexed query.
            let events = fetch(st.org_id, st.since, &st.last_id).unwrap_or_else(|err| {
                eprintln!("events stream: {err}");
                Vec::new()
            });
            st.more = events.len() >= BATCH as usize;
            let Some(last) = events.last() else {
                st.idle_polls += 1;
                if st.idle_polls >= KEEPALIVE_EVERY {
                    st.idle_polls = 0;
                    return Some((Ok(web::Bytes::from_static(b": keepalive\n\n")), st));
                }
                continue;
            };
            st.idle_polls = 0;
            st.since = last.created_at;
            st.last_id = last.id.to_string();
            let out: String = events.iter().map(frame).collect();
            return Some((Ok(web::Bytes::from(out)), st));
        }
    });
    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(body)
}
