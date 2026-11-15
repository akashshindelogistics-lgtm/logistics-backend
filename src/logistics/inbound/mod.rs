//! Inbound messages: an email or WhatsApp message from outside the org,
//! stored as data for the order-intake agent. See `docs/inbound-channels.md`.
//!
//! Everything in a message is written by someone outside the org, so it's only
//! ever shown to an agent as quoted data. What an agent can do about it is
//! limited by the agent tools and the autonomy policy, not by this text.

use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;
use uuid::Uuid;

pub const MAX_BODY_CHARS: usize = 8_000;
pub const MAX_SENDER_CHARS: usize = 255;
pub const MAX_SUBJECT_CHARS: usize = 255;
pub const MAX_EXTERNAL_ID_CHARS: usize = 191;

/// Waiting for the order-intake agent.
pub const RECEIVED: &str = "RECEIVED";
/// A dispatch was made from it.
pub const PROCESSED: &str = "PROCESSED";
/// The agent raised an alert about it for a person.
pub const ALERTED: &str = "ALERTED";
/// The agent decided it needs nothing, such as a thank-you.
pub const IGNORED: &str = "IGNORED";
/// A person rejected the dispatch proposed from it.
pub const REJECTED: &str = "REJECTED";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Channel {
    Email,
    Whatsapp,
}

