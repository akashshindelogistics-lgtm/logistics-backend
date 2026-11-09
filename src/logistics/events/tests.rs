use super::queue::{self, EventType, MAX_ATTEMPTS};
use super::worker::{Consumer, drain_once};
use crate::logistics::db::connection::DbConnection;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::test_support::TestDb;
use futures_util::future::LocalBoxFuture;
use mysql::prelude::*;
use mysql::*;
use std::cell::Cell;
use std::rc::Rc;
use uuid::Uuid;

const NAME: &str = "test_consumer";

fn conn() -> PooledConn {
    DbConnection::from_env().get_connection().expect("conn")
}

fn org() -> Organization {
    Organization::create_organization("Events Co", "1 St").expect("org")
}

fn publish(org_id: Uuid, dedupe: Option<&str>, consumers: &[&str]) -> bool {
    let mut c = conn();
    let mut tx = c.start_transaction(TxOpts::default()).expect("tx");
    let published = queue::publish_to(
        &mut tx,
        org_id,
        "test.event",
        serde_json::json!({"n": 1}),
        dedupe,
        consumers,
    )
    .expect("publish");
    tx.commit().expect("commit");
    published
}

/// (status, attempts, next_attempt_at, last_error) of the only delivery.
fn only_delivery() -> (String, u32, i64, Option<String>) {
    conn()
        .query_first("SELECT status, attempts, next_attempt_at, last_error FROM EventDeliveries")
        .expect("query")
        .expect("one delivery")
}

fn make_due() {
    conn()
        .query_drop("UPDATE EventDeliveries SET next_attempt_at = 0")
        .expect("make due");
}

#[derive(Clone, Default)]
struct Probe {
    calls: Rc<Cell<u32>>,
    dead: Rc<Cell<u32>>,
    last_attempt: Rc<Cell<u32>>,
}

#[derive(Clone, Copy)]
enum Behaviour {
    Succeed,
    Fail,
    Panic,
}

struct TestConsumer {
    probe: Probe,
    behaviour: Behaviour,
}

impl Consumer for TestConsumer {
    fn name(&self) -> &'static str {
        NAME
    }
    fn handle<'a>(
        &'a self,
        _event: &'a queue::Event,
        attempt: u32,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        self.probe.calls.set(self.probe.calls.get() + 1);
        self.probe.last_attempt.set(attempt);
        let behaviour = self.behaviour;
        Box::pin(async move {
            match behaviour {
                Behaviour::Succeed => Ok(()),
                Behaviour::Fail => Err("provider down".to_string()),
                Behaviour::Panic => panic!("consumer bug"),
            }
        })
    }
    fn on_dead_letter(&self, _event: &queue::Event) {
        self.probe.dead.set(self.probe.dead.get() + 1);
    }
}

fn consumers(probe: &Probe, behaviour: Behaviour) -> Vec<Box<dyn Consumer>> {
    vec![Box::new(TestConsumer {
        probe: probe.clone(),
        behaviour,
    })]
}

#[actix_web::test]
async fn a_published_event_is_consumed_once() {
    let _db = TestDb::create();
    publish(org().id, None, &[NAME]);
    let probe = Probe::default();
    let cs = consumers(&probe, Behaviour::Succeed);

    assert_eq!(drain_once(&cs, "w").await.expect("drain"), 1);
    assert_eq!(probe.calls.get(), 1);
    assert_eq!(probe.last_attempt.get(), 1);
    assert_eq!(only_delivery().0, "DONE");

    assert_eq!(drain_once(&cs, "w").await.expect("second drain"), 0);
    assert_eq!(probe.calls.get(), 1);
}

#[actix_web::test]
async fn a_rolled_back_transaction_publishes_nothing() {
    let _db = TestDb::create();
    let org = org();
    {
        let mut c = conn();
        let mut tx = c.start_transaction(TxOpts::default()).expect("tx");
        queue::publish_to(
            &mut tx,
            org.id,
            "test.event",
            serde_json::json!({}),
            None,
            &[NAME],
        )
        .expect("publish");
        tx.rollback().expect("rollback");
    }
    let probe = Probe::default();
    assert_eq!(
        drain_once(&consumers(&probe, Behaviour::Succeed), "w")
            .await
            .expect("drain"),
        0
    );
}

