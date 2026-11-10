//! Storage for events and per-consumer deliveries. Nothing outside this file
//! knows how the queue is stored.

use crate::logistics::notification::consumer::CONSUMER_NAME as NOTIFICATION_CONSUMER;
use mysql::prelude::*;
use mysql::*;
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Attempts before a delivery is given up on and marked `DEAD`.
pub const MAX_ATTEMPTS: u32 = 6;
/// How long a claim lasts. A worker that dies mid-event loses it after this.
pub const LEASE_SECS: i64 = 300;
const BASE_BACKOFF_SECS: i64 = 30;
const MAX_BACKOFF_SECS: i64 = 3600;
const MAX_ERROR_CHARS: usize = 1000;

const PENDING: &str = "PENDING";
const PROCESSING: &str = "PROCESSING";
const DONE: &str = "DONE";
const DEAD: &str = "DEAD";

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Everything that can be published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    DispatchCreated,
    DispatchDelivered,
    DispatchRunningLate,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::DispatchCreated => "dispatch.created",
            EventType::DispatchDelivered => "dispatch.delivered",
            EventType::DispatchRunningLate => "dispatch.running_late",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dispatch.created" => Some(EventType::DispatchCreated),
            "dispatch.delivered" => Some(EventType::DispatchDelivered),
            "dispatch.running_late" => Some(EventType::DispatchRunningLate),
            _ => None,
        }
    }

    /// The consumers that receive this event.
    pub fn subscribers(self) -> &'static [&'static str] {
        match self {
            EventType::DispatchCreated
            | EventType::DispatchDelivered
            | EventType::DispatchRunningLate => &[NOTIFICATION_CONSUMER],
        }
    }
}

/// An event as stored.
#[derive(Debug, Clone)]
pub struct Event {
    pub id: Uuid,
    pub org_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

/// One claimed unit of work: `consumer` should process `event`.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub delivery_id: u64,
    pub consumer: String,
    /// 1 on the first try. Counts this claim.
    pub attempt: u32,
    pub event: Event,
    /// Set when a worker kept dying on this delivery. It has been marked
    /// `DEAD` and must not be processed again.
    pub exhausted: bool,
}

/// Create the queue tables. MySQL implicitly commits on `CREATE TABLE`, so this
/// must run before a transaction opens, never inside one.
pub fn ensure_tables(conn: &mut PooledConn) -> Result<(), Box<dyn Error>> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS Events (
            id VARCHAR(36) PRIMARY KEY,
            org_id VARCHAR(36) NOT NULL,
            event_type VARCHAR(64) NOT NULL,
            payload TEXT NOT NULL,
            dedupe_key VARCHAR(191) DEFAULT NULL UNIQUE,
            created_at BIGINT NOT NULL,
            INDEX idx_events_created (created_at),
            CONSTRAINT fk_event_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
        )",
    )?;
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS EventDeliveries (
            id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
            event_id VARCHAR(36) NOT NULL,
            consumer VARCHAR(64) NOT NULL,
            status VARCHAR(16) NOT NULL,
            attempts INT UNSIGNED NOT NULL DEFAULT 0,
            next_attempt_at BIGINT NOT NULL,
            locked_until BIGINT DEFAULT NULL,
            locked_by VARCHAR(36) DEFAULT NULL,
            last_error TEXT DEFAULT NULL,
            completed_at BIGINT DEFAULT NULL,
            UNIQUE KEY uq_delivery (event_id, consumer),
            INDEX idx_delivery_due (consumer, status, next_attempt_at),
            CONSTRAINT fk_delivery_event FOREIGN KEY (event_id) REFERENCES Events(id) ON DELETE CASCADE
        )",
    )?;
    Ok(())
}

/// Publish to `event_type`'s subscribers, inside the caller's transaction, so
/// the event exists exactly when the change it describes does. Returns `false`
/// if `dedupe_key` was already published. Call
/// [`wake`](super::worker::wake) after committing.
pub fn publish(
    tx: &mut Transaction<'_>,
    org_id: Uuid,
    event_type: EventType,
    payload: serde_json::Value,
    dedupe_key: Option<&str>,
) -> Result<bool, Box<dyn Error>> {
    publish_to(
        tx,
        org_id,
        event_type.as_str(),
        payload,
        dedupe_key,
        event_type.subscribers(),
    )
}

/// [`publish`] with an explicit subscriber list.
pub(crate) fn publish_to(
    tx: &mut Transaction<'_>,
    org_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
    dedupe_key: Option<&str>,
    consumers: &[&str],
) -> Result<bool, Box<dyn Error>> {
    let event_id = Uuid::new_v4().to_string();
    let now = now_unix();
    // Not `INSERT IGNORE`, which would also hide a real error like a bad org.
    tx.exec_drop(
        "INSERT INTO Events (id, org_id, event_type, payload, dedupe_key, created_at)
         VALUES (:id, :org_id, :event_type, :payload, :dedupe_key, :created_at)
         ON DUPLICATE KEY UPDATE id = id",
        params! {
            "id" => &event_id,
            "org_id" => org_id.to_string(),
            "event_type" => event_type,
            "payload" => payload.to_string(),
            "dedupe_key" => dedupe_key,
            "created_at" => now,
        },
    )?;
    if tx.affected_rows() == 0 {
        return Ok(false);
    }
    for consumer in consumers {
        tx.exec_drop(
            "INSERT INTO EventDeliveries (event_id, consumer, status, next_attempt_at)
             VALUES (:event_id, :consumer, :status, :next_attempt_at)",
            params! {
                "event_id" => &event_id,
                "consumer" => *consumer,
                "status" => PENDING,
                "next_attempt_at" => now,
            },
        )?;
    }
    Ok(true)
}

