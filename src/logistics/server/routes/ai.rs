//! Claude-backed endpoints: dispatch status summaries, the "ask your data"
//! assistant, the daily digest and reorder suggestions.

use super::common::*;
use super::dispatches::load_owned_dispatch;
use crate::logistics::ai::assistant::AssistantAnswer;
use crate::logistics::ai::reindex_job::{self, ReindexJob};
use crate::logistics::customer::customer::Customer;
use actix_web::{HttpResponse, Responder, get, post, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// A natural-language question for the org-scoped "ask your data" assistant.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct AssistantAskPayload {
    pub question: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AssistantAnswerResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<AssistantAnswer>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AssistantReindexResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<ReindexJob>,
}

#[utoipa::path(
    get,
    path = "/api/dispatches/{id}/summary",
    tag = "Dispatch",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Dispatch order UUID")),
    responses(
        (status = 200, description = "AI-generated plain-English status summary", body = EmptyResponse),
        (status = 404, description = "Dispatch not found", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/dispatches/{id}/summary")]
pub async fn get_dispatch_summary(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    let dispatch = match load_owned_dispatch(path.into_inner(), auth.org_id) {
        Ok(d) => d,
        Err(resp) => return resp,
    };

    let customer = match Customer::get_by_id(dispatch.customer_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return HttpResponse::NotFound().json(ApiResponse::<String> {
                success: false,
                message: "Customer for this dispatch not found".to_string(),
                data: None,
            });
        }
        Err(err) => {
            return HttpResponse::InternalServerError().json(ApiResponse::<String> {
                success: false,
                message: format!("Failed to fetch customer: {}", err),
                data: None,
            });
        }
    };

    match crate::logistics::ai::status::generate_dispatch_summary(&dispatch, &customer).await {
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

//
// An org-scoped "ask your data" assistant: a natural-language question is
// answered using only facts retrieved from that org's own indexed data
// (dispatch notifications, vehicle compliance documents — see
// crate::logistics::ai::chunk). Every role may ask; this is a read, not a
// write. See docs/todo-archive.org's "AI opportunities" section for the full design.

#[utoipa::path(
    post,
    path = "/api/orgs/{id}/assistant/ask",
    tag = "AI Assistant",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    request_body = AssistantAskPayload,
    responses(
        (status = 200, description = "Answer grounded in the organization's own indexed data", body = AssistantAnswerResponse),
        (status = 400, description = "Empty question", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[post("/orgs/{id}/assistant/ask")]
pub async fn ask_assistant(
    path: web::Path<Uuid>,
    payload: web::Json<AssistantAskPayload>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only ask about your own organization".to_string(),
            data: None,
        });
    }

    let question = payload.question.trim();
    if question.is_empty() {
        return HttpResponse::BadRequest().json(ApiResponse::<String> {
            success: false,
            message: "Question must not be empty".to_string(),
            data: None,
        });
    }

    match crate::logistics::ai::assistant::answer_question(org_id, question).await {
        Ok(answer) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Answer generated".to_string(),
            data: Some(answer),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to generate answer: {}", err),
            data: None,
        }),
    }
}

/// Start a rebuild of every assistant chunk for an org and return at once.
/// Exists for orgs whose customers/godowns/vehicles/dispatches predate this
/// feature and so never triggered a write-through index hook. The rebuild runs
/// as a background job (see `crate::logistics::ai::reindex_job`); poll
/// `GET /orgs/{id}/assistant/reindex` for its `chunks_indexed` count. A job
/// already queued or running is returned instead of starting another.
#[utoipa::path(
    post,
    path = "/api/orgs/{id}/assistant/reindex",
    tag = "AI Assistant",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 202, description = "The reindex job was queued (or is already running)", body = AssistantReindexResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[post("/orgs/{id}/assistant/reindex")]
pub async fn reindex_assistant(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only reindex your own organization".to_string(),
            data: None,
        });
    }

    match reindex_job::enqueue(org_id) {
        Ok(job) => HttpResponse::Accepted().json(ApiResponse {
            success: true,
            message: "Reindex queued".to_string(),
            data: Some(job),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to queue reindex: {}", err),
            data: None,
        }),
    }
}

/// The org's most recent reindex job and how many chunks it has written so
/// far. `data` is null if the org never ran one.
#[utoipa::path(
    get,
    path = "/api/orgs/{id}/assistant/reindex",
    tag = "AI Assistant",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "The latest reindex job, if any", body = AssistantReindexResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/assistant/reindex")]
