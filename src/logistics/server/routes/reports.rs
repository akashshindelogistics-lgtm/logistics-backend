//! Operational reports (`/api/orgs/{id}/reports/*`).

use super::common::*;
use crate::logistics::reports::OpsReport;
use crate::logistics::reports::export::ExportDataset;
use actix_web::{HttpResponse, Responder, get, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OpsReportResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<OpsReport>,
}

#[utoipa::path(
    get,
    path = "/api/orgs/{id}/reports",
    tag = "Reports",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Operational report for the organization", body = OpsReportResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/reports")]
pub async fn get_ops_report(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only view reports for your own organization"
                .to_string(),
            data: None,
        });
    }
    match OpsReport::for_org(org_id) {
        Ok(report) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Report generated".to_string(),
            data: Some(report),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to build report: {}", err),
            data: None,
        }),
    }
}

/// "Explain this report" - an AI-narrated summary of the org's operational
/// report, same shape as `GET /api/dispatches/{id}/summary` but pointed at
/// [`OpsReport`] instead of one dispatch. Not RAG - straight
/// prompt-over-structured-data, exactly like the dispatch summary already is.
#[utoipa::path(
    get,
    path = "/api/orgs/{id}/reports/summary",
    tag = "Reports",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "AI-generated plain-English report briefing", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/reports/summary")]
pub async fn get_ops_report_summary(
    path: web::Path<Uuid>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only summarize reports for your own organization"
                .to_string(),
            data: None,
        });
    }

    let report = match OpsReport::for_org(org_id) {
        Ok(report) => report,
        Err(err) => {
            return HttpResponse::InternalServerError().json(ApiResponse::<String> {
                success: false,
                message: format!("Failed to build report: {}", err),
                data: None,
            });
        }
    };

    match crate::logistics::ai::report::generate_report_summary(&report).await {
        Ok(summary) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Summary generated".to_string(),
            data: Some(summary),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to generate summary: {}", err),
            data: None,
        }),
    }
}

/// Download one of the org's datasets: CSV (`dispatches`, `stock`, `invoices`,
/// `customers`) or Tally import XML (`tally-invoices`, `tally-stock`).
#[utoipa::path(
    get,
    path = "/api/orgs/{id}/export/{dataset}",
    tag = "Reports",
    security(("bearer_auth" = [])),
    params(
        ("id" = Uuid, Path, description = "Organization UUID"),
        ("dataset" = String, Path, description = "One of dispatches, stock, invoices, customers, tally-invoices, tally-stock")
    ),
    responses(
        (status = 200, description = "CSV file (text/csv) with a header row, or Tally XML (application/xml)", content((String = "text/csv"), (String = "application/xml"))),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 404, description = "Unknown dataset", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/export/{dataset}")]
pub async fn export_org_csv(
    path: web::Path<(Uuid, String)>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let (org_id, dataset) = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only export your own organization's data".to_string(),
            data: None,
        });
    }
    let Some(dataset) = ExportDataset::parse(&dataset) else {
        return HttpResponse::NotFound().json(ApiResponse::<String> {
            success: false,
            message: "Unknown dataset: use dispatches, stock, invoices, customers, tally-invoices or tally-stock".to_string(),
            data: None,
        });
    };
    match dataset.to_csv(org_id) {
        Ok(csv) => HttpResponse::Ok()
            .content_type(dataset.content_type())
            .insert_header((
                "Content-Disposition",
                format!(
                    "attachment; filename=\"{}.{}\"",
                    dataset.as_str(),
                    dataset.extension()
                ),
            ))
            .body(csv),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to export {}: {}", dataset.as_str(), err),
            data: None,
        }),
    }
}
