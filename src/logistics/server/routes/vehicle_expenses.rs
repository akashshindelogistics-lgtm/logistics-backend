//! Per-vehicle expenses (fuel, tolls, driver advances) for cost-per-trip
//! reporting. See `docs/vehicle-expenses.md`.

use super::common::*;
use super::vehicle_documents::ensure_owned_vehicle;
use crate::logistics::dispatch::trip::Trip;
use crate::logistics::vehicle::expense::{ExpenseType, VehicleExpense, VehicleExpenseError};
use actix_web::{HttpResponse, Responder, delete, get, post, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Log one expense against a vehicle.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct VehicleExpensePayload {
    pub expense_type: ExpenseType,
    /// Whole currency units, greater than zero.
    pub amount: i64,
    /// ISO `YYYY-MM-DD`.
    pub incurred_on: String,
    /// Trip (or standalone dispatch) id this cost belongs to, optional.
    #[serde(default)]
    pub trip_id: Option<Uuid>,
    /// Litres filled, for fuel fills.
    #[serde(default)]
    pub litres: Option<f64>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct VehicleExpenseResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<VehicleExpense>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct VehicleExpenseListResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<Vec<VehicleExpense>>,
}

fn error_500(what: &str, err: impl std::fmt::Display) -> HttpResponse {
    HttpResponse::InternalServerError().json(ApiResponse::<String> {
        success: false,
        message: format!("{what}: {err}"),
        data: None,
    })
}

#[utoipa::path(
    get,
    path = "/api/vehicles/{reg}/expenses",
    tag = "Vehicle expenses",
    security(("bearer_auth" = [])),
    params(("reg" = String, Path, description = "Vehicle registration number")),
    responses(
        (status = 200, description = "The vehicle's expenses, newest first", body = VehicleExpenseListResponse),
        (status = 404, description = "Vehicle not found in this organization", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/vehicles/{reg}/expenses")]
pub async fn list_vehicle_expenses(
    path: web::Path<String>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let reg = path.into_inner();
    if let Err(resp) = ensure_owned_vehicle(&reg, auth.org_id) {
        return resp;
    }
    match VehicleExpense::list_by_vehicle(&reg) {
        Ok(items) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: format!("Retrieved {} expenses", items.len()),
            data: Some(items),
        }),
        Err(err) => error_500("Failed to list vehicle expenses", err),
    }
}

#[utoipa::path(
    post,
    path = "/api/vehicles/{reg}/expenses",
    tag = "Vehicle expenses",
    security(("bearer_auth" = [])),
    params(("reg" = String, Path, description = "Vehicle registration number")),
    request_body = VehicleExpensePayload,
    responses(
        (status = 201, description = "Expense logged", body = VehicleExpenseResponse),
        (status = 400, description = "Invalid amount, litres or date", body = EmptyResponse),
        (status = 404, description = "Vehicle or trip not found in this organization", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[post("/vehicles/{reg}/expenses")]
pub async fn add_vehicle_expense(
    path: web::Path<String>,
    payload: web::Json<VehicleExpensePayload>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let reg = path.into_inner();
    if let Err(resp) = ensure_owned_vehicle(&reg, auth.org_id) {
        return resp;
    }
    let body = payload.into_inner();
    if let Some(trip_id) = body.trip_id {
        match Trip::get_by_id(trip_id) {
            Ok(Some(trip)) if trip.org_id == auth.org_id => {}
            Ok(_) => {
                return HttpResponse::NotFound().json(ApiResponse::<String> {
                    success: false,
                    message: "Trip not found in this organization".to_string(),
                    data: None,
                });
            }
            Err(err) => return error_500("Failed to fetch trip", err),
        }
    }
    match VehicleExpense::create(
        auth.org_id,
        &reg,
        body.trip_id,
        body.expense_type,
        body.amount,
        body.incurred_on,
        body.litres,
        body.notes,
    ) {
        Ok(item) => HttpResponse::Created().json(ApiResponse {
            success: true,
            message: "Vehicle expense logged".to_string(),
            data: Some(item),
        }),
        Err(VehicleExpenseError::Db(err)) => error_500("Failed to save vehicle expense", err),
        Err(err) => HttpResponse::BadRequest().json(ApiResponse::<String> {
            success: false,
            message: err.to_string(),
            data: None,
        }),
    }
}

#[utoipa::path(
    delete,
    path = "/api/vehicle-expenses/{id}",
    tag = "Vehicle expenses",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Vehicle expense UUID")),
    responses(
        (status = 200, description = "Expense deleted", body = EmptyResponse),
        (status = 403, description = "Expense belongs to a different organization", body = EmptyResponse),
        (status = 404, description = "Expense not found", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[delete("/vehicle-expenses/{id}")]
pub async fn delete_vehicle_expense(
    path: web::Path<Uuid>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let item = match VehicleExpense::get_by_id(path.into_inner()) {
        Ok(Some(item)) if item.org_id == auth.org_id => item,
        Ok(Some(_)) => {
            return HttpResponse::Forbidden().json(ApiResponse::<String> {
                success: false,
                message: "Access denied: expense belongs to a different organization".to_string(),
                data: None,
            });
        }
        Ok(None) => {
            return HttpResponse::NotFound().json(ApiResponse::<String> {
                success: false,
                message: "Vehicle expense not found".to_string(),
                data: None,
            });
        }
        Err(err) => return error_500("Failed to fetch vehicle expense", err),
    };
    match item.delete() {
        Ok(_) => HttpResponse::Ok().json(ApiResponse::<String> {
            success: true,
            message: "Vehicle expense deleted".to_string(),
            data: None,
        }),
        Err(err) => error_500("Failed to delete vehicle expense", err),
    }
}

#[utoipa::path(
    get,
    path = "/api/orgs/{id}/vehicle-expenses",
    tag = "Vehicle expenses",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Every expense across the org's fleet, newest first", body = VehicleExpenseListResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/vehicle-expenses")]
pub async fn list_org_vehicle_expenses(
    path: web::Path<Uuid>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied".to_string(),
            data: None,
        });
    }
    match VehicleExpense::list_by_org(org_id) {
        Ok(items) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: format!("Retrieved {} expenses", items.len()),
            data: Some(items),
        }),
        Err(err) => error_500("Failed to list vehicle expenses", err),
    }
}