/// Claim up to `limit` due deliveries for `consumer`. `SKIP LOCKED` lets
/// competing workers each take different rows without waiting on one another.
pub fn claim(
    conn: &mut PooledConn,
    consumer: &str,
    worker_token: &str,
    limit: u32,
) -> Result<Vec<Claimed>, Box<dyn Error>> {
    let now = now_unix();
    let mut tx = conn.start_transaction(TxOpts::default())?;
    let rows: Vec<(u64, u32, String, String, String, String, i64)> = tx.exec(
        "SELECT d.id, d.attempts, e.id, e.org_id, e.event_type, e.payload, e.created_at
         FROM EventDeliveries d
         JOIN Events e ON e.id = d.event_id
         WHERE d.consumer = :consumer
           AND ((d.status = 'PENDING' AND d.next_attempt_at <= :now)
             OR (d.status = 'PROCESSING' AND d.locked_until <= :now))
         ORDER BY d.id
         LIMIT :limit
         FOR UPDATE OF d SKIP LOCKED",
        params! { "consumer" => consumer, "now" => now, "limit" => limit },
    )?;

    let mut claimed = Vec::with_capacity(rows.len());
    for (delivery_id, attempts, event_id, org_id, event_type, payload, created_at) in rows {
        let exhausted = attempts >= MAX_ATTEMPTS;
        if exhausted {
            tx.exec_drop(
                "UPDATE EventDeliveries
                 SET status = :status, locked_until = NULL, locked_by = NULL,
                     last_error = 'worker lost the lease on every attempt'
                 WHERE id = :id",
                params! { "status" => DEAD, "id" => delivery_id },
            )?;
        } else {
            tx.exec_drop(
                "UPDATE EventDeliveries
                 SET status = :status, attempts = attempts + 1,
                     locked_until = :locked_until, locked_by = :locked_by
                 WHERE id = :id",
                params! {
                    "status" => PROCESSING,
                    "locked_until" => now + LEASE_SECS,
                    "locked_by" => worker_token,
                    "id" => delivery_id,
                },
            )?;
        }
        claimed.push(Claimed {
            delivery_id,
            consumer: consumer.to_string(),
            attempt: attempts + 1,
            event: Event {
                id: Uuid::parse_str(&event_id)?,
                org_id: Uuid::parse_str(&org_id)?,
                event_type,
                payload: serde_json::from_str(&payload)?,
                created_at,
            },
            exhausted,
        });
    }
    tx.commit()?;
    Ok(claimed)
}

/// Mark a claimed delivery finished. The `locked_by` check means a worker
/// whose lease already expired can't overwrite the worker that took over.
pub fn complete(
    conn: &mut PooledConn,
    delivery_id: u64,
    worker_token: &str,
) -> Result<(), Box<dyn Error>> {
    conn.exec_drop(
        "UPDATE EventDeliveries
         SET status = :status, completed_at = :now, locked_until = NULL, locked_by = NULL,
             last_error = NULL
         WHERE id = :id AND status = 'PROCESSING' AND locked_by = :token",
        params! {
            "status" => DONE,
            "now" => now_unix(),
            "id" => delivery_id,
            "token" => worker_token,
        },
    )?;
    Ok(())
}

/// Delay before retrying after failed attempt number `attempt` (1-based).
pub fn backoff_secs(attempt: u32) -> i64 {
    let doublings = attempt.saturating_sub(1).min(16);
    (BASE_BACKOFF_SECS << doublings).min(MAX_BACKOFF_SECS)
}

/// Record a failed attempt: schedule a retry, or mark the delivery `DEAD` once
/// its attempts run out. Returns whether it is now dead.
pub fn fail(
    conn: &mut PooledConn,
    delivery_id: u64,
    worker_token: &str,
    attempt: u32,
    error: &str,
) -> Result<bool, Box<dyn Error>> {
    let dead = attempt >= MAX_ATTEMPTS;
    let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
    conn.exec_drop(
        "UPDATE EventDeliveries
         SET status = :status, next_attempt_at = :next, locked_until = NULL, locked_by = NULL,
             last_error = :error
         WHERE id = :id AND status = 'PROCESSING' AND locked_by = :token",
        params! {
            "status" => if dead { DEAD } else { PENDING },
            "next" => now_unix() + backoff_secs(attempt),
            "error" => error,
            "id" => delivery_id,
            "token" => worker_token,
        },
    )?;
    Ok(dead)
}

/// Delete events older than `cutoff` whose deliveries have all finished
/// successfully. `DEAD` ones are kept so they can be investigated.
pub fn purge_finished(conn: &mut PooledConn, cutoff: i64) -> Result<u64, Box<dyn Error>> {
    conn.exec_drop(
        "DELETE FROM Events
         WHERE created_at < :cutoff
           AND NOT EXISTS (
               SELECT 1 FROM EventDeliveries d
               WHERE d.event_id = Events.id AND d.status <> :done
           )",
        params! { "cutoff" => cutoff, "done" => DONE },
    )?;
    Ok(conn.affected_rows())
}
