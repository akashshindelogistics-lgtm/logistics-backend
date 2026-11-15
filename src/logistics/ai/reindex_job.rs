//! `assistant/reindex` as a background job. The request records a job row and
//! publishes an event in one transaction and returns at once; the
//! `assistant_reindexer` consumer rebuilds the index and keeps `chunks_indexed`
//! current, so the dashboard can poll the job for progress.
//!
//! The consumer is one of [`worker::local_only_consumers`]: it runs on its own
//! thread even when `EVENT_WORKERS_ENABLED` is off, like the synchronous call
//! it replaced, and never delays notifications. It claims one job at a time
//! and renews its queue lease while it works, so a long run isn't re-claimed
//! and started a second time.

use crate::logistics::ai::chunk::reindex_org_with_progress;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::events::queue::{self, Event, EventType, LEASE_SECS, now_unix};
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

/// A `QUEUED` or `RUNNING` job untouched for this long is taken as abandoned
/// (workers switched off, delivery purged), so it no longer blocks a new one.
/// A running job touches its row every [`HEARTBEAT_SECS`].
const STALE_AFTER_SECS: i64 = 2 * LEASE_SECS;
/// How often a running job saves its count and renews its lease.
const HEARTBEAT_SECS: i64 = 10;

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

/// The org's job that is still `QUEUED` or `RUNNING` and not stale, if any.
fn active_job(
    conn: &mut impl Queryable,
    org_id: Uuid,
    now: i64,
) -> Result<Option<ReindexJob>, Box<dyn Error>> {
    let row: Option<JobRow> = conn.exec_first(
        format!(
            "SELECT {COLUMNS} FROM AssistantReindexJobs
             WHERE org_id = :org_id AND status IN ('QUEUED', 'RUNNING') AND updated_at > :cutoff
             ORDER BY created_at DESC LIMIT 1"
        ),
        params! { "org_id" => org_id.to_string(), "cutoff" => now - STALE_AFTER_SECS },
    )?;
    row.map(job_from_row).transpose()
}

/// Queue a reindex of `org_id` and return its job at once. If one is already
/// queued or running, that job is returned instead of starting another, unless
/// it has gone stale.
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
    let now = now_unix();
    if let Some(job) = active_job(&mut tx, org_id, now)? {
        return Ok(job);
    }

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
    worker::wake_local();
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

/// Mark a job `FAILED`, keeping its count. A job that finished is left alone:
/// a duplicate delivery can run out of retries after another run completed.
fn mark_failed(conn: &mut PooledConn, job_id: Uuid, error: &str) -> Result<(), Box<dyn Error>> {
    conn.exec_drop(
        "UPDATE AssistantReindexJobs
         SET status = :status, error = :error, updated_at = :now
         WHERE id = :id AND status <> :done",
        params! {
            "status" => FAILED,
            "error" => error,
            "now" => now_unix(),
            "id" => job_id.to_string(),
            "done" => DONE,
        },
    )?;
    Ok(())
}

/// What a delivery should do with its job.
#[derive(Debug, PartialEq, Eq)]
enum RunDecision {
    /// Already finished: complete the delivery.
    Finished,
    /// Another worker is running it (it saved progress within a lease): retry
    /// later rather than run it twice at once.
    RunningElsewhere,
    Start,
}

fn run_decision(job: &ReindexJob, now: i64) -> RunDecision {
    if job.status == DONE {
        RunDecision::Finished
    } else if job.status == RUNNING && now - job.updated_at < LEASE_SECS {
        RunDecision::RunningElsewhere
    } else {
        RunDecision::Start
    }
}

