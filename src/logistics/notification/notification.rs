//! Dispatch notifications to the customer and driver.
//!
//! When a dispatch is created, and again when it's delivered, the system
//! records a notification for each party it should tell and publishes an
//! event, in the same transaction as the dispatch change. The background
//! [`crate::logistics::notification::consumer`] then writes the AI copy and
//! sends it via [`crate::logistics::notification::delivery`], with retries.
//! See `docs/async-events.md`. Every row lands with status `QUEUED`
//! (ready to send), `SKIPPED` (no phone/email on file for that party),
//! `SENT` (delivery succeeded), or `FAILED` (every retry was used up). A `QUEUED` row whose channel has no
//! provider credentials configured is left `QUEUED` rather than `FAILED` —
//! nothing was actually attempted, and it's ready to send once configured.
//! See `docs/notifications.md`.

use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::driver::driver::Driver;
use crate::logistics::events;
use crate::logistics::events::queue::EventType;
use crate::logistics::vehicle::vehicle::Vehicle;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What happened to the dispatch that we're telling someone about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotificationEvent {
    DispatchCreated,
    DispatchDelivered,
    /// A dispatch is still `IN_TRANSIT` past
    /// [`crate::logistics::dispatch::dispatch::PROMISED_DELIVERY_HOURS`] since
    /// it was created. Recorded by
    /// [`crate::logistics::notification::delay_alerts::scan_and_alert`].
    DispatchRunningLate,
}

impl NotificationEvent {
    fn as_str(&self) -> &'static str {
        match self {
            NotificationEvent::DispatchCreated => "DISPATCH_CREATED",
            NotificationEvent::DispatchDelivered => "DISPATCH_DELIVERED",
            NotificationEvent::DispatchRunningLate => "DISPATCH_RUNNING_LATE",
        }
    }

    /// The email subject line for this event. Only used for
    /// [`NotificationChannel::Email`] — SMS has no subject.
    pub(crate) fn email_subject(&self) -> &'static str {
        match self {
            NotificationEvent::DispatchCreated => "Your order is on its way",
            NotificationEvent::DispatchDelivered => "Your order has been delivered",
            NotificationEvent::DispatchRunningLate => "Your order is running behind schedule",
        }
    }
}

/// How a notification would be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotificationChannel {
    Sms,
    Email,
}

impl NotificationChannel {
    fn as_str(&self) -> &'static str {
        match self {
            NotificationChannel::Sms => "SMS",
            NotificationChannel::Email => "EMAIL",
        }
    }
    fn from_str(s: &str) -> Self {
        match s {
            "EMAIL" => NotificationChannel::Email,
            _ => NotificationChannel::Sms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotificationStatus {
    /// A message ready for a provider to send. Also the resting state for a
    /// channel with no provider configured — an attempt was never made.
    Queued,
    /// No phone or email on file for this party — nothing to send.
    Skipped,
    /// Delivery succeeded.
    Sent,
    /// Delivery was attempted and the provider reported an error.
    Failed,
}

impl NotificationStatus {
    fn as_str(&self) -> &'static str {
        match self {
            NotificationStatus::Queued => "QUEUED",
            NotificationStatus::Skipped => "SKIPPED",
            NotificationStatus::Sent => "SENT",
            NotificationStatus::Failed => "FAILED",
        }
    }
    fn from_str(s: &str) -> Self {
        match s {
            "SKIPPED" => NotificationStatus::Skipped,
            "SENT" => NotificationStatus::Sent,
            "FAILED" => NotificationStatus::Failed,
            _ => NotificationStatus::Queued,
        }
    }
}

/// One recorded notification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Notification {
    pub id: Uuid,
    pub org_id: Uuid,
    pub dispatch_id: Uuid,
    pub event: NotificationEvent,
    pub channel: NotificationChannel,
    /// `"customer"` or `"driver"`.
    pub recipient_kind: String,
    /// The phone number or email address, or a placeholder when `SKIPPED`.
    pub recipient: String,
    pub body: String,
    pub status: NotificationStatus,
    pub created_at: i64,
}

type Row = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
);