#[actix_web::test]
async fn each_subscriber_gets_its_own_delivery() {
    let _db = TestDb::create();
    publish(org().id, None, &[NAME, "another_consumer"]);
    let probe = Probe::default();
    drain_once(&consumers(&probe, Behaviour::Succeed), "w")
        .await
        .expect("drain");

    let other: Option<String> = conn()
        .query_first("SELECT status FROM EventDeliveries WHERE consumer = 'another_consumer'")
        .expect("query");
    assert_eq!(other.as_deref(), Some("PENDING"));
}

#[actix_web::test]
async fn a_dedupe_key_publishes_only_once() {
    let _db = TestDb::create();
    let org = org();
    assert!(publish(org.id, Some("key"), &[NAME]));
    assert!(!publish(org.id, Some("key"), &[NAME]));
    assert!(publish(org.id, Some("other"), &[NAME]));

    let count: Option<i64> = conn()
        .query_first("SELECT COUNT(*) FROM EventDeliveries")
        .expect("count");
    assert_eq!(count, Some(2));
}

#[actix_web::test]
async fn a_failed_delivery_waits_for_backoff_then_retries() {
    let _db = TestDb::create();
    publish(org().id, None, &[NAME]);
    let probe = Probe::default();
    let cs = consumers(&probe, Behaviour::Fail);

    drain_once(&cs, "w").await.expect("drain");
    let (status, attempts, next, error) = only_delivery();
    assert_eq!((status.as_str(), attempts), ("PENDING", 1));
    assert!(next > queue::now_unix());
    assert_eq!(error.as_deref(), Some("provider down"));

    // Not due yet, so nothing is claimed.
    assert_eq!(drain_once(&cs, "w").await.expect("drain"), 0);
    assert_eq!(probe.calls.get(), 1);

    make_due();
    drain_once(&cs, "w").await.expect("drain");
    assert_eq!(probe.calls.get(), 2);
    assert_eq!(probe.last_attempt.get(), 2);
}

#[actix_web::test]
async fn retries_run_out_into_the_dead_letter_state() {
    let _db = TestDb::create();
    publish(org().id, None, &[NAME]);
    let probe = Probe::default();
    let cs = consumers(&probe, Behaviour::Fail);

    for _ in 0..MAX_ATTEMPTS {
        make_due();
        drain_once(&cs, "w").await.expect("drain");
    }
    assert_eq!(only_delivery().0, "DEAD");
    assert_eq!(probe.calls.get(), MAX_ATTEMPTS);
    assert_eq!(probe.dead.get(), 1);

    make_due();
    assert_eq!(drain_once(&cs, "w").await.expect("drain"), 0);
}

#[actix_web::test]
async fn a_panicking_consumer_counts_as_a_failed_attempt() {
    let _db = TestDb::create();
    publish(org().id, None, &[NAME]);
    let probe = Probe::default();

    drain_once(&consumers(&probe, Behaviour::Panic), "w")
        .await
        .expect("drain survives the panic");

    let (status, attempts, _, error) = only_delivery();
    assert_eq!((status.as_str(), attempts), ("PENDING", 1));
    assert_eq!(error.as_deref(), Some("consumer panicked"));
}

#[actix_web::test]
async fn a_live_claim_is_not_taken_but_an_expired_one_is() {
    let _db = TestDb::create();
    publish(org().id, None, &[NAME]);

    let mut c = conn();
    let first = queue::claim(&mut c, NAME, "dead-worker", 10).expect("claim");
    assert_eq!(first.len(), 1);
    assert!(
        queue::claim(&mut c, NAME, "other", 10)
            .expect("claim")
            .is_empty()
    );

    c.query_drop("UPDATE EventDeliveries SET locked_until = 0")
        .expect("expire");
    let second = queue::claim(&mut c, NAME, "new-worker", 10).expect("reclaim");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].attempt, 2);

    // The worker that lost the lease can't finish over the new owner.
    queue::complete(&mut c, first[0].delivery_id, "dead-worker").expect("late complete");
    assert_eq!(only_delivery().0, "PROCESSING");
    queue::complete(&mut c, second[0].delivery_id, "new-worker").expect("complete");
    assert_eq!(only_delivery().0, "DONE");
}