/// Whether a running job should save its count and renew its lease again.
fn heartbeat_due(last: i64, now: i64) -> bool {
    now - last >= HEARTBEAT_SECS
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
    match run_decision(&job, now_unix()) {
        RunDecision::Finished => return Ok(()),
        RunDecision::RunningElsewhere => {
            return Err(format!("job {job_id} is already running"));
        }
        RunDecision::Start => {}
    }
    update(&mut conn, job_id, RUNNING, 0, None).map_err(err)?;
    // Heartbeats are best effort: losing one must not fail the job.
    let _ = queue::extend_lease(&mut conn, event.id, CONSUMER_NAME);
    let mut last_beat = now_unix();
    let result = reindex_org_with_progress(event.org_id, &mut |count| {
        let now = now_unix();
        if heartbeat_due(last_beat, now) {
            last_beat = now;
            let _ = update(&mut conn, job_id, RUNNING, count, None);
            let _ = queue::extend_lease(&mut conn, event.id, CONSUMER_NAME);
        }
    });
    match result {
        Ok(total) => update(&mut conn, job_id, DONE, total, None).map_err(err),
        Err(e) => {
            let message = e.to_string();
            // Back to QUEUED, so the retry isn't taken for a run in progress.
            let _ = update(&mut conn, job_id, QUEUED, 0, Some(message.as_str()));
            Err(message)
        }
    }
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

    fn batch_size(&self) -> u32 {
        1
    }

    fn on_dead_letter(&self, event: &Event) {
        let Ok(job_id) = job_id_of(event) else { return };
        let result = DbConnection::from_env()
            .get_connection()
            .and_then(|mut conn| {
                mark_failed(&mut conn, job_id, "reindex failed after every retry")
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

    fn conn() -> PooledConn {
        DbConnection::from_env().get_connection().expect("conn")
    }

    fn event_for(org_id: Uuid, job_id: Uuid) -> Event {
        Event {
            id: Uuid::new_v4(),
            org_id,
            event_type: EventType::AssistantReindexRequested.as_str().to_string(),
            payload: serde_json::json!({ "job_id": job_id }),
            created_at: now_unix(),
        }
    }

    fn job_with(status: &str, updated_at: i64) -> ReindexJob {
        ReindexJob {
            id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            status: status.to_string(),
            chunks_indexed: 0,
            error: None,
            created_at: 0,
            updated_at,
        }
    }

    #[test]
    fn a_stale_active_job_does_not_block_a_new_one() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Stale Org", "1 St").expect("org");
        let stuck = enqueue(org.id).expect("enqueue");
        conn()
            .exec_drop(
                "UPDATE AssistantReindexJobs SET updated_at = :at WHERE id = :id",
                params! {
                    "at" => now_unix() - STALE_AFTER_SECS - 1,
                    "id" => stuck.id.to_string(),
                },
            )
            .expect("age the job");
        let fresh = enqueue(org.id).expect("enqueue again");
        assert_ne!(fresh.id, stuck.id);
        assert_eq!(fresh.status, QUEUED);
        // And the fresh one now blocks in turn.
        assert_eq!(enqueue(org.id).expect("third").id, fresh.id);
    }

    #[test]
    fn a_job_is_started_unless_done_or_running_within_a_lease() {
        let now = 10_000;
        assert_eq!(
            run_decision(&job_with(DONE, now), now),
            RunDecision::Finished
        );
        assert_eq!(
            run_decision(&job_with(RUNNING, now - LEASE_SECS + 1), now),
            RunDecision::RunningElsewhere
        );
        // A worker that died mid-run stopped saving progress a lease ago.
        assert_eq!(
            run_decision(&job_with(RUNNING, now - LEASE_SECS), now),
            RunDecision::Start
        );
        assert_eq!(
            run_decision(&job_with(QUEUED, now), now),
            RunDecision::Start
        );
    }

    #[test]
    fn heartbeats_are_throttled_well_inside_the_lease() {
        assert!(!heartbeat_due(100, 100 + HEARTBEAT_SECS - 1));
        assert!(heartbeat_due(100, 100 + HEARTBEAT_SECS));
    }

    #[actix_web::test]
    async fn the_consumer_claims_one_reindex_at_a_time() {
        let _db = TestDb::create();
        let first = Organization::create_organization("Reindex One Org", "1 St").expect("org");
        let second = Organization::create_organization("Reindex Two Org", "2 St").expect("org");
        enqueue(first.id).expect("first");
        enqueue(second.id).expect("second");
        assert_eq!(drain_once(&consumers(), "w").await.expect("drain"), 1);
        assert_eq!(drain_once(&consumers(), "w").await.expect("drain"), 1);
        assert_eq!(drain_once(&consumers(), "w").await.expect("drain"), 0);
    }

    #[actix_web::test]
    async fn a_job_running_elsewhere_is_retried_not_run_twice() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Busy Org", "1 St").expect("org");
        let job = enqueue(org.id).expect("enqueue");
        update(&mut conn(), job.id, RUNNING, 7, None).expect("mark running");

        assert_eq!(drain_once(&consumers(), "w").await.expect("drain"), 1);

        let still = get(org.id, job.id).unwrap().unwrap();
        assert_eq!(still.status, RUNNING);
        assert_eq!(still.chunks_indexed, 7);
        let pending: Option<String> = conn()
            .query_first(
                "SELECT last_error FROM EventDeliveries WHERE status = 'PENDING' AND attempts = 1",
            )
            .expect("query");
        assert!(
            pending
                .expect("a retry is scheduled")
                .contains("already running")
        );
    }

    #[test]
    fn extend_lease_renews_a_claimed_delivery() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Lease Org", "1 St").expect("org");
        enqueue(org.id).expect("enqueue");
        let mut c = conn();
        let claimed = queue::claim(&mut c, CONSUMER_NAME, "w", 1).expect("claim");
        c.query_drop("UPDATE EventDeliveries SET locked_until = 0")
            .expect("expire");
        queue::extend_lease(&mut c, claimed[0].event.id, CONSUMER_NAME).expect("extend");
        let until: Option<i64> = c
            .query_first("SELECT locked_until FROM EventDeliveries")
            .expect("query");
        assert!(until.expect("a row") > now_unix() + LEASE_SECS - 5);
    }

    #[actix_web::test]
    async fn dead_letter_leaves_a_finished_job_done() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Done Org", "1 St").expect("org");
        Customer::create_customer(org.id, "Legacy Customer", "1 Old Rd").expect("customer");
        let job = enqueue(org.id).expect("enqueue");
        drain_once(&consumers(), "w").await.expect("drain");
        let done = get(org.id, job.id).unwrap().unwrap();
        assert_eq!(done.status, DONE);

        ReindexConsumer.on_dead_letter(&event_for(org.id, job.id));
        let after = get(org.id, job.id).unwrap().unwrap();
        assert_eq!(after.status, DONE);
        assert_eq!(after.chunks_indexed, done.chunks_indexed);
        assert!(after.error.is_none());
    }

    #[test]
    fn dead_letter_marks_the_job_failed() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Reindex Dead Org", "1 St").expect("org");
        let job = enqueue(org.id).expect("enqueue");
        ReindexConsumer.on_dead_letter(&event_for(org.id, job.id));
        let failed = get(org.id, job.id).unwrap().unwrap();
        assert_eq!(failed.status, FAILED);
        assert!(failed.error.is_some());
    }
}
