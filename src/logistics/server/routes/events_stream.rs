//! Server-Sent Events feed of an org's events, so open dashboards refresh
//! without a reload. See `docs/async-events.md`.

use super::common::*;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::events::queue::{self, Event, now_unix};
use actix_web::{HttpResponse, Responder, get, web};
use std::collections::HashSet;
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

fn fetch(org_id: Uuid, since: i64) -> Result<Vec<Event>, String> {
    let mut conn = DbConnection::from_env()
        .get_connection()
        .map_err(|e| e.to_string())?;
    queue::ensure_tables(&mut conn).map_err(|e| e.to_string())?;
    queue::list_for_org_since(&mut conn, org_id, since, BATCH).map_err(|e| e.to_string())
}

struct StreamState {
    org_id: Uuid,
    since: i64,
    /// Ids already sent at second `since`, since the cursor is inclusive.
    seen: HashSet<Uuid>,
    idle_polls: u32,
    first: bool,
}

#[utoipa::path(
    get,
    path = "/api/events/stream",
    tag = "Events",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "text/event-stream of the org's new events (`event:` is the type, `data:` the JSON payload). Only events from after connecting are sent.", content_type = "text/event-stream", body = String),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/events/stream")]
pub async fn stream_events(auth: AuthenticatedOrg) -> impl Responder {
    let state = StreamState {
        org_id: auth.org_id,
        since: now_unix(),
        seen: HashSet::new(),
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
            tokio::time::sleep(POLL_INTERVAL).await;
            let (org_id, since) = (st.org_id, st.since);
            // Inline like the other handlers: one small indexed query.
            let events = fetch(org_id, since).unwrap_or_else(|err| {
                eprintln!("events stream: {err}");
                Vec::new()
            });
            let fresh: Vec<&Event> = events.iter().filter(|e| !st.seen.contains(&e.id)).collect();
            if fresh.is_empty() {
                st.idle_polls += 1;
                if st.idle_polls >= KEEPALIVE_EVERY {
                    st.idle_polls = 0;
                    return Some((Ok(web::Bytes::from_static(b": keepalive\n\n")), st));
                }
                continue;
            }
            st.idle_polls = 0;
            let out: String = fresh.iter().map(|e| frame(e)).collect();
            if let Some(last) = events.last() {
                if last.created_at > st.since {
                    st.seen.clear();
                }
                st.since = last.created_at;
                st.seen.extend(
                    events
                        .iter()
                        .filter(|e| e.created_at == st.since)
                        .map(|e| e.id),
                );
            }
            return Some((Ok(web::Bytes::from(out)), st));
        }
    });
    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(body)
}