#[actix_web::test]
async fn a_delivery_that_keeps_crashing_its_worker_goes_dead() {
    let _db = TestDb::create();
    publish(org().id, None, &[NAME]);
    conn()
        .exec_drop(
            "UPDATE EventDeliveries SET status = 'PROCESSING', locked_until = 0, attempts = :n",
            params! { "n" => MAX_ATTEMPTS },
        )
        .expect("simulate crashes");

    let probe = Probe::default();
    drain_once(&consumers(&probe, Behaviour::Succeed), "w")
        .await
        .expect("drain");

    assert_eq!(probe.calls.get(), 0, "must not run it again");
    assert_eq!(probe.dead.get(), 1);
    assert_eq!(only_delivery().0, "DEAD");
}

#[actix_web::test]
async fn competing_claims_never_get_the_same_delivery() {
    let _db = TestDb::create();
    let org = org();
    for _ in 0..3 {
        publish(org.id, None, &[NAME]);
    }

    // The first claim holds its row locks while the second one runs.
    let mut holder = conn();
    let mut tx = holder.start_transaction(TxOpts::default()).expect("tx");
    let locked: Vec<u64> = tx
        .exec(
            "SELECT id FROM EventDeliveries ORDER BY id LIMIT 2 FOR UPDATE",
            (),
        )
        .expect("lock");

    let mut c = conn();
    let claimed = queue::claim(&mut c, NAME, "w2", 10).expect("claim skips locked rows");
    tx.rollback().expect("rollback");

    assert_eq!(claimed.len(), 1);
    assert!(!locked.contains(&claimed[0].delivery_id));
}

#[actix_web::test]
async fn deleting_an_org_removes_its_events() {
    let _db = TestDb::create();
    let org = org();
    publish(org.id, None, &[NAME]);
    org.remove_organization().expect("remove");

    let count: Option<i64> = conn()
        .query_first("SELECT COUNT(*) FROM EventDeliveries")
        .expect("count");
    assert_eq!(count, Some(0));
}

#[actix_web::test]
async fn purge_only_removes_old_fully_finished_events() {
    let _db = TestDb::create();
    let org = org();
    publish(org.id, Some("done-old"), &[NAME]);
    publish(org.id, Some("pending-old"), &[NAME]);
    publish(org.id, Some("done-new"), &[NAME]);

    let mut c = conn();
    c.query_drop(
        "UPDATE EventDeliveries d JOIN Events e ON e.id = d.event_id
         SET d.status = 'DONE' WHERE e.dedupe_key LIKE 'done-%'",
    )
    .expect("finish");
    c.query_drop("UPDATE Events SET created_at = 1 WHERE dedupe_key LIKE '%-old'")
        .expect("age");

    assert_eq!(queue::purge_finished(&mut c, 1000).expect("purge"), 1);
    let left: Vec<String> = c
        .query("SELECT dedupe_key FROM Events ORDER BY dedupe_key")
        .expect("left");
    assert_eq!(left, vec!["done-new", "pending-old"]);
}

#[test]
fn backoff_doubles_and_is_capped() {
    assert_eq!(queue::backoff_secs(1), 30);
    assert_eq!(queue::backoff_secs(2), 60);
    assert_eq!(queue::backoff_secs(3), 120);
    assert_eq!(queue::backoff_secs(40), 3600);
}

#[test]
fn event_types_round_trip_and_all_have_subscribers() {
    for t in [
        EventType::DispatchCreated,
        EventType::DispatchDelivered,
        EventType::DispatchRunningLate,
    ] {
        assert_eq!(EventType::parse(t.as_str()), Some(t));
        assert!(!t.subscribers().is_empty());
    }
    assert_eq!(EventType::parse("nope"), None);
}
