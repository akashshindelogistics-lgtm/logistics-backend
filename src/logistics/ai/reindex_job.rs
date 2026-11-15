//! `assistant/reindex` as a background job. The request records a job row and
//! publishes an event in one transaction and returns at once; the
//! `assistant_reindexer` consumer rebuilds the index and keeps `chunks_indexed`
//! current, so the dashboard can poll the job for progress.

use crate::logistics::ai::chunk::reindex_org_with_progress;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::events::queue::{self, Event, EventType, now_unix};
use crate::logistics::events::worker::{self, Consumer};
use futures_util::future::LocalBoxFuture;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use utoipa::ToSchema;
use uuid::Uuid;

pub const CONSUMER_NAME: &str = "assistant_reindexer";
const MAX_ERROR_CHARS: usize = 1000;

const QUEUED: &str = "QUEUED";
const RUNNING: &str = "RUNNING";
const DONE: &str = "DONE";
const FAILED: &str = "FAILED";

/// One reindex run. `chunks_indexed` counts chunks written so far, and is the
/// final total once `status` is `DONE`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReindexJob {
    pub id: Uuid,
    pub org_id: Uuid,
    /// `QUEUED`, `RUNNING`, `DONE` or `FAILED`.
    pub status: String,
    pub chunks_indexed: u64,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

type JobRow = (String, String, String, u64, Option<String>, i64, i64);

const COLUMNS: &str = "id, org_id, status, chunks_indexed, error, created_at, updated_at";

fn job_from_row(row: JobRow) -> Result<ReindexJob, Box<dyn Error>> {
    Ok(ReindexJob {
        id: Uuid::parse_str(&row.0)?,
        org_id: Uuid::parse_str(&row.1)?,
        status: row.2,
        chunks_indexed: row.3,
        error: row.4,
        created_at: row.5,
        updated_at: row.6,
    })
}

fn ensure_table(conn: &mut PooledConn) -> Result<(), Box<dyn Error>> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS AssistantReindexJobs (
            id VARCHAR(36) PRIMARY KEY,
            org_id VARCHAR(36) NOT NULL,
            status VARCHAR(16) NOT NULL,
            chunks_indexed BIGINT UNSIGNED NOT NULL DEFAULT 0,
            error TEXT DEFAULT NULL,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            INDEX idx_reindex_job_org (org_id, created_at),
            CONSTRAINT fk_reindex_job_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
        )",
    )?;
    Ok(())
}

/// Queue a reindex of `org_id` and return its job at once. If one is already
/// queued or running, that job is returned instead of starting another.
pub fn enqueue(org_id: Uuid) -> Result<ReindexJob, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    // Before the transaction: `CREATE TABLE` commits implicitly.
    ensure_table(&mut conn)?;
    queue::ensure_tables(&mut conn)?;

    let mut tx = conn.start_transaction(TxOpts::default())?;
    // Lock the org row so two concurrent requests can't both start a job.
    tx.exec_drop(
        "SELECT id FROM Orgs WHERE id = :org_id FOR UPDATE",
        params! { "org_id" => org_id.to_string() },
    )?;
    let existing: Option<JobRow> = tx.exec_first(
        format!(
            "SELECT {COLUMNS} FROM AssistantReindexJobs
             WHERE org_id = :org_id AND status IN ('QUEUED', 'RUNNING')
             ORDER BY created_at DESC LIMIT 1"
        ),
        params! { "org_id" => org_id.to_string() },
    )?;
    if let Some(row) = existing {
        return job_from_row(row);
    }

    let now = now_unix();
    let job = ReindexJob {
        id: Uuid::new_v4(),
        org_id,
        status: QUEUED.to_string(),
        chunks_indexed: 0,
        error: None,
        created_at: now,
        updated_at: now,
    };
    tx.exec_drop(
        "INSERT INTO AssistantReindexJobs (id, org_id, status, chunks_indexed, created_at, updated_at)
         VALUES (:id, :org_id, :status, 0, :now, :now)",
        params! {
            "id" => job.id.to_string(),
            "org_id" => org_id.to_string(),
            "status" => QUEUED,
            "now" => now,
        },
    )?;
    queue::publish(
        &mut tx,
        org_id,
        EventType::AssistantReindexRequested,
        serde_json::json!({ "job_id": job.id }),
        None,
    )?;
    tx.commit()?;
    worker::wake();
    Ok(job)
}

/// A job of `org_id`, or `None` if it doesn't exist or belongs to another org.
pub fn get(org_id: Uuid, job_id: Uuid) -> Result<Option<ReindexJob>, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    let row: Option<JobRow> = conn.exec_first(
        format!("SELECT {COLUMNS} FROM AssistantReindexJobs WHERE id = :id AND org_id = :org_id"),
        params! { "id" => job_id.to_string(), "org_id" => org_id.to_string() },
    )?;
    row.map(job_from_row).transpose()
}

/// The org's most recent job, so the dashboard can show progress after a reload.
pub fn latest(org_id: Uuid) -> Result<Option<ReindexJob>, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    let row: Option<JobRow> = conn.exec_first(
        format!(
            "SELECT {COLUMNS} FROM AssistantReindexJobs
             WHERE org_id = :org_id ORDER BY created_at DESC, id LIMIT 1"
        ),
        params! { "org_id" => org_id.to_string() },
    )?;
    row.map(job_from_row).transpose()
}