pub async fn get_assistant_reindex(
    path: web::Path<Uuid>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only view your own organization".to_string(),
            data: None,
        });
    }

    match reindex_job::latest(org_id) {
        Ok(job) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Latest reindex job".to_string(),
            data: job,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to load reindex job: {}", err),
            data: None,
        }),
    }
}

/// The daily ops digest: an on-demand AI summary of "what needs attention
/// today" across dispatches, compliance and stock. Folded into the assistant
/// widget as a canned starter question rather than a page of its own — see
/// `crate::logistics::ai::digest`.
#[utoipa::path(
    get,
    path = "/api/orgs/{id}/assistant/digest",
    tag = "AI Assistant",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "AI-generated 'what needs attention today' briefing", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/assistant/digest")]
pub async fn get_daily_digest(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message: "Access denied: you can only view the digest for your own organization"
                .to_string(),
            data: None,
        });
    }

    match crate::logistics::ai::digest::generate_daily_digest(org_id).await {
        Ok(digest) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Digest generated".to_string(),
            data: Some(digest),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to generate digest: {}", err),
            data: None,
        }),
    }
}

/// Smart reorder suggestions: narrates godowns below their reorder
/// threshold with a suggested reorder quantity based on recent dispatch
/// velocity, not just the bare below-threshold flag. Folded into the
/// assistant widget as a canned starter question, same as the daily digest —
/// see `crate::logistics::ai::reorder`.
#[utoipa::path(
    get,
    path = "/api/orgs/{id}/assistant/reorder-suggestions",
    tag = "AI Assistant",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "AI-generated reorder-suggestion briefing", body = EmptyResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/assistant/reorder-suggestions")]
pub async fn get_reorder_suggestions(
    path: web::Path<Uuid>,
    auth: AuthenticatedOrg,
) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message:
                "Access denied: you can only view reorder suggestions for your own organization"
                    .to_string(),
            data: None,
        });
    }

    match crate::logistics::ai::reorder::generate_reorder_narration(org_id).await {
        Ok(narration) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Reorder suggestions generated".to_string(),
            data: Some(narration),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to generate reorder suggestions: {}", err),
            data: None,
        }),
    }
}

/// One line of the production plan: a stock item below its threshold and how
/// many units to make to restock it.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ProductionRun {
    pub godown_name: String,
    pub description: String,
    pub quantity_on_hand: i64,
    pub weekly_velocity: f64,
    /// Units to produce. `0` when there's no recent dispatch activity to
    /// estimate demand from.
    pub quantity_to_produce: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ProductionPlanResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<Vec<ProductionRun>>,
}

/// Production planning for manufacturers (e.g. AAC block plants), where low
/// stock means "make more", not "buy more". Same computation as the reorder
/// suggestions, returned as a structured "schedule production run" list,
/// largest run first. No AI call. See `docs/godowns.md`.
#[utoipa::path(
    get,
    path = "/api/orgs/{id}/production-plan",
    tag = "AI Assistant",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Organization UUID")),
    responses(
        (status = 200, description = "Production runs to schedule", body = ProductionPlanResponse),
        (status = 403, description = "Forbidden", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/orgs/{id}/production-plan")]
pub async fn get_production_plan(path: web::Path<Uuid>, auth: AuthenticatedOrg) -> impl Responder {
    let org_id = path.into_inner();
    if org_id != auth.org_id {
        return HttpResponse::Forbidden().json(ApiResponse::<String> {
            success: false,
            message:
                "Access denied: you can only view the production plan for your own organization"
                    .to_string(),
            data: None,
        });
    }

    match crate::logistics::ai::reorder::compute_reorder_suggestions(org_id) {
        Ok(suggestions) => {
            let mut runs: Vec<ProductionRun> = suggestions
                .into_iter()
                .map(|s| ProductionRun {
                    godown_name: s.godown_name,
                    description: s.description,
                    quantity_on_hand: s.quantity_on_hand,
                    weekly_velocity: s.weekly_velocity,
                    quantity_to_produce: s.suggested_reorder_quantity,
                })
                .collect();
            runs.sort_by_key(|a| std::cmp::Reverse(a.quantity_to_produce));
            HttpResponse::Ok().json(ApiResponse {
                success: true,
                message: "Production plan computed".to_string(),
                data: Some(runs),
            })
        }
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to compute production plan: {}", err),
            data: None,
        }),
    }
}
