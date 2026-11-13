//! Organization CRUD (`/api/orgs`, `/api/orgs/{id}`, `/api/orgs/{id}/location`).

use super::common::*;
use crate::logistics::auth::auth::OrgCredentials;
use crate::logistics::orgs::onboarding::OnboardingStatus;
use crate::logistics::orgs::orgs::Organization;
use actix_web::{HttpResponse, Responder, delete, get, post, put, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct CreateOrgPayload {
    pub name: String,
    pub address: String,
    pub password: String,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct UpdateOrgPayload {
    pub name: String,
    pub address: String,
    /// The organization's GSTIN (15 letters/digits, stored upper-case). Leave
    /// it out to keep the current one; send an empty string to clear it.
    #[serde(default)]
    pub gstin: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrgListResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<Vec<Organization>>,
}

#[utoipa::path(
    get,
    path = "/api/orgs",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Authenticated organization", body = OrgListResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs")]
pub async fn list_orgs(auth: AuthenticatedOrg) -> impl Responder {
    match Organization::get_by_id(auth.org_id) {
        Ok(Some(org)) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Retrieved organization".to_string(),
            data: Some(vec![org]),
        }),
        Ok(None) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "No organization found".to_string(),
            data: Some(Vec::<Organization>::new()),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to retrieve organization: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    get,
    path = "/api/orgs/{id}",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Organization detail with vehicles and stock", body = OrgResponse),
        (status = 403, description = "Forbidden: can only access your own organization", body = EmptyResponse),
        (status = 404, description = "Organization not found", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}")]
pub async fn get_org(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only view your own organization".to_string(),
            data: None,
        });
    }
    match Organization::get_by_id(org_id) {
        Ok(Some(org)) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Organization retrieved successfully".to_string(),
            data: Some(org),
        }),
        Ok(None) => HttpResponse::NotFound().json(ApiResponse::<String> {
            success: false,
            message: "Organization not found".to_string(),
            data: None,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to retrieve organization: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    post,
    path = "/api/orgs",
    tag = "Organizations",
    request_body = CreateOrgPayload,
    responses(
        (status = 201, description = "Organization created successfully", body = OrgResponse),
        (status = 500, description = "Internal server error", body = EmptyResponse)
    )
)]
#[post("/orgs")]
pub async fn create_org(payload: web::Json<CreateOrgPayload>) -> impl Responder {
    match Organization::create_organization(&payload.name, &payload.address) {
        Ok(org) => {
            if let Err(err) = OrgCredentials::create(org.id, &org.name, &payload.password) {
                return HttpResponse::InternalServerError().json(ApiResponse::<String> {
                    success: false,
                    message: format!("Failed to save credentials: {}", err),
                    data: None,
                });
            }
            HttpResponse::Created().json(ApiResponse {
                success: true,
                message: "Organization created successfully".to_string(),
                data: Some(org),
            })
        }
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to create organization: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    put,
    path = "/api/orgs/{id}",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    request_body = UpdateOrgPayload,
    responses(
        (status = 200, description = "Organization updated successfully", body = OrgResponse),
        (status = 400, description = "Malformed GSTIN", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[put("/orgs/{id}")]
pub async fn update_org(
    path: web::Path<Uuid>,
    payload: web::Json<UpdateOrgPayload>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only update your own organization".to_string(),
            data: None,
        });
    }
    let mut org = Organization {
        id: org_id,
        name: String::new(),
        address: String::new(),
        vehicles: Vec::new(),
        godowns: Vec::new(),
        location: None,
        gstin: None,
    };
    if let Err(msg) = crate::logistics::gst::normalize_gstin(payload.gstin.clone()) {
        return HttpResponse::BadRequest().json(ApiResponse::<String> {
            success: false,
            message: msg,
            data: None,
        });
    }

    let result = org
        .update_organization(&payload.name, &payload.address)
        .and_then(|_| match &payload.gstin {
            Some(g) => org.set_gstin(Some(g.clone())),
            None => {
                org.gstin = Organization::get_by_id(org_id)?.and_then(|o| o.gstin);
                Ok(())
            }
        });
    match result {
        Ok(_) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Organization updated successfully".to_string(),
            data: Some(org),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to update organization: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    put,
    path = "/api/orgs/{id}/location",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    request_body = LocationPayload,
    responses(
        (status = 200, description = "Organization location updated successfully", body = LocationResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[put("/orgs/{id}/location")]
pub async fn update_org_location(
    path: web::Path<Uuid>,
    payload: web::Json<LocationPayload>,
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
    let mut org = Organization {
        id: org_id,
        name: String::new(),
        address: String::new(),
        vehicles: Vec::new(),
        godowns: Vec::new(),
        location: None,
        gstin: None,
    };

    match org.update_location(payload.latitude, payload.longitude, payload.address.clone()) {
        Ok(_) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Organization location updated successfully".to_string(),
            data: org.location,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to update organization location: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    delete,
    path = "/api/orgs/{id}",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Organization deleted successfully", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[delete("/orgs/{id}")]
pub async fn delete_org(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    if let Err(resp) = auth.require_role(&ADMIN_ONLY) {
        return resp;
    }
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only delete your own organization".to_string(),
            data: None,
        });
    }
    let org = Organization {
        id: org_id,
        name: String::new(),
        address: String::new(),
        vehicles: Vec::new(),
        godowns: Vec::new(),
        location: None,
        gstin: None,
    };

    match org.remove_organization() {
        Ok(_) => HttpResponse::Ok().json(ApiResponse::<String> {
            success: true,
            message: "Organization deleted successfully".to_string(),
            data: None,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to delete organization: {}", err),
            data: None,
        }),
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OnboardingResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<OnboardingStatus>,
}

#[utoipa::path(
    get,
    path = "/api/orgs/{id}/onboarding",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Setup checklist, derived from the org's data", body = OnboardingResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 404, description = "Organization not found", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/onboarding")]
pub async fn get_onboarding(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only view your own organization".to_string(),
            data: None,
        });
    }
    match Organization::onboarding_status(org_id) {
        Ok(status) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Onboarding status retrieved successfully".to_string(),
            data: Some(status),
        }),
        Err(err) if err.to_string() == "Organization not found" => {
            HttpResponse::NotFound().json(ApiResponse::<String> {
                success: false,
                message: "Organization not found".to_string(),
                data: None,
            })
        }
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to retrieve onboarding status: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    put,
    path = "/api/orgs/{id}/onboarding/dismiss",
    tag = "Organizations",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Checklist hidden; returns the updated status", body = OnboardingResponse),
        (status = 403, description = "Forbidden (Admin only)", body = EmptyResponse),
        (status = 404, description = "Organization not found", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[put("/orgs/{id}/onboarding/dismiss")]
pub async fn dismiss_onboarding(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    if let Err(resp) = auth.require_role(&ADMIN_ONLY) {
        return resp;
    }
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only update your own organization".to_string(),
            data: None,
        });
    }
    match Organization::dismiss_onboarding(org_id)
        .and_then(|_| Organization::onboarding_status(org_id))
    {
        Ok(status) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Onboarding checklist dismissed".to_string(),
            data: Some(status),
        }),
        Err(err) if err.to_string() == "Organization not found" => {
            HttpResponse::NotFound().json(ApiResponse::<String> {
                success: false,
                message: "Organization not found".to_string(),
                data: None,
            })
        }
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to dismiss onboarding: {}", err),
            data: None,
        }),
    }
}
