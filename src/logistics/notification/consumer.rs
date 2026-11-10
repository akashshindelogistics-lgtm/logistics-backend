//! Turns dispatch events into sent messages: writes the AI copy for each
//! `QUEUED` notification and sends it. Only `QUEUED` rows are touched, so a
//! retry skips anything already sent.

use crate::logistics::ai::notification_copy as copy;
use crate::logistics::events::queue::{Event, EventType};
use crate::logistics::events::worker::Consumer;
use crate::logistics::notification::delivery::{self, DeliveryError};
use crate::logistics::notification::notification::{
    Notification, NotificationEvent, NotificationStatus, with_tracking_link,
};
use futures_util::future::LocalBoxFuture;
use std::future::Future;
use uuid::Uuid;

pub const CONSUMER_NAME: &str = "notification_sender";

pub struct NotificationConsumer;

impl Consumer for NotificationConsumer {
    fn name(&self) -> &'static str {
        CONSUMER_NAME
    }

    fn handle<'a>(
        &'a self,
        event: &'a Event,
        attempt: u32,
    ) -> LocalBoxFuture<'a, Result<(), String>> {
        Box::pin(process_event(event, attempt, send))
    }

    fn on_dead_letter(&self, event: &Event) {
        if let Err(err) = mark_undelivered_failed(event) {
            eprintln!("notification consumer: dead-letter update failed: {err}");
        }
    }
}

async fn send(n: Notification) -> Result<(), DeliveryError> {
    delivery::deliver(n.channel, &n.recipient, n.event.email_subject(), &n.body).await
}

fn notification_event(event: &Event) -> Result<NotificationEvent, String> {
    match EventType::parse(&event.event_type) {
        Some(EventType::DispatchCreated) => Ok(NotificationEvent::DispatchCreated),
        Some(EventType::DispatchDelivered) => Ok(NotificationEvent::DispatchDelivered),
        Some(EventType::DispatchRunningLate) => Ok(NotificationEvent::DispatchRunningLate),
        None => Err(format!("unknown event type {}", event.event_type)),
    }
}

