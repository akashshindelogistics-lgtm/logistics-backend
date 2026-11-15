//! Inbound email and WhatsApp messages. See `docs/inbound-channels.md`.

use super::common::*;
use crate::logistics::inbound::{
    self, Channel, InboundMessage, MAX_BODY_CHARS, MAX_EXTERNAL_ID_CHARS, MAX_SENDER_CHARS,
    MAX_SUBJECT_CHARS,
};
use actix_web::http::StatusCode;
use actix_web::{HttpRequest, HttpResponse, Responder, get, post, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

const LIST_LIMIT: u32 = 100;
const SECRET_HEADER: &str = "X-Inbound-Secret";

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InboundMessagePayload {
    pub channel: Channel,
    /// The sender's email address or phone number.
    pub sender: String,
    pub subject: Option<String>,
    pub body: String,
    /// The provider's message id (an email Message-ID, a WhatsApp message id),
    /// at most 191 characters. Send it so a retried delivery isn't stored twice:
    /// a repeat answers 200 with the message already stored.
    pub external_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InboundMessageResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<InboundMessage>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InboundMessageListResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<Vec<InboundMessage>>,
}

fn error(status: StatusCode, message: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(ApiResponse::<String> {
        success: false,
        message: message.into(),
        data: None,
    })
}

/// Compares without stopping at the first difference.
fn secrets_match(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[utoipa::path(
    post,
    path = "/api/inbound/{org_id}/messages",
    tag = "Inbound",
    params(("org_id" = Uuid, Path, description = "Organization UUID")),
    request_body = InboundMessagePayload,
    responses(
        (status = 201, description = "Stored for the order-intake agent", body = InboundMessageResponse),
        (status = 200, description = "Already stored: a repeat of an earlier external_id, nothing new stored", body = InboundMessageResponse),
        (status = 400, description = "Empty or too long", body = EmptyResponse),
        (status = 401, description = "Missing or wrong X-Inbound-Secret", body = EmptyResponse),
        (status = 404, description = "No such organization", body = EmptyResponse),
        (status = 503, description = "INBOUND_WEBHOOK_SECRET isn't configured", body = EmptyResponse)
    )
)]
#[post("/inbound/{org_id}/messages")]
pub async fn receive_inbound_message(
    req: HttpRequest,
    path: web::Path<Uuid>,
    payload: web::Json<InboundMessagePayload>,
) -> impl Responder {
    let expected = std::env::var("INBOUND_WEBHOOK_SECRET").unwrap_or_default();
    if expected.trim().is_empty() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Inbound messages need INBOUND_WEBHOOK_SECRET to be configured",
        );
    }
    let given = req
        .headers()
        .get(SECRET_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !secrets_match(given, expected.trim()) {
        return error(StatusCode::UNAUTHORIZED, "Wrong inbound secret");
    }

    let org_id = path.into_inner();
    let body = payload.body.trim();
    let sender = payload.sender.trim();
    if body.is_empty() || sender.is_empty() {
        return error(StatusCode::BAD_REQUEST, "sender and body are required");
    }
    if body.chars().count() > MAX_BODY_CHARS
        || sender.chars().count() > MAX_SENDER_CHARS
        || payload
            .subject
            .as_deref()
            .is_some_and(|s| s.chars().count() > MAX_SUBJECT_CHARS)
        || payload
            .external_id
            .as_deref()
            .is_some_and(|e| e.trim().chars().count() > MAX_EXTERNAL_ID_CHARS)
    {
        return error(StatusCode::BAD_REQUEST, "Message is too long");
    }
    match crate::logistics::orgs::orgs::Organization::get_by_id(org_id) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::NOT_FOUND, "Organization not found"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    match inbound::receive_once(
        org_id,
        payload.channel,
        sender,
        payload.subject.as_deref(),
        body,
        payload.external_id.as_deref(),
    ) {
        Ok((message, true)) => HttpResponse::Created().json(ApiResponse {
            success: true,
            message: "Message received".to_string(),
            data: Some(message),
        }),
        Ok((message, false)) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Message already received".to_string(),
            data: Some(message),
        }),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[utoipa::path(
    get,
    path = "/api/orgs/{id}/inbound/messages",
    tag = "Inbound",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "The org's 100 most recent inbound messages, newest first", body = InboundMessageListResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/inbound/messages")]
pub async fn list_inbound_messages(
    path: web::Path<Uuid>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return error(StatusCode::FORBIDDEN, "Access denied");
    }
    match inbound::list(org_id, false, LIST_LIMIT) {
        Ok(messages) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: format!("Retrieved {} inbound messages", messages.len()),
            data: Some(messages),
        }),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
