//! MSG91 delivery reports: `POST /api/public/msg91/delivery-reports`.
//!
//! MSG91 calls this URL when an SMS reaches (or fails to reach) a phone. No
//! bearer token, so the URL carries a shared secret, `?secret=`, which must
//! equal `MSG91_WEBHOOK_SECRET`; without that variable the endpoint is off.
//! See `docs/notifications.md`.

use super::common::*;
use crate::logistics::notification::notification::{DeliveryStatus, Notification};
use actix_web::{HttpResponse, Responder, post, web};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct DeliveryReportQuery {
    pub secret: Option<String>,
}

/// MSG91's status for one number: `1` is delivered; `2` (failed), `9` (NDNC /
/// DND-blocked), `16` (rejected), `17` (blocked number) and `25` (rejected by
/// the operator) are failures. Anything else (still pending, submitted) says
/// nothing final, so it's ignored. When the code is unknown, the `desc` text
/// (e.g. `"DELIVERED"`) decides. The codes are from MSG91's docs and still
/// need confirming against a live account (#313).
fn report_status(report: &Value) -> Option<DeliveryStatus> {
    let word = |v: &Value| match v {
        Value::String(s) => Some(s.trim().to_lowercase()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    [&report["status"], &report["desc"]]
        .into_iter()
        .filter_map(word)
        .find_map(|status| match status.as_str() {
            "1" | "delivered" => Some(DeliveryStatus::Delivered),
            "2" | "9" | "16" | "17" | "25" | "failed" | "rejected" | "undelivered" | "blocked"
            | "ndnc" => Some(DeliveryStatus::Failed),
            _ => None,
        })
}

/// The final status of one webhook entry. MSG91's standard report nests the
/// per-number result in `report: [{"number", "status", "desc", "date"}]`; a
/// flat entry carries `status` itself. Each notification is one SMS to one
/// number, so the first final per-number status wins.
fn entry_status(entry: &Value) -> Option<DeliveryStatus> {
    entry["report"]
        .as_array()
        .and_then(|items| items.iter().find_map(report_status))
        .or_else(|| report_status(entry))
}

/// Parses the body MSG91 posts: JSON (one entry or an array), or the legacy
/// form-encoded `data=<json>`.
fn parse_reports(body: &[u8]) -> Option<Vec<Value>> {
    let value = serde_json::from_slice::<Value>(body).ok().or_else(|| {
        let form = std::str::from_utf8(body).ok()?;
        let fields =
            web::Query::<std::collections::HashMap<String, String>>::from_query(form).ok()?;
        serde_json::from_str::<Value>(fields.get("data")?).ok()
    })?;
    Some(match value {
        Value::Array(items) => items,
        single => vec![single],
    })
}

fn report_request_id(report: &Value) -> Option<&str> {
    ["requestId", "request_id"]
        .iter()
        .find_map(|k| report[*k].as_str())
        .filter(|id| !id.is_empty())
}

fn secret_matches(given: Option<&str>) -> bool {
    let Ok(expected) = std::env::var("MSG91_WEBHOOK_SECRET") else {
        return false;
    };
    let expected = expected.trim();
    let given = given.unwrap_or("");
    // Constant-time comparison of equal-length byte strings.
    !expected.is_empty()
        && expected.len() == given.len()
        && expected
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[utoipa::path(
    post,
    path = "/api/public/msg91/delivery-reports",
    tag = "Notifications",
    params(("secret" = String, Query, description = "Must equal MSG91_WEBHOOK_SECRET")),
    request_body(
        content = String,
        description = "MSG91 report(s) as JSON, or form-encoded `data=<json>`; flat `{requestId, status}` or nested `{requestId, report: [{status, desc}]}`, up to 256 KB",
        content_type = "application/json"
    ),
    responses(
        (status = 200, description = "Reports stored", body = EmptyResponse),
        (status = 400, description = "Body is neither JSON nor a form data=<json> field", body = EmptyResponse),
        (status = 401, description = "Wrong or missing secret, or webhook not enabled", body = EmptyResponse)
    )
)]
/// Receives MSG91 delivery reports (JSON, one entry or an array, or a form
/// `data=<json>` field; flat or with a nested `report[]`) and stores
/// delivered/failed on the matching notification. Unknown request IDs and
/// pending statuses are ignored, so MSG91 never retries them. The body is read
/// as raw bytes, so actix's 256 KB payload limit applies instead of the 32 KB
/// JSON one: MSG91 batches reports.
#[post("/public/msg91/delivery-reports")]
pub async fn msg91_delivery_reports(
    query: web::Query<DeliveryReportQuery>,
    body: web::Bytes,
) -> impl Responder {
    if !secret_matches(query.secret.as_deref()) {
        return HttpResponse::Unauthorized().json(ApiResponse::<String> {
            success: false,
            message: "Unauthorized".to_string(),
            data: None,
        });
    }
    let Some(reports) = parse_reports(&body) else {
        return HttpResponse::BadRequest().json(ApiResponse::<String> {
            success: false,
            message: "Expected a JSON body or a form field data=<json>".to_string(),
            data: None,
        });
    };
    for report in &reports {
        let (Some(id), Some(status)) = (report_request_id(report), entry_status(report)) else {
            continue;
        };
        if let Err(err) = Notification::record_delivery_report(id, status) {
            return HttpResponse::InternalServerError().json(ApiResponse::<String> {
                success: false,
                message: format!("Failed to store delivery report: {err}"),
                data: None,
            });
        }
    }
    HttpResponse::Ok().json(ApiResponse::<String> {
        success: true,
        message: "Delivery reports received".to_string(),
        data: None,
    })
}
