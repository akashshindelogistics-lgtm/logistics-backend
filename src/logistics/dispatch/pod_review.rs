//! Background check of a delivered dispatch's proof-of-delivery photo, run by
//! the `pod_photo_check` consumer on `dispatch.delivered`. See
//! `docs/file-uploads.md`.
//!
//! The check at delivery time (`ai::pod_check`) only blocks a clear "no" and
//! fails open. This consumer looks again with retries, and records the outcome
//! in `PodPhotoChecks` so a person can review every photo that is doubtful or
//! could not be checked.

use crate::logistics::ai::pod_check::{self, PodVerdict};
use crate::logistics::db::connection::DbConnection;
use crate::logistics::dispatch::dispatch::DispatchOrder;
use crate::logistics::events::queue::{Event, now_unix};
use crate::logistics::events::worker::Consumer;
use crate::logistics::upload::upload::UploadedFile;
use futures_util::future::LocalBoxFuture;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use utoipa::ToSchema;
use uuid::Uuid;

pub const CONSUMER_NAME: &str = "pod_photo_check";

/// Outcomes that need no one: `OK` and `NOT_CHECKED` (not one of our uploads).
const OK: &str = "OK";
const NOT_CHECKED: &str = "NOT_CHECKED";
/// Outcomes a person should look at.
const DOUBTFUL: &str = "DOUBTFUL";
const UNCHECKED: &str = "UNCHECKED";

/// One flagged proof-of-delivery photo.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodPhotoFlag {
    pub dispatch_id: Uuid,
    /// `DOUBTFUL`: the model says it is not a signature or delivery photo.
    /// `UNCHECKED`: the check kept failing, so nobody has looked at it.
    pub verdict: String,
    pub reason: String,
    pub checked_at: i64,
}

pub(crate) fn ensure_table(conn: &mut PooledConn) -> Result<(), Box<dyn Error>> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS PodPhotoChecks (
            dispatch_id VARCHAR(36) PRIMARY KEY,
            org_id VARCHAR(36) NOT NULL,
            verdict VARCHAR(16) NOT NULL,
            reason VARCHAR(1000) NOT NULL,
            checked_at BIGINT NOT NULL,
            INDEX idx_pod_photo_checks_org (org_id, verdict)
        )",
    )?;
    Ok(())
}

/// Insert or overwrite: a repeated event just records the same outcome again.
fn record(
    org_id: Uuid,
    dispatch_id: Uuid,
    verdict: &str,
    reason: &str,
) -> Result<(), Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    conn.exec_drop(
        "INSERT INTO PodPhotoChecks (dispatch_id, org_id, verdict, reason, checked_at)
         VALUES (:dispatch_id, :org_id, :verdict, :reason, :checked_at)
         ON DUPLICATE KEY UPDATE verdict = VALUES(verdict), reason = VALUES(reason),
                                 checked_at = VALUES(checked_at)",
        params! {
            "dispatch_id" => dispatch_id.to_string(),
            "org_id" => org_id.to_string(),
            "verdict" => verdict,
            "reason" => reason.chars().take(1000).collect::<String>(),
            "checked_at" => now_unix(),
        },
    )?;
    Ok(())
}

/// The org's photos a person should look at, newest first.
pub fn list_flagged(org_id: Uuid) -> Result<Vec<PodPhotoFlag>, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    let rows: Vec<(String, String, String, i64)> = conn.exec(
        "SELECT dispatch_id, verdict, reason, checked_at FROM PodPhotoChecks
         WHERE org_id = :org_id AND verdict IN ('DOUBTFUL', 'UNCHECKED')
         ORDER BY checked_at DESC",
        params! { "org_id" => org_id.to_string() },
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, verdict, reason, checked_at)| {
            Some(PodPhotoFlag {
                dispatch_id: Uuid::parse_str(&id).ok()?,
                verdict,
                reason,
                checked_at,
            })
        })
        .collect())
}

pub struct PodPhotoConsumer;

