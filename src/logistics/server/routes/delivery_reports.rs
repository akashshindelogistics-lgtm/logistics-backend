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

/// MSG91's status for a report: `1` is delivered, `2`, `9` and `16` are
/// failures (failed, DND-blocked, rejected). Anything else (still pending)
/// says nothing final, so it's ignored.
fn report_status(report: &Value) -> Option<DeliveryStatus> {
    let status = match &report["status"] {
        Value::String(s) => s.trim().to_lowercase(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    match status.as_str() {
        "1" | "delivered" => Some(DeliveryStatus::Delivered),
        "2" | "9" | "16" | "failed" | "rejected" | "undelivered" | "blocked" => {
            Some(DeliveryStatus::Failed)
        }
        _ => None,
    }
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
    responses(
        (status = 200, description = "Reports stored", body = EmptyResponse),
        (status = 401, description = "Wrong or missing secret, or webhook not enabled", body = EmptyResponse)
    )
)]
/// Receives MSG91 delivery reports (one JSON object or an array of them) and
/// stores delivered/failed on the matching notification. Unknown request IDs
/// and pending statuses are ignored, so MSG91 never retries them.
#[post("/public/msg91/delivery-reports")]
pub async fn msg91_delivery_reports(
    query: web::Query<DeliveryReportQuery>,
    body: web::Json<Value>,
) -> impl Responder {
    if !secret_matches(query.secret.as_deref()) {
        return HttpResponse::Unauthorized().json(ApiResponse::<String> {
            success: false,
            message: "Unauthorized".to_string(),
            data: None,
        });
    }
    let body = body.into_inner();
    let reports = match &body {
        Value::Array(items) => items.as_slice(),
        single => std::slice::from_ref(single),
    };
    for report in reports {
        let (Some(id), Some(status)) = (report_request_id(report), report_status(report)) else {
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
