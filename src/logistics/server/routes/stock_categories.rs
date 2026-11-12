//! HSN codes per stock category (`/api/stock-categories`). See `docs/gst.md`.

use super::common::*;
use crate::logistics::gst::normalize_hsn_code;
use crate::logistics::stock::category::StockCategory;
use actix_web::{HttpResponse, Responder, delete, get, put, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct SetHsnPayload {
    /// 2 to 8 digits.
    pub hsn_code: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StockCategoryResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<StockCategory>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StockCategoryListResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<Vec<StockCategory>>,
}

#[utoipa::path(
    get,
    path = "/api/stock-categories",
    tag = "Godowns",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Stock categories of the organization that have an HSN code", body = StockCategoryListResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/stock-categories")]
pub async fn list_stock_categories(auth: AuthenticatedOrg) -> impl Responder {
    match StockCategory::list_by_org(auth.org_id) {
        Ok(categories) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: format!("Retrieved {} stock categories", categories.len()),
            data: Some(categories),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to list stock categories: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    put,
    path = "/api/stock-categories/{name}/hsn",
    tag = "Godowns",
    security(("bearer_auth" = [])),
    params(("name" = String, Path, description = "Category name, as in Stock.category")),
    request_body = SetHsnPayload,
    responses(
        (status = 200, description = "HSN code saved for the category", body = StockCategoryResponse),
        (status = 400, description = "Malformed HSN code or blank name", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[put("/stock-categories/{name}/hsn")]
pub async fn set_stock_category_hsn(
    path: web::Path<String>,
    payload: web::Json<SetHsnPayload>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    if let Err(resp) = auth.require_role(&WAREHOUSE_ROLES) {
        return resp;
    }
    let name = path.into_inner();
    let invalid = name.trim().is_empty()
        || !matches!(
            normalize_hsn_code(Some(payload.hsn_code.clone())),
            Ok(Some(_))
        );
    match StockCategory::set(auth.org_id, &name, &payload.hsn_code) {
        Ok(category) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "HSN code saved".to_string(),
            data: Some(category),
        }),
        Err(err) if invalid => HttpResponse::BadRequest().json(ApiResponse::<String> {
            success: false,
            message: err.to_string(),
            data: None,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to save HSN code: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    delete,
    path = "/api/stock-categories/{name}/hsn",
    tag = "Godowns",
    security(("bearer_auth" = [])),
    params(("name" = String, Path, description = "Category name, as in Stock.category")),
    responses(
        (status = 200, description = "HSN code removed", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 404, description = "The category has no HSN code", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[delete("/stock-categories/{name}/hsn")]
pub async fn delete_stock_category_hsn(
    path: web::Path<String>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    if let Err(resp) = auth.require_role(&WAREHOUSE_ROLES) {
        return resp;
    }
    match StockCategory::delete(auth.org_id, &path.into_inner()) {
        Ok(true) => HttpResponse::Ok().json(ApiResponse::<String> {
            success: true,
            message: "HSN code removed".to_string(),
            data: None,
        }),
        Ok(false) => HttpResponse::NotFound().json(ApiResponse::<String> {
            success: false,
            message: "Category has no HSN code".to_string(),
            data: None,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to remove HSN code: {}", err),
            data: None,
        }),
    }
}