fn dispatch_id(event: &Event) -> Result<Uuid, String> {
    event.payload["dispatch_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| "event payload has no valid dispatch_id".to_string())
}

/// `Err` from the model means "keep the template text".
async fn personalized_body(n: &Notification, event: &Event) -> Option<String> {
    let name = event.payload["customer_name"].as_str()?;
    let dispatch_id = n.dispatch_id.to_string();
    let short_id = &dispatch_id[..8];
    let result = match (n.event, n.recipient_kind.as_str()) {
        (NotificationEvent::DispatchCreated, "driver") => {
            copy::generate_dispatch_created_driver_body(name, short_id).await
        }
        (NotificationEvent::DispatchCreated, _) => {
            copy::generate_dispatch_created_customer_body(name, short_id).await
        }
        (NotificationEvent::DispatchDelivered, _) => {
            copy::generate_dispatch_delivered_customer_body(name, short_id).await
        }
        (NotificationEvent::DispatchRunningLate, _) => {
            let hours_late = event.payload["hours_late"].as_i64().unwrap_or(0);
            copy::generate_dispatch_running_late_customer_body(name, short_id, hours_late).await
        }
    };
    // The driver's message never carries the customer's link.
    let link = (n.recipient_kind != "driver")
        .then(|| event.payload["tracking_url"].as_str())
        .flatten();
    result.ok().map(|body| with_tracking_link(body, link))
}

/// Send every `QUEUED` notification for this event. One row failing doesn't
/// stop the others; any failure returns `Err`, which retries only what's left.
/// A row with no provider configured stays `QUEUED` and isn't a failure.
pub(crate) async fn process_event<S, F>(event: &Event, attempt: u32, send: S) -> Result<(), String>
where
    S: Fn(Notification) -> F,
    F: Future<Output = Result<(), DeliveryError>>,
{
    let kind = notification_event(event)?;
    let queued = Notification::list_queued(dispatch_id(event)?, kind).map_err(|e| e.to_string())?;

    let mut failures = Vec::new();
    for mut n in queued {
        // First attempt only: a retry shouldn't pay for the copy again.
        if attempt == 1
            && let Some(body) = personalized_body(&n, event).await
        {
            Notification::update_body(&n, &body).map_err(|e| e.to_string())?;
            n.body = body;
        }

        let id = n.id;
        match send(n).await {
            Ok(()) => {
                // Logged, not retried: the message has gone, and a retry
                // would send it a second time.
                if let Err(err) = Notification::update_status(id, NotificationStatus::Sent) {
                    eprintln!("notification consumer: sent {id} but couldn't mark it: {err}");
                }
            }
            Err(DeliveryError::NotConfigured) => {}
            Err(err) => failures.push(format!("{id}: {err}")),
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn mark_undelivered_failed(event: &Event) -> Result<(), String> {
    let kind = notification_event(event)?;
    let queued = Notification::list_queued(dispatch_id(event)?, kind).map_err(|e| e.to_string())?;
    for n in queued {
        Notification::update_status(n.id, NotificationStatus::Failed).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::customer::customer::Customer;
    use crate::logistics::db::connection::DbConnection;
    use crate::logistics::events::queue;
    use crate::logistics::orgs::orgs::Organization;
    use crate::logistics::test_support::TestDb;
    use mysql::*;
    use std::cell::RefCell;

    fn setup() -> (Uuid, Customer, Uuid) {
        let org = Organization::create_organization("Send Co", "1 St").expect("org");
        let mut customer = Customer::create_customer(org.id, "Asha", "1 Rd").expect("customer");
        customer
            .set_contact(None, Some("asha@example.com".into()))
            .expect("contact");
        (org.id, customer, Uuid::new_v4())
    }

    fn enqueue(org_id: Uuid, customer: &Customer, dispatch_id: Uuid) -> Vec<Notification> {
        let mut conn = DbConnection::from_env().get_connection().expect("conn");
        Notification::prepare_tables(&mut conn).expect("tables");
        let mut tx = conn.start_transaction(TxOpts::default()).expect("tx");
        let rows = Notification::enqueue_dispatch_created(
            &mut tx,
            org_id,
            dispatch_id,
            customer,
            Some("+91 98888 22222"),
        )
        .expect("enqueue");
        tx.commit().expect("commit");
        rows
    }

    fn claimed_event() -> Event {
        let mut conn = DbConnection::from_env().get_connection().expect("conn");
        queue::claim(&mut conn, CONSUMER_NAME, "w", 1)
            .expect("claim")
            .remove(0)
            .event
    }

    fn statuses(dispatch_id: Uuid) -> Vec<NotificationStatus> {
        Notification::list_by_dispatch(dispatch_id)
            .expect("list")
            .into_iter()
            .map(|n| n.status)
            .collect()
    }

    /// Attempt 2 skips AI copy, so these never call a real model.
    const RETRY: u32 = 2;

    #[actix_web::test]
    async fn sends_every_queued_row_and_marks_it_sent() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = setup();
        enqueue(org, &customer, dispatch);

        let sent = RefCell::new(Vec::new());
        process_event(&claimed_event(), RETRY, |n| {
            sent.borrow_mut().push(n.recipient.clone());
            async { Ok(()) }
        })
        .await
        .expect("process");

        assert_eq!(sent.borrow().len(), 2);
        assert_eq!(statuses(dispatch), vec![NotificationStatus::Sent; 2]);
    }

    #[actix_web::test]
    async fn a_retry_only_resends_what_failed() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = setup();
        enqueue(org, &customer, dispatch);
        let event = claimed_event();

        let err = process_event(&event, RETRY, |n| async move {
            if n.recipient_kind == "driver" {
                Err(DeliveryError::ProviderError("sms down".into()))
            } else {
                Ok(())
            }
        })
        .await
        .expect_err("a failed row fails the event");
        assert!(err.contains("sms down"), "{err}");

        let resent = RefCell::new(Vec::new());
        process_event(&event, RETRY, |n| {
            resent.borrow_mut().push(n.recipient_kind.clone());
            async { Ok(()) }
        })
        .await
        .expect("retry");

        assert_eq!(*resent.borrow(), vec!["driver".to_string()]);
        assert_eq!(statuses(dispatch), vec![NotificationStatus::Sent; 2]);
    }

    #[actix_web::test]
    async fn an_unconfigured_provider_leaves_rows_queued_without_failing() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = setup();
        enqueue(org, &customer, dispatch);

        process_event(&claimed_event(), RETRY, |_| async {
            Err(DeliveryError::NotConfigured)
        })
        .await
        .expect("not a failure");

        assert_eq!(statuses(dispatch), vec![NotificationStatus::Queued; 2]);
    }

    #[actix_web::test]
    async fn skipped_rows_are_never_sent() {
        let _db = TestDb::create();
        let (org, _, dispatch) = setup();
        let no_contact = Customer::create_customer(org, "None", "2 Rd").expect("customer");
        let mut conn = DbConnection::from_env().get_connection().expect("conn");
        Notification::prepare_tables(&mut conn).expect("tables");
        let mut tx = conn.start_transaction(TxOpts::default()).expect("tx");
        Notification::enqueue_dispatch_created(&mut tx, org, dispatch, &no_contact, None)
            .expect("enqueue");
        tx.commit().expect("commit");

        process_event(&claimed_event(), RETRY, |_| async {
            panic!("nothing should be sent")
        })
        .await
        .expect("process");
        assert_eq!(statuses(dispatch), vec![NotificationStatus::Skipped; 2]);
    }

    #[actix_web::test]
    async fn the_first_attempt_keeps_the_template_when_ai_is_unavailable() {
        // SAFETY: matches the other notification tests that clear this key.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let _db = TestDb::create();
        let (org, customer, dispatch) = setup();
        let original = enqueue(org, &customer, dispatch);

        let bodies = RefCell::new(Vec::new());
        process_event(&claimed_event(), 1, |n| {
            bodies.borrow_mut().push(n.body.clone());
            async { Ok(()) }
        })
        .await
        .expect("process");

        assert_eq!(bodies.borrow()[0], original[0].body);
    }

    #[actix_web::test]
    async fn dead_lettering_marks_only_undelivered_rows_failed() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = setup();
        let rows = enqueue(org, &customer, dispatch);
        Notification::update_status(rows[0].id, NotificationStatus::Sent).expect("sent");

        NotificationConsumer.on_dead_letter(&claimed_event());

        assert_eq!(
            statuses(dispatch),
            vec![NotificationStatus::Sent, NotificationStatus::Failed]
        );
    }

    #[actix_web::test]
    async fn a_malformed_payload_is_an_error_not_a_panic() {
        let event = Event {
            id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            event_type: EventType::DispatchCreated.as_str().to_string(),
            payload: serde_json::json!({}),
            created_at: 0,
        };
        assert!(
            process_event(&event, RETRY, |_| async { Ok(()) })
                .await
                .is_err()
        );
    }
}