impl Consumer for PodPhotoConsumer {
    fn name(&self) -> &'static str {
        CONSUMER_NAME
    }

    fn handle<'a>(
        &'a self,
        event: &'a Event,
        _attempt: u32,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        Box::pin(async move { check(event).await })
    }

    /// Retries are used up: a person must look at the photo.
    fn on_dead_letter(&self, event: &Event) {
        let Some(dispatch_id) = dispatch_id(event) else {
            return;
        };
        if let Err(err) = record(
            event.org_id,
            dispatch_id,
            UNCHECKED,
            "the automatic photo check kept failing",
        ) {
            eprintln!("pod photo check: dead-letter update failed: {err}");
        }
    }
}

fn dispatch_id(event: &Event) -> Option<Uuid> {
    event.payload["dispatch_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
}

async fn check(event: &Event) -> Result<(), String> {
    let id = dispatch_id(event).ok_or("event payload has no valid dispatch_id")?;
    let org_id = event.org_id;
    let err = |e: Box<dyn Error>| e.to_string();

    let dispatch = DispatchOrder::get_by_id(id).map_err(err)?;
    let url = dispatch
        .filter(|d| d.org_id == org_id)
        .and_then(|d| d.proof_of_delivery)
        .map(|p| p.signature_or_photo_url);
    let file = url
        .as_deref()
        .and_then(pod_check::upload_id_from_url)
        .map(UploadedFile::get_by_id)
        .transpose()
        .map_err(err)?
        .flatten()
        .filter(|f| f.org_id == org_id);
    let Some(file) = file else {
        // An external URL or a missing proof is never fetched.
        return record(org_id, id, NOT_CHECKED, "not an upload of this backend").map_err(err);
    };

    match pod_check::check_upload(&file).await? {
        PodVerdict::Plausible => record(org_id, id, OK, ""),
        PodVerdict::NotProof => record(
            org_id,
            id,
            DOUBTFUL,
            "doesn't look like a signature or a photo of the delivered goods",
        ),
    }
    .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::test_support::TestDb;
    use serde_json::json;

    fn event(org_id: Uuid, dispatch_id: Uuid) -> Event {
        Event {
            id: Uuid::new_v4(),
            org_id,
            event_type: "dispatch.delivered".to_string(),
            payload: json!({ "dispatch_id": dispatch_id }),
            created_at: 0,
        }
    }

    #[test]
    fn only_doubtful_and_unchecked_photos_are_listed_per_org() {
        let _db = TestDb::create();
        let (org, other) = (Uuid::new_v4(), Uuid::new_v4());
        let (ok, bad, stuck, theirs) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        record(org, ok, OK, "").unwrap();
        record(org, bad, DOUBTFUL, "a meme").unwrap();
        record(org, stuck, UNCHECKED, "api down").unwrap();
        record(other, theirs, DOUBTFUL, "x").unwrap();
        // A repeated event overwrites rather than failing or duplicating.
        record(org, bad, DOUBTFUL, "a meme").unwrap();

        let flags = list_flagged(org).unwrap();
        let mut ids: Vec<_> = flags.iter().map(|f| f.dispatch_id).collect();
        ids.sort();
        let mut want = vec![bad, stuck];
        want.sort();
        assert_eq!(ids, want);
    }

    #[actix_web::test]
    async fn a_proof_that_is_not_one_of_our_uploads_is_noted_and_not_flagged() {
        let _db = TestDb::create();
        let (org, dispatch) = (Uuid::new_v4(), Uuid::new_v4());
        // No such dispatch: nothing to fetch, and no paid call.
        check(&event(org, dispatch)).await.expect("handled");
        assert!(list_flagged(org).unwrap().is_empty());
        let mut conn = DbConnection::from_env().get_connection().unwrap();
        let verdict: Option<String> = conn
            .exec_first(
                "SELECT verdict FROM PodPhotoChecks WHERE dispatch_id = :id",
                params! { "id" => dispatch.to_string() },
            )
            .unwrap();
        assert_eq!(verdict.as_deref(), Some(NOT_CHECKED));
    }

    #[test]
    fn an_exhausted_delivery_is_flagged_for_a_person() {
        let _db = TestDb::create();
        let (org, dispatch) = (Uuid::new_v4(), Uuid::new_v4());
        PodPhotoConsumer.on_dead_letter(&event(org, dispatch));
        let flags = list_flagged(org).unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].verdict, UNCHECKED);
    }
}