fn update(
    conn: &mut PooledConn,
    job_id: Uuid,
    status: &str,
    chunks: usize,
    error: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let error = error.map(|e| e.chars().take(MAX_ERROR_CHARS).collect::<String>());
    conn.exec_drop(
        "UPDATE AssistantReindexJobs
         SET status = :status, chunks_indexed = :chunks, error = :error, updated_at = :now
         WHERE id = :id",
        params! {
            "status" => status,
            "chunks" => chunks as u64,
            "error" => error,
            "now" => now_unix(),
            "id" => job_id.to_string(),
        },
    )?;
    Ok(())
}

fn job_id_of(event: &Event) -> Result<Uuid, String> {
    event.payload["job_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| "event has no job_id".to_string())
}

fn run(event: &Event) -> Result<(), String> {
    let job_id = job_id_of(event)?;
    let err = |e: Box<dyn Error>| e.to_string();
    let mut conn = DbConnection::from_env()
        .get_connection()
        .map_err(|e| e.to_string())?;
    let Some(job) = get(event.org_id, job_id).map_err(err)? else {
        // The org or job is gone: nothing to do, and a retry can't help.
        return Ok(());
    };
    if job.status == DONE {
        return Ok(());
    }
    update(&mut conn, job_id, RUNNING, 0, None).map_err(err)?;
    let total = reindex_org_with_progress(event.org_id, &mut |count| {
        // Progress is best effort: losing an update must not fail the job.
        let _ = update(&mut conn, job_id, RUNNING, count, None);
    })
    .map_err(err)?;
    update(&mut conn, job_id, DONE, total, None).map_err(err)
}

pub struct ReindexConsumer;

impl Consumer for ReindexConsumer {
    fn name(&self) -> &'static str {
        CONSUMER_NAME
    }

    fn handle<'a>(
        &'a self,
        event: &'a Event,
        _attempt: u32,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        Box::pin(async move { run(event) })
    }

    fn on_dead_letter(&self, event: &Event) {
        let Ok(job_id) = job_id_of(event) else { return };
        let result = DbConnection::from_env()
            .get_connection()
            .and_then(|mut conn| {
                update(
                    &mut conn,
                    job_id,
                    FAILED,
                    0,
                    Some("reindex failed after every retry"),
                )
            });
        if let Err(err) = result {
            eprintln!("reindex consumer: dead-letter update failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::customer::customer::Customer;
    use crate::logistics::events::worker::drain_once;
    use crate::logistics::orgs::orgs::Organization;
    use crate::logistics::test_support::TestDb;

    fn consumers() -> Vec<Box<dyn Consumer>> {
        vec![Box::new(ReindexConsumer)]
    }

    #[test]
    fn enqueue_returns_a_queued_job_without_indexing() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Job Org", "1 St").expect("org");
        let job = enqueue(org.id).expect("enqueue");
        assert_eq!(job.status, QUEUED);
        assert_eq!(job.chunks_indexed, 0);
        assert_eq!(get(org.id, job.id).unwrap().unwrap().status, QUEUED);
    }

    #[test]
    fn enqueue_reuses_the_active_job() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Dup Org", "1 St").expect("org");
        let first = enqueue(org.id).expect("first");
        let second = enqueue(org.id).expect("second");
        assert_eq!(first.id, second.id);
    }

    #[actix_web::test]
    async fn the_consumer_rebuilds_the_index_and_records_the_count() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Run Org", "1 St").expect("org");
        Customer::create_customer(org.id, "Legacy Customer", "1 Old Rd").expect("customer");
        let job = enqueue(org.id).expect("enqueue");

        let processed = drain_once(&consumers(), "test-worker")
            .await
            .expect("drain");
        assert_eq!(processed, 1);

        let done = get(org.id, job.id).unwrap().unwrap();
        assert_eq!(done.status, DONE);
        assert!(done.chunks_indexed >= 1);
        assert_eq!(latest(org.id).unwrap().unwrap().id, job.id);

        // Finished, so the next request starts a fresh job.
        assert_ne!(enqueue(org.id).expect("again").id, job.id);
    }

    #[test]
    fn a_job_is_not_visible_to_another_org() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Own Org", "1 St").expect("org");
        let other = Organization::create_organization("Reindex Other Org", "2 St").expect("org");
        let job = enqueue(org.id).expect("enqueue");
        assert!(get(other.id, job.id).unwrap().is_none());
    }

    #[test]
    fn dead_letter_marks_the_job_failed() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Dead Org", "1 St").expect("org");
        let job = enqueue(org.id).expect("enqueue");
        let event = Event {
            id: Uuid::new_v4(),
            org_id: org.id,
            event_type: EventType::AssistantReindexRequested.as_str().to_string(),
            payload: serde_json::json!({ "job_id": job.id }),
            created_at: now_unix(),
        };
        ReindexConsumer.on_dead_letter(&event);
        let failed = get(org.id, job.id).unwrap().unwrap();
        assert_eq!(failed.status, FAILED);
        assert!(failed.error.is_some());
    }
}
