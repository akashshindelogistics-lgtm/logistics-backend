//! Runs consumers against the queue on dedicated OS threads. The MySQL driver
//! is blocking, so this work must stay away from the HTTP workers' threads.

use super::queue::{self, Claimed, Event};
use crate::logistics::db::connection::DbConnection;
use crate::logistics::notification::consumer::NotificationConsumer;
use crate::logistics::notification::delay_alerts;
use crate::logistics::server::metrics;
use crate::logistics::vehicle::location_history;
use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use std::error::Error;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

const BATCH_SIZE: u32 = 10;
/// Backstop for retries coming due. New events arrive through [`wake`].
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const DELAY_SCAN_INTERVAL: Duration = Duration::from_secs(15 * 60);
const RETENTION_SECS: i64 = 30 * 24 * 3600;
const DEFAULT_WORKERS: usize = 2;
const MAX_WORKERS: usize = 16;

/// Handles one event type family. Delivery is at-least-once, so `handle` must
/// be safe to run again for an event it has already seen.
pub trait Consumer {
    fn name(&self) -> &'static str;

    /// `Err` schedules a retry, with the message kept in `last_error`.
    fn handle<'a>(
        &'a self,
        event: &'a Event,
        attempt: u32,
    ) -> LocalBoxFuture<'a, Result<(), String>>;

    /// Called once when retries are exhausted.
    fn on_dead_letter(&self, _event: &Event) {}
}

/// Every consumer that runs in production.
pub fn registered_consumers() -> Vec<Box<dyn Consumer>> {
    vec![Box::new(NotificationConsumer)]
}

fn wake_signal() -> &'static Notify {
    static SIGNAL: OnceLock<Notify> = OnceLock::new();
    SIGNAL.get_or_init(Notify::new)
}

/// Tell a worker there's new work. Call after commit, so it can see the rows.
pub fn wake() {
    wake_signal().notify_one();
}

/// Claim and process one batch per consumer, returning how many deliveries
/// were claimed. Tests call this directly on the test thread.
pub async fn drain_once(
    consumers: &[Box<dyn Consumer>],
    worker_token: &str,
) -> Result<usize, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    queue::ensure_tables(&mut conn)?;

    let mut processed = 0;
    for consumer in consumers {
        let claimed = queue::claim(&mut conn, consumer.name(), worker_token, BATCH_SIZE)?;
        processed += claimed.len();
        for delivery in &claimed {
            if let Err(err) = process(&mut conn, consumer.as_ref(), delivery, worker_token).await {
                eprintln!(
                    "event worker: delivery {} bookkeeping failed: {err}",
                    delivery.delivery_id
                );
            }
        }
    }
    Ok(processed)
}

async fn process(
    conn: &mut mysql::PooledConn,
    consumer: &dyn Consumer,
    delivery: &Claimed,
    worker_token: &str,
) -> Result<(), Box<dyn Error>> {
    if delivery.exhausted {
        consumer.on_dead_letter(&delivery.event);
        return Ok(());
    }

    // A panicking consumer must count as a failed attempt, not kill the worker.
    let outcome = AssertUnwindSafe(consumer.handle(&delivery.event, delivery.attempt))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err("consumer panicked".to_string()));

    match outcome {
        Ok(()) => queue::complete(conn, delivery.delivery_id, worker_token),
        Err(error) => {
            eprintln!(
                "event worker: {} failed {} (attempt {}): {error}",
                consumer.name(),
                delivery.event.event_type,
                delivery.attempt
            );
            let dead = queue::fail(
                conn,
                delivery.delivery_id,
                worker_token,
                delivery.attempt,
                &error,
            )?;
            if dead {
                consumer.on_dead_letter(&delivery.event);
            }
            Ok(())
        }
    }
}

/// `EVENT_WORKERS_ENABLED`: only an explicit `true` or `1` turns workers on.
fn parse_enabled(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("true" | "1")
    )
}

/// `EVENT_WORKER_COUNT`, clamped so a typo can't spawn thousands of threads.
fn parse_worker_count(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_WORKERS)
        .clamp(1, MAX_WORKERS)
}

fn spawn_runtime_thread<F, Fut>(name: String, run: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let spawned = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(run()),
                Err(err) => eprintln!("event worker: failed to start a runtime: {err}"),
            }
        });
    if let Err(err) = spawned {
        eprintln!("event worker: failed to start thread {name}: {err}");
    }
}

async fn consume_forever() {
    let consumers = registered_consumers();
    let worker_token = Uuid::new_v4().to_string();
    loop {
        match drain_once(&consumers, &worker_token).await {
            Ok(0) => {
                let _ = tokio::time::timeout(POLL_INTERVAL, wake_signal().notified()).await;
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("event worker: {err}");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

async fn scan_forever() {
    loop {
        tokio::time::sleep(DELAY_SCAN_INTERVAL).await;
        if let Err(err) = delay_alerts::scan_and_alert().await {
            metrics::DELAY_ALERT_SCANS_FAILED.fetch_add(1, Ordering::Relaxed);
            eprintln!("delay-alert scan failed: {err}");
        } else {
            metrics::DELAY_ALERT_SCANS_OK.fetch_add(1, Ordering::Relaxed);
        }
        let purged = DbConnection::from_env().get_connection().and_then(|mut c| {
            queue::ensure_tables(&mut c)?;
            queue::purge_finished(&mut c, queue::now_unix() - RETENTION_SECS)
        });
        if let Err(err) = purged {
            eprintln!("event retention failed: {err}");
        }
        if let Err(err) = location_history::prune_expired() {
            eprintln!("location history retention failed: {err}");
        }
    }
}

/// Start the consumers and the delay-alert scan, if enabled. Off by default: a
/// dev server on the local database must not message real customers.
pub fn start_from_env() {
    if !parse_enabled(std::env::var("EVENT_WORKERS_ENABLED").ok().as_deref()) {
        println!("Event workers are off (set EVENT_WORKERS_ENABLED=true to enable them).");
        return;
    }
    let count = parse_worker_count(std::env::var("EVENT_WORKER_COUNT").ok().as_deref());
    println!("Starting {count} event worker(s).");
    for i in 0..count {
        spawn_runtime_thread(format!("event-worker-{i}"), consume_forever);
    }
    spawn_runtime_thread("event-scanner".to_string(), scan_forever);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_need_an_explicit_opt_in() {
        for off in [None, Some(""), Some("false"), Some("0"), Some("yes")] {
            assert!(!parse_enabled(off), "{off:?}");
        }
        for on in [Some("true"), Some("TRUE"), Some(" 1 ")] {
            assert!(parse_enabled(on), "{on:?}");
        }
    }

    #[test]
    fn worker_count_defaults_and_clamps() {
        assert_eq!(parse_worker_count(None), DEFAULT_WORKERS);
        assert_eq!(parse_worker_count(Some("nope")), DEFAULT_WORKERS);
        assert_eq!(parse_worker_count(Some("0")), 1);
        assert_eq!(parse_worker_count(Some("4")), 4);
        assert_eq!(parse_worker_count(Some("9999")), MAX_WORKERS);
    }

    #[test]
    fn production_consumers_have_unique_names() {
        let consumers = registered_consumers();
        let mut names: Vec<_> = consumers.iter().map(|c| c.name()).collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total);
    }
}
