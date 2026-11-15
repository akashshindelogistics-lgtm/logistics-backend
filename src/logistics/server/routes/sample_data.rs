//! Optional sample data for a new organisation
//! (`/api/orgs/{id}/sample-data`). See `docs/onboarding.md`, design 6.

use super::common::*;
use crate::logistics::orgs::sample_data::{self, SampleDataError, SampleDataSummary};
use actix_web::{HttpResponse, Responder, delete, get, post, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SampleDataStatus {
    pub loaded: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SampleDataStatusResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<SampleDataStatus>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SampleDataSummaryResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<SampleDataSummary>,
}

fn forbidden_org() -> HttpResponse {
    HttpResponse::Forbidden().json(ApiResponse::<String> {
        success: false,
        message: "Access denied: you can only manage your own organization".to_string(),
        data: None,
    })
}

fn failure(err: SampleDataError, what: &str) -> HttpResponse {
    let message = format!("Failed to {what} sample data: {err}");
    let body = ApiResponse::<String> {
        success: false,
        message,
        data: None,
    };
    match err {
        SampleDataError::AlreadyLoaded | SampleDataError::InUse(_) => {
            HttpResponse::Conflict().json(body)
        }
        SampleDataError::Db(_) => HttpResponse::InternalServerError().json(body),
    }
}

#[utoipa::path(
    get,
    path = "/api/orgs/{id}/sample-data",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Whether sample data is loaded", body = SampleDataStatusResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/sample-data")]
pub async fn get_sample_data_status(
    path: web::Path<Uuid>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return forbidden_org();
    }
    match sample_data::is_loaded(org_id) {
        Ok(loaded) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Retrieved sample data status".to_string(),
            data: Some(SampleDataStatus { loaded }),
        }),
        Err(err) => failure(err.into(), "read"),
    }
}

#[utoipa::path(
    post,
    path = "/api/orgs/{id}/sample-data",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 201, description = "Sample godown, stock, vehicle, driver and customer created", body = SampleDataSummaryResponse),
        (status = 403, description = "Forbidden: Admin only", body = EmptyResponse),
        (status = 409, description = "Sample data is already loaded", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[post("/orgs/{id}/sample-data")]
pub async fn load_sample_data(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    if let Err(resp) = auth.require_role(&ADMIN_ONLY) {
        return resp;
    }
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return forbidden_org();
    }
    match sample_data::load(org_id) {
        Ok(summary) => HttpResponse::Created().json(ApiResponse {
            success: true,
            message: "Sample data loaded".to_string(),
            data: Some(summary),
        }),
        Err(err) => failure(err, "load"),
    }
}

#[utoipa::path(
    delete,
    path = "/api/orgs/{id}/sample-data",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Sample data removed (all zeros if none was loaded)", body = SampleDataSummaryResponse),
        (status = 403, description = "Forbidden: Admin only", body = EmptyResponse),
        (status = 409, description = "Your own data still depends on the sample data", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[delete("/orgs/{id}/sample-data")]
pub async fn remove_sample_data(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    if let Err(resp) = auth.require_role(&ADMIN_ONLY) {
        return resp;
    }
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return forbidden_org();
    }
    match sample_data::remove(org_id) {
        Ok(summary) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Sample data removed".to_string(),
            data: Some(summary),
        }),
        Err(err) => failure(err, "remove"),
    }
}
