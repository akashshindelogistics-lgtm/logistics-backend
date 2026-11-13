//! CSV import (`/api/orgs/{id}/import/{kind}`) and its templates.

use super::common::*;
use crate::logistics::import::csv_import::{
    ImportKind, ImportReport, MAX_IMPORT_BYTES, RowError, import_csv,
};
use actix_multipart::Multipart;
use actix_web::{HttpResponse, Responder, get, post, web};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ImportReportResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<ImportReport>,
}

#[derive(Debug, Deserialize)]
pub struct ImportQuery {
    #[serde(default)]
    pub dry_run: bool,
}

fn unknown_kind() -> HttpResponse {
    HttpResponse::NotFound().json(ApiResponse::<String> {
        success: false,
        message: "Unknown import kind: use vehicles, drivers, customers or stock".to_string(),
        data: None,
    })
}

fn bad_request(message: String) -> HttpResponse {
    HttpResponse::BadRequest().json(ApiResponse::<String> {
        success: false,
        message,
        data: None,
    })
}

/// Download the CSV template (header row) for an import kind.
#[utoipa::path(
    get,
    path = "/api/orgs/{id}/import/{kind}/template",
    tag = "Import",
    security(("bearer_auth" = [])),
    params(
        ("id" = Uuid, Path, description = "Organization UUID"),
        ("kind" = String, Path, description = "One of vehicles, drivers, customers, stock")
    ),
    responses(
        (status = 200, description = "CSV template (text/csv) with the header row", content((String = "text/csv"))),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 404, description = "Unknown kind", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/import/{kind}/template")]
pub async fn import_template(
    path: web::Path<(Uuid, String)>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let (org_id, kind) = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only use your own organization's templates"
                .to_string(),
            data: None,
        });
    }
    let Some(kind) = ImportKind::parse(&kind) else {
        return unknown_kind();
    };
    HttpResponse::Ok()
        .content_type("text/csv; charset=utf-8")
        .insert_header((
            "Content-Disposition",
            format!("attachment; filename=\"{}-template.csv\"", kind.as_str()),
        ))
        .body(kind.template())
}

/// Import vehicles, drivers, customers or stock from a CSV. With
/// `?dry_run=true` nothing is written and row errors are reported by line.
/// A real import is all-or-nothing: any row error (`422`) writes nothing.
#[utoipa::path(
    post,
    path = "/api/orgs/{id}/import/{kind}",
    tag = "Import",
    security(("bearer_auth" = [])),
    params(
        ("id" = Uuid, Path, description = "Organization UUID"),
        ("kind" = String, Path, description = "One of vehicles, drivers, customers, stock"),
        ("dry_run" = Option<bool>, Query, description = "Validate only; write nothing")
    ),
    request_body(
        content = String,
        description = "multipart/form-data with one `file` field — a CSV with the template's header, up to 1 MiB and 1,000 rows",
        content_type = "multipart/form-data"
    ),
    responses(
        (status = 200, description = "Imported, or dry run report (errors listed, nothing written)", body = ImportReportResponse),
        (status = 400, description = "Unreadable file, wrong header, or over the size/row limit", body = EmptyResponse),
        (status = 422, description = "Some rows are invalid; nothing was written", body = ImportReportResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 404, description = "Unknown kind", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[post("/orgs/{id}/import/{kind}")]
pub async fn import_org_csv(
    path: web::Path<(Uuid, String)>,
    query: web::Query<ImportQuery>,
    mut payload: Multipart,
    auth: AuthenticatedOrg,
) -> impl Responder {
    if let Err(resp) = auth.require_role(&ADMIN_ONLY) {
        return resp;
    }
    let (org_id, kind) = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only import into your own organization".to_string(),
            data: None,
        });
    }
    let Some(kind) = ImportKind::parse(&kind) else {
        return unknown_kind();
    };

    let mut bytes: Vec<u8> = Vec::new();
    let Some(item) = payload.next().await else {
        return bad_request("No file was uploaded".to_string());
    };
    let mut field = match item {
        Ok(f) => f,
        Err(err) => return bad_request(format!("Malformed multipart upload: {err}")),
    };
    while let Some(chunk) = field.next().await {
        let data = match chunk {
            Ok(d) => d,
            Err(err) => return bad_request(format!("Malformed multipart upload: {err}")),
        };
        if bytes.len() + data.len() > MAX_IMPORT_BYTES {
            return bad_request(format!("File exceeds the {MAX_IMPORT_BYTES}-byte limit"));
        }
        bytes.extend_from_slice(&data);
    }

    let dry_run = query.dry_run;
    let report = match import_csv(org_id, kind, &bytes, dry_run) {
        Ok(Ok(report)) => report,
        Ok(Err(file_err)) => return bad_request(file_err.0),
        Err(err) => {
            return HttpResponse::InternalServerError().json(ApiResponse::<String> {
                success: false,
                message: format!("Failed to import {}: {err}", kind.as_str()),
                data: None,
            });
        }
    };

    let has_errors = !report.errors.is_empty();
    let message = if has_errors {
        format!(
            "{} row(s) have errors; nothing was written",
            report
                .errors
                .iter()
                .map(|e: &RowError| e.line)
                .collect::<std::collections::HashSet<_>>()
                .len()
        )
    } else if report.dry_run {
        format!("Dry run: {} row(s) are valid", report.valid_rows)
    } else {
        format!("Imported {} row(s)", report.imported)
    };
    let body = ApiResponse {
        success: !has_errors,
        message,
        data: Some(report),
    };
    if has_errors && !dry_run {
        HttpResponse::UnprocessableEntity().json(body)
    } else {
        HttpResponse::Ok().json(body)
    }
}