const SELECT_COLS: &str =
    "id, org_id, dispatch_id, event, channel, recipient_kind, recipient, body, status, created_at";

impl Notification {
    pub fn ensure_table(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS Notifications (
                id VARCHAR(36) PRIMARY KEY,
                seq BIGINT NOT NULL AUTO_INCREMENT UNIQUE,
                org_id VARCHAR(36) NOT NULL,
                dispatch_id VARCHAR(36) NOT NULL,
                event VARCHAR(32) NOT NULL,
                channel VARCHAR(16) NOT NULL,
                recipient_kind VARCHAR(16) NOT NULL,
                recipient VARCHAR(255) NOT NULL,
                body TEXT NOT NULL,
                status VARCHAR(16) NOT NULL,
                created_at BIGINT NOT NULL,
                CONSTRAINT fk_notification_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
            )",
        )?;
        // `seq` (a stable newest-first ordering key) was added after the table
        // first shipped; back-fill it onto a local database that predates it.
        let has_seq: Option<i64> = conn.exec_first(
            "SELECT 1 FROM information_schema.columns
             WHERE table_schema = DATABASE() AND table_name = 'Notifications'
               AND column_name = 'seq'",
            (),
        )?;
        if has_seq.is_none() {
            conn.query_drop(
                "ALTER TABLE Notifications ADD COLUMN seq BIGINT NOT NULL AUTO_INCREMENT UNIQUE",
            )?;
        }
        Ok(())
    }

    fn row_to_notification(
        (
            id,
            org_id,
            dispatch_id,
            event,
            channel,
            recipient_kind,
            recipient,
            body,
            status,
            created_at,
        ): Row,
    ) -> Self {
        Notification {
            id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
            org_id: Uuid::parse_str(&org_id).unwrap_or_else(|_| Uuid::new_v4()),
            dispatch_id: Uuid::parse_str(&dispatch_id).unwrap_or_else(|_| Uuid::new_v4()),
            event: match event.as_str() {
                "DISPATCH_DELIVERED" => NotificationEvent::DispatchDelivered,
                "DISPATCH_RUNNING_LATE" => NotificationEvent::DispatchRunningLate,
                _ => NotificationEvent::DispatchCreated,
            },
            channel: NotificationChannel::from_str(&channel),
            recipient_kind,
            recipient,
            body,
            status: NotificationStatus::from_str(&status),
            created_at,
        }
    }

    fn insert(conn: &mut impl Queryable, n: &Notification) -> Result<(), Box<dyn Error>> {
        conn.exec_drop(
            "INSERT INTO Notifications (id, org_id, dispatch_id, event, channel, recipient_kind, recipient, body, status, created_at)
             VALUES (:id, :org_id, :dispatch_id, :event, :channel, :recipient_kind, :recipient, :body, :status, :created_at)",
            params! {
                "id" => n.id.to_string(),
                "org_id" => n.org_id.to_string(),
                "dispatch_id" => n.dispatch_id.to_string(),
                "event" => n.event.as_str(),
                "channel" => n.channel.as_str(),
                "recipient_kind" => &n.recipient_kind,
                "recipient" => &n.recipient,
                "body" => &n.body,
                "status" => n.status.as_str(),
                "created_at" => n.created_at,
            },
        )?;
        Ok(())
    }

    /// The best channel + address to reach a customer on: email if we have it,
    /// otherwise SMS, otherwise nothing.
    fn customer_target(customer: &Customer) -> Option<(NotificationChannel, String)> {
        if let Some(email) = customer.email.as_deref().filter(|s| !s.is_empty()) {
            Some((NotificationChannel::Email, email.to_string()))
        } else {
            customer
                .phone
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|phone| (NotificationChannel::Sms, phone.to_string()))
        }
    }

    fn build(
        org_id: Uuid,
        dispatch_id: Uuid,
        event: NotificationEvent,
        recipient_kind: &str,
        target: Option<(NotificationChannel, String)>,
        body: String,
    ) -> Notification {
        let (channel, recipient, status) = match target {
            Some((ch, to)) => (ch, to, NotificationStatus::Queued),
            None => (
                NotificationChannel::Sms,
                "(no contact on file)".to_string(),
                NotificationStatus::Skipped,
            ),
        };
        Notification {
            id: Uuid::new_v4(),
            org_id,
            dispatch_id,
            event,
            channel,
            recipient_kind: recipient_kind.to_string(),
            recipient,
            body,
            status,
            created_at: now_unix(),
        }
    }

    /// Create the notification and queue tables. Must run before the
    /// transaction opens: MySQL implicitly commits on `CREATE TABLE`.
    pub fn prepare_tables(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
        Self::ensure_table(conn)?;
        events::queue::ensure_tables(conn)
    }

    /// The phone of the active driver assigned to `reg`, if any. A failed
    /// lookup just means the driver isn't told.
    pub fn driver_phone_for_vehicle(reg: &str, org_id: Uuid) -> Option<String> {
        let vehicle = Vehicle::list_by_org(org_id)
            .ok()?
            .into_iter()
            .find(|v| v.registration_number == reg)?;
        let driver_id = vehicle.assigned_driver_id?;
        Driver::get_by_id(driver_id).ok().flatten().map(|d| d.phone)
    }

    /// Record the "your shipment is on its way" notifications (customer and
    /// driver) and publish the event that sends them, in the caller's
    /// transaction. Bodies start as a fixed template; the consumer replaces
    /// them with AI copy when that's configured. Call [`Self::index_best_effort`]
    /// and `events::worker::wake` after committing.
    pub fn enqueue_dispatch_created(
        tx: &mut Transaction<'_>,
        org_id: Uuid,
        dispatch_id: Uuid,
        customer: &Customer,
        driver_phone: Option<&str>,
    ) -> Result<Vec<Notification>, Box<dyn Error>> {
        let short_id = &dispatch_id.to_string()[..8];
        let driver_target = driver_phone
            .filter(|s| !s.is_empty())
            .map(|p| (NotificationChannel::Sms, p.to_string()));
        let notifs = vec![
            Self::build(
                org_id,
                dispatch_id,
                NotificationEvent::DispatchCreated,
                "customer",
                Self::customer_target(customer),
                format!(
                    "Hi {}, your order (ref {short_id}) has been dispatched and is on its way.",
                    customer.name
                ),
            ),
            Self::build(
                org_id,
                dispatch_id,
                NotificationEvent::DispatchCreated,
                "driver",
                driver_target,
                format!(
                    "New trip assigned: dispatch {short_id} to {}.",
                    customer.name
                ),
            ),
        ];
        Self::insert_all(tx, &notifs)?;
        events::queue::publish(
            tx,
            org_id,
            EventType::DispatchCreated,
            serde_json::json!({
                "dispatch_id": dispatch_id,
                "customer_name": customer.name,
            }),
            None,
        )?;
        Ok(notifs)
    }

    /// Record the "your shipment was delivered" notification and publish its
    /// event, in the caller's transaction.
    pub fn enqueue_dispatch_delivered(
        tx: &mut Transaction<'_>,
        org_id: Uuid,
        dispatch_id: Uuid,
        customer: &Customer,
    ) -> Result<Vec<Notification>, Box<dyn Error>> {
        let short_id = &dispatch_id.to_string()[..8];
        let notifs = vec![Self::build(
            org_id,
            dispatch_id,
            NotificationEvent::DispatchDelivered,
            "customer",
            Self::customer_target(customer),
            format!(
                "Hi {}, your order (ref {short_id}) has been delivered. Thank you!",
                customer.name
            ),
        )];
        Self::insert_all(tx, &notifs)?;
        events::queue::publish(
            tx,
            org_id,
            EventType::DispatchDelivered,
            serde_json::json!({
                "dispatch_id": dispatch_id,
                "customer_name": customer.name,
            }),
            None,
        )?;
        Ok(notifs)
    }

    /// Record the "running behind schedule" notification and publish its
    /// event, at most once per dispatch however many scans race: the event's
    /// unique dedupe key decides. Returns `None` if it was already published.
    pub fn enqueue_dispatch_running_late(
        tx: &mut Transaction<'_>,
        org_id: Uuid,
        dispatch_id: Uuid,
        customer: &Customer,
        hours_late: i64,
    ) -> Result<Option<Vec<Notification>>, Box<dyn Error>> {
        let published = events::queue::publish(
            tx,
            org_id,
            EventType::DispatchRunningLate,
            serde_json::json!({
                "dispatch_id": dispatch_id,
                "customer_name": customer.name,
                "hours_late": hours_late,
            }),
            Some(&format!("dispatch.running_late:{dispatch_id}")),
        )?;
        if !published {
            return Ok(None);
        }
        let short_id = &dispatch_id.to_string()[..8];
        let notifs = vec![Self::build(
            org_id,
            dispatch_id,
            NotificationEvent::DispatchRunningLate,
            "customer",
            Self::customer_target(customer),
            format!(
                "Hi {}, your order (ref {short_id}) is running about {hours_late}h behind its expected delivery time. We're sorry for the delay.",
                customer.name
            ),
        )];
        Self::insert_all(tx, &notifs)?;
        Ok(Some(notifs))
    }

    fn insert_all(tx: &mut Transaction<'_>, notifs: &[Notification]) -> Result<(), Box<dyn Error>> {
        for n in notifs {
            Self::insert(tx, n)?;
        }
        Ok(())
    }

    /// Index notifications for the "ask your data" assistant. Run after
    /// commit: the index writes on its own connection and couldn't see the
    /// row earlier. Never fails the caller.
    pub fn index_best_effort(notifs: &[Notification]) {
        for n in notifs {
            crate::logistics::ai::chunk::upsert_best_effort(
                n.org_id,
                crate::logistics::ai::chunk::ChunkKind::Notification,
                &n.id.to_string(),
                crate::logistics::ai::chunk::notification_chunk_text(n),
            );
        }
    }

    pub(crate) fn update_status(
        id: Uuid,
        status: NotificationStatus,
    ) -> Result<(), Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        conn.exec_drop(
            "UPDATE Notifications SET status = :status WHERE id = :id",
            params! { "status" => status.as_str(), "id" => id.to_string() },
        )?;
        Ok(())
    }

    /// Replace a row's body with its personalized copy and reindex it.
    pub(crate) fn update_body(n: &Notification, body: &str) -> Result<(), Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        conn.exec_drop(
            "UPDATE Notifications SET body = :body WHERE id = :id",
            params! { "body" => body, "id" => n.id.to_string() },
        )?;
        let mut updated = n.clone();
        updated.body = body.to_string();
        Self::index_best_effort(&[updated]);
        Ok(())
    }

    /// The rows for one dispatch event that still need sending.
    pub(crate) fn list_queued(
        dispatch_id: Uuid,
        event: NotificationEvent,
    ) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        let rows: Vec<Row> = conn.exec(
            format!(
                "SELECT {SELECT_COLS} FROM Notifications
                 WHERE dispatch_id = :dispatch_id AND event = :event AND status = 'QUEUED'
                 ORDER BY seq ASC"
            ),
            params! { "dispatch_id" => dispatch_id.to_string(), "event" => event.as_str() },
        )?;
        Ok(rows.into_iter().map(Self::row_to_notification).collect())
    }

    pub fn list_by_dispatch(dispatch_id: Uuid) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let rows: Vec<Row> = conn.exec(
            format!(
                "SELECT {SELECT_COLS} FROM Notifications WHERE dispatch_id = :dispatch_id
                 ORDER BY seq ASC"
            ),
            params! { "dispatch_id" => dispatch_id.to_string() },
        )?;
        Ok(rows.into_iter().map(Self::row_to_notification).collect())
    }

    pub fn list_by_org(org_id: Uuid, limit: u32) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let rows: Vec<Row> = conn.exec(
            format!(
                "SELECT {SELECT_COLS} FROM Notifications WHERE org_id = :org_id
                 ORDER BY seq DESC LIMIT :limit"
            ),
            params! { "org_id" => org_id.to_string(), "limit" => limit },
        )?;
        Ok(rows.into_iter().map(Self::row_to_notification).collect())
    }
}
