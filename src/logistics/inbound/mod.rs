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
    /// `RECEIVED` until a dispatch has been made from it, then `PROCESSED`.
    pub status: String,
    pub received_at: i64,
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
            INDEX idx_inbound_org (org_id, status),
            CONSTRAINT fk_inbound_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
        )",
    )
    .map_err(|e| e.to_string())
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
        status: "RECEIVED".to_string(),
        received_at: now_unix(),
    };
    let mut conn = connect()?;
    conn.exec_drop(
        "INSERT INTO InboundMessages (id, org_id, channel, sender, subject, body, customer_id, status, received_at)
         VALUES (:id, :org_id, :channel, :sender, :subject, :body, :customer_id, :status, :received_at)",
        params! {
            "id" => message.id.to_string(), "org_id" => org_id.to_string(),
            "channel" => channel.as_str(), "sender" => &message.sender,
            "subject" => &message.subject, "body" => &message.body,
            "customer_id" => message.customer_id.map(|c| c.to_string()),
            "status" => &message.status, "received_at" => message.received_at,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(message)
}

type Row9 = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    i64,
);

const COLS: &str = "id, org_id, channel, sender, subject, body, customer_id, status, received_at";

fn to_message(r: Row9) -> InboundMessage {
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
    }
}

pub fn get(id: Uuid) -> Result<Option<InboundMessage>, String> {
    let mut conn = connect()?;
    let row: Option<Row9> = conn
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
    let rows: Vec<Row9> = conn
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

pub fn mark_processed(id: Uuid) -> Result<(), String> {
    let mut conn = connect()?;
    conn.exec_drop(
        "UPDATE InboundMessages SET status = 'PROCESSED' WHERE id = :id",
        params! { "id" => id.to_string() },
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
