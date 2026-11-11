//! The customer tracking link: `GET /api/public/track/{token}`, opened from
//! the link in a dispatch's created / running-late message. No login — the
//! random token is the whole credential — so the answer carries only what the
//! customer needs: no customer details, driver, vehicle number or line items.
//! See `docs/customer-tracking.md`.

use super::common::*;
use crate::logistics::dispatch::dispatch::{
    DispatchOrder, DispatchStatus, DispatchStatusEvent, PROMISED_DELIVERY_HOURS,
};
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::vehicle::vehicle::Vehicle;
use actix_web::{HttpResponse, Responder, get, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Where the vehicle carrying the order last reported from.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PublicVehiclePosition {
    pub latitude: f64,
    pub longitude: f64,
    /// Unix seconds of the fix.
    pub updated_at: i64,
}

/// What a customer sees for their order.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PublicTracking {
    /// First 8 characters of the dispatch id, as quoted in the message.
    pub reference: String,
    /// The organization sending the goods.
    pub shipper: String,
    pub status: DispatchStatus,
    pub dispatched_at: i64,
    /// `dispatched_at` plus the promised delivery window.
    pub expected_by: i64,
    pub status_history: Vec<DispatchStatusEvent>,
    pub delivered_at: Option<i64>,
    /// Only while the goods are `LOADED` or `IN_TRANSIT`, and only if the
    /// vehicle has reported a position: once delivered, the truck's
    /// whereabouts are none of the customer's business.
    pub vehicle_position: Option<PublicVehiclePosition>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PublicTrackingResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<PublicTracking>,
}

fn vehicle_position(dispatch: &DispatchOrder) -> Option<PublicVehiclePosition> {
    if !matches!(
        dispatch.status,
        DispatchStatus::Loaded | DispatchStatus::InTransit
    ) {
        return None;
    }
    let reg = dispatch.vehicle_registration_number.as_deref()?;
    let vehicle = Vehicle::list_by_org(dispatch.org_id)
        .ok()?
        .into_iter()
        .find(|v| v.registration_number == reg)?;
    let location = vehicle.location?;
    Some(PublicVehiclePosition {
        latitude: location.latitude,
        longitude: location.longitude,
        updated_at: location.timestamp,
    })
}

fn not_found() -> HttpResponse {
    HttpResponse::NotFound().json(ApiResponse::<String> {
        success: false,
        message: "No order matches this tracking link".to_string(),
        data: None,
    })
}

#[utoipa::path(
    get,
    path = "/api/public/track/{token}",
    tag = "Dispatch",
    params(("token" = Uuid, Path, description = "The dispatch's tracking token, from the customer's message")),
    responses(
        (status = 200, description = "The order's status and the vehicle's last position", body = PublicTrackingResponse),
        (status = 404, description = "No dispatch has this token", body = EmptyResponse)
    )
)]
/// Public order tracking for the customer. No bearer token: the random
/// tracking token in the path is the credential.
#[get("/public/track/{token}")]
pub async fn public_track_dispatch(path: web::Path<String>) -> impl Responder {
    // A malformed token is just another unknown link, not a 400.
    let Ok(token) = Uuid::parse_str(&path.into_inner()) else {
        return not_found();
    };
    let dispatch = match DispatchOrder::by_tracking_token(token) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(),
        Err(err) => {
            eprintln!("public tracking lookup failed: {err}");
            return HttpResponse::InternalServerError().json(ApiResponse::<String> {
                success: false,
                message: "Tracking is unavailable right now".to_string(),
                data: None,
            });
        }
    };
    let shipper = Organization::get_by_id(dispatch.org_id)
        .ok()
        .flatten()
        .map(|o| o.name)
        .unwrap_or_default();
    let data = PublicTracking {
        reference: dispatch.id.to_string()[..8].to_string(),
        shipper,
        status: dispatch.status,
        dispatched_at: dispatch.dispatched_at,
        expected_by: dispatch.dispatched_at + (PROMISED_DELIVERY_HOURS * 3600.0) as i64,
        delivered_at: dispatch.proof_of_delivery.as_ref().map(|p| p.delivered_at),
        vehicle_position: vehicle_position(&dispatch),
        status_history: dispatch.status_history,
    };
    HttpResponse::Ok().json(PublicTrackingResponse {
        success: true,
        message: "Order found".to_string(),
        data: Some(data),
    })
}