impl Channel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Channel::Email => "EMAIL",
            Channel::Whatsapp => "WHATSAPP",
        }
    }

    fn from_db(s: &str) -> Self {
        if s == "WHATSAPP" {
            Channel::Whatsapp
        } else {
            Channel::Email
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InboundMessage {
    pub id: Uuid,
    pub org_id: Uuid,
    pub channel: Channel,
    /// The sender's email address or phone number, as the channel reported it.
    pub sender: String,
    pub subject: Option<String>,
    pub body: String,
    /// The org's customer this sender matched by email or phone. `null` when
    /// nobody matched, and then no agent can raise a dispatch from it.
    pub customer_id: Option<Uuid>,
    /// `RECEIVED` until it's dealt with, then `PROCESSED` (a dispatch was made),
    /// `ALERTED`, `IGNORED` or `REJECTED` (a person said no to its proposal).
    pub status: String,
    pub received_at: i64,
    /// The provider's message id, when the gateway sent one. A retried delivery
    /// with the same id returns this message instead of storing a second one.
    pub external_id: Option<String>,
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn ensure_tables(conn: &mut PooledConn) -> Result<(), String> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS InboundMessages (
            id VARCHAR(36) PRIMARY KEY,
            seq BIGINT NOT NULL AUTO_INCREMENT UNIQUE,
            org_id VARCHAR(36) NOT NULL,
            channel VARCHAR(16) NOT NULL,
            sender VARCHAR(255) NOT NULL,
            subject VARCHAR(255) NULL,
            body TEXT NOT NULL,
            customer_id VARCHAR(36) NULL,
            status VARCHAR(16) NOT NULL,
            received_at BIGINT NOT NULL,
            external_id VARCHAR(191) NULL,
            INDEX idx_inbound_org (org_id, status),
            UNIQUE KEY uq_inbound_external (org_id, channel, external_id),
            CONSTRAINT fk_inbound_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
        )",
    )
    .map_err(|e| e.to_string())?;
    ensure_external_id(conn)
}

/// Add `external_id` and its unique key to a table that predates them. Existing
/// rows get `NULL`, which the unique key allows any number of times. Each step
/// is checked on its own, so an interrupted run is finished by the next call.
fn ensure_external_id(conn: &mut PooledConn) -> Result<(), String> {
    let has_column: Option<i64> = conn
        .query_first(
            "SELECT 1 FROM information_schema.columns
             WHERE table_schema = DATABASE() AND table_name = 'InboundMessages'
               AND column_name = 'external_id'",
        )
        .map_err(|e| e.to_string())?;
    if has_column.is_none() {
        ignore_duplicate(
            conn.query_drop("ALTER TABLE InboundMessages ADD COLUMN external_id VARCHAR(191) NULL"),
        )?;
    }
    let has_key: Option<i64> = conn
        .query_first(
            "SELECT 1 FROM information_schema.statistics
             WHERE table_schema = DATABASE() AND table_name = 'InboundMessages'
               AND index_name = 'uq_inbound_external' LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    if has_key.is_none() {
        ignore_duplicate(conn.query_drop(
            "ALTER TABLE InboundMessages
             ADD UNIQUE KEY uq_inbound_external (org_id, channel, external_id)",
        ))?;
    }
    Ok(())
}

/// Treat "duplicate column" (1060) / "duplicate key name" (1061) as success:
/// a concurrent caller already ran the same `ALTER TABLE`.
fn ignore_duplicate(result: mysql::Result<()>) -> Result<(), String> {
    match result {
        Err(mysql::Error::MySqlError(e)) if e.code == 1060 || e.code == 1061 => Ok(()),
        other => other.map_err(|e| e.to_string()),
    }
}

fn connect() -> Result<PooledConn, String> {
    let mut conn = DbConnection::from_env()
        .get_connection()
        .map_err(|e| e.to_string())?;
    ensure_tables(&mut conn)?;
    Ok(conn)
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// Whether two phone numbers are the same, ignoring formatting and country
/// code: the last 10 digits must agree.
fn same_phone(a: &str, b: &str) -> bool {
    let (a, b) = (digits(a), digits(b));
    a.len() >= 10 && b.len() >= 10 && a[a.len() - 10..] == b[b.len() - 10..]
}

/// The one customer of `org_id` whose email or phone is `sender`. Two
/// customers sharing it is ambiguous, so that matches nobody.
pub fn match_customer(
    org_id: Uuid,
    channel: Channel,
    sender: &str,
) -> Result<Option<Uuid>, String> {
    let sender = sender.trim();
    let matches: Vec<Uuid> = Customer::list_by_org(org_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|c| match channel {
            Channel::Email => c
                .email
                .as_deref()
                .is_some_and(|e| !e.is_empty() && e.trim().eq_ignore_ascii_case(sender)),
            Channel::Whatsapp => c.phone.as_deref().is_some_and(|p| same_phone(p, sender)),
        })
        .map(|c| c.id)
        .collect();
    Ok(if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    })
}

fn clip(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/// Store a received message, matching its sender to a customer. The caller has
/// already checked the lengths.
pub fn receive(
    org_id: Uuid,
    channel: Channel,
    sender: &str,
    subject: Option<&str>,
    body: &str,
) -> Result<InboundMessage, String> {
    receive_once(org_id, channel, sender, subject, body, None).map(|(message, _)| message)
}

/// Like [`receive`], but a message whose `external_id` this org already has on
/// this channel isn't stored again: the stored one comes back, with `false`.
pub fn receive_once(
    org_id: Uuid,
    channel: Channel,
    sender: &str,
    subject: Option<&str>,
    body: &str,
    external_id: Option<&str>,
) -> Result<(InboundMessage, bool), String> {
    let external_id = external_id
        .map(|e| clip(e, MAX_EXTERNAL_ID_CHARS))
        .filter(|e| !e.is_empty());
    if let Some(ext) = &external_id
        && let Some(existing) = get_by_external_id(org_id, channel, ext)?
    {
        return Ok((existing, false));
    }
    let customer_id = match_customer(org_id, channel, sender)?;
    let message = InboundMessage {
        id: Uuid::new_v4(),
        org_id,
        channel,
        sender: clip(sender, MAX_SENDER_CHARS),
        subject: subject
            .map(|s| clip(s, MAX_SUBJECT_CHARS))
            .filter(|s| !s.is_empty()),
        body: clip(body, MAX_BODY_CHARS),
        customer_id,
        status: RECEIVED.to_string(),
        received_at: now_unix(),
        external_id,
    };
    let mut conn = connect()?;
    let inserted = conn.exec_drop(
        "INSERT INTO InboundMessages (id, org_id, channel, sender, subject, body, customer_id, status, received_at, external_id)
         VALUES (:id, :org_id, :channel, :sender, :subject, :body, :customer_id, :status, :received_at, :external_id)",
        params! {
            "id" => message.id.to_string(), "org_id" => org_id.to_string(),
            "channel" => channel.as_str(), "sender" => &message.sender,
            "subject" => &message.subject, "body" => &message.body,
            "customer_id" => message.customer_id.map(|c| c.to_string()),
            "status" => &message.status, "received_at" => message.received_at,
            "external_id" => &message.external_id,
        },
    );
    match inserted {
        Ok(()) => Ok((message, true)),
        // Two deliveries of the same message racing: the other one won.
        Err(mysql::Error::MySqlError(e)) if e.code == 1062 => {
            let ext = message.external_id.as_deref().unwrap_or_default();
            get_by_external_id(org_id, channel, ext)?
                .map(|existing| (existing, false))
                .ok_or_else(|| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn get_by_external_id(
    org_id: Uuid,
    channel: Channel,
    external_id: &str,
) -> Result<Option<InboundMessage>, String> {
    let mut conn = connect()?;
    let row: Option<Row10> = conn
        .exec_first(
            format!(
                "SELECT {COLS} FROM InboundMessages
                 WHERE org_id = :org_id AND channel = :channel AND external_id = :external_id"
            ),
            params! {
                "org_id" => org_id.to_string(), "channel" => channel.as_str(),
                "external_id" => external_id,
            },
        )
        .map_err(|e| e.to_string())?;
    Ok(row.map(to_message))
}

type Row10 = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    i64,
    Option<String>,
);

const COLS: &str =
    "id, org_id, channel, sender, subject, body, customer_id, status, received_at, external_id";

fn to_message(r: Row10) -> InboundMessage {
    InboundMessage {
        id: Uuid::parse_str(&r.0).unwrap_or_default(),
        org_id: Uuid::parse_str(&r.1).unwrap_or_default(),
        channel: Channel::from_db(&r.2),
        sender: r.3,
        subject: r.4,
        body: r.5,
        customer_id: r.6.and_then(|c| Uuid::parse_str(&c).ok()),
        status: r.7,
        received_at: r.8,
        external_id: r.9,
    }
}

pub fn get(id: Uuid) -> Result<Option<InboundMessage>, String> {
    let mut conn = connect()?;
    let row: Option<Row10> = conn
        .exec_first(
            format!("SELECT {COLS} FROM InboundMessages WHERE id = :id"),
            params! { "id" => id.to_string() },
        )
        .map_err(|e| e.to_string())?;
    Ok(row.map(to_message))
}

/// Newest first, optionally only those still `RECEIVED`.
pub fn list(org_id: Uuid, only_received: bool, limit: u32) -> Result<Vec<InboundMessage>, String> {
    let mut conn = connect()?;
    let rows: Vec<Row10> = conn
        .exec(
            format!(
                "SELECT {COLS} FROM InboundMessages
                 WHERE org_id = :org_id AND (NOT :only_received OR status = 'RECEIVED')
                 ORDER BY seq DESC LIMIT {limit}"
            ),
            params! { "org_id" => org_id.to_string(), "only_received" => only_received },
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().map(to_message).collect())
}

pub fn count_received(org_id: Uuid) -> Result<i64, String> {
    let mut conn = connect()?;
    let n: Option<i64> = conn
        .exec_first(
            "SELECT COUNT(*) FROM InboundMessages WHERE org_id = :org_id AND status = 'RECEIVED'",
            params! { "org_id" => org_id.to_string() },
        )
        .map_err(|e| e.to_string())?;
    Ok(n.unwrap_or(0))
}

/// Record what became of a `RECEIVED` message. A message that's already been
/// dealt with keeps its status.
pub fn mark_handled(id: Uuid, status: &str) -> Result<(), String> {
    let mut conn = connect()?;
    conn.exec_drop(
        "UPDATE InboundMessages SET status = :status WHERE id = :id AND status = 'RECEIVED'",
        params! { "status" => status, "id" => id.to_string() },
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
