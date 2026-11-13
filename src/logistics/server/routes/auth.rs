//! Login endpoints (`/api/auth/*`): the public org list, org-owner and
//! team-member login, and `GET /api/auth/me`.

use super::common::*;
use crate::logistics::auth::auth::{
    OrgCredentials, OrgSummary, generate_token, generate_user_token,
};
use crate::logistics::notification::delivery::send_email;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::user::user::{OrgUser, ResendOutcome, UserError};
use actix_web::{HttpResponse, Responder, get, post, web};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct LoginPayload {
    pub org_id: Uuid,
    pub password: String,
}

/// Body for `POST /api/auth/user-login` — a team member signs in with the
/// email + password an Admin set for them.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct UserLoginPayload {
    pub email: String,
    pub password: String,
}

/// Body for `POST /api/auth/signup`.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct SignupPayload {
    pub org_name: String,
    pub address: String,
    pub owner_name: String,
    pub email: String,
    /// 8–72 characters with at least one letter and one digit.
    pub password: String,
    /// Must be `true`: the owner accepts the terms and privacy policy. The
    /// time is stored as `terms_accepted_at`.
    pub accept_terms: bool,
}

/// Signups allowed per client IP per window (`SIGNUP_RATE_LIMIT`, default 5).
pub(crate) fn signup_limit() -> usize {
    std::env::var("SIGNUP_RATE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
}

/// The address the signup limit keys on. Behind a reverse proxy
/// (`TRUSTED_PROXY=1`) it is the rightmost `X-Forwarded-For` entry, the one
/// the proxy appended; entries left of it, and `realip_remote_addr()`, are
/// client-supplied and forgeable. Otherwise, or without the header, the socket
/// peer.
fn signup_client_ip(req: &actix_web::HttpRequest) -> String {
    let forwarded = std::env::var("TRUSTED_PROXY")
        .is_ok_and(|v| matches!(v.trim(), "1" | "true"))
        .then(|| req.headers().get("x-forwarded-for")?.to_str().ok())
        .flatten()
        .and_then(|v| v.rsplit(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty());
    match forwarded {
        Some(ip) => ip.to_string(),
        None => req
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

const SIGNUP_WINDOW: Duration = Duration::from_secs(3600);

/// Record a signup attempt from `key`; `false` when it is over the limit.
/// In-memory and per process, so a restart resets it.
fn signup_allowed(key: &str) -> bool {
    static HITS: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();
    let mut hits = HITS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    hits.retain(|_, v| {
        v.retain(|t| now.duration_since(*t) < SIGNUP_WINDOW);
        !v.is_empty()
    });
    let entry = hits.entry(key.to_string()).or_default();
    if entry.len() >= signup_limit() {
        return false;
    }
    entry.push(now);
    true
}

/// Body for `POST /api/auth/verify-email`.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct VerifyEmailPayload {
    pub token: String,
}

/// What the dashboard needs to decide which banners to show.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SessionData {
    pub org_id: String,
    pub role: String,
    /// The signed-in team member, `None` for the shared org-password login.
    pub user_id: Option<String>,
    pub email: Option<String>,
    /// `false` while the signed-in user's email awaits verification: show the
    /// verify banner with a resend button. `true` for the org-password login.
    pub email_verified: bool,
    /// `true` when the org has no active Admin user: show "Create your first
    /// Admin user", which is `POST /api/orgs/{id}/users` with role `ADMIN`.
    pub needs_admin_user: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SessionResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<SessionData>,
}

/// The link in the verification email, `None` without `PUBLIC_APP_URL`.
fn verification_url(token: &str) -> Option<String> {
    let base = std::env::var("PUBLIC_APP_URL").ok()?;
    let base = base.trim().trim_end_matches('/');
    (!base.is_empty()).then(|| format!("{base}/verify-email?token={token}"))
}

/// Mail the verification link in the background. Best effort: a missing
/// transport or a provider error is logged (never the token) and signup or
/// resend still succeeds; the user can resend later.
fn queue_verification_email(to: String, name: String, token: String) {
    if cfg!(test) {
        return;
    }
    actix_web::rt::spawn(async move {
        let link = match verification_url(&token) {
            Some(l) => l,
            None => format!("(set PUBLIC_APP_URL) token: {token}"),
        };
        let body = format!(
            "Hi {name},\n\nConfirm your email address to finish setting up your account:\n{link}\n\nThe link works for 24 hours. If you didn't sign up, ignore this email."
        );
        if let Err(err) = send_email(&to, "Verify your email address", &body).await {
            eprintln!("verification email to {to} not sent: {err}");
        }
    });
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct LoginData {
    pub token: String,
    pub org_id: String,
    pub org_name: String,
    /// `ADMIN` for the org-owner login, else the team member's role.
    #[serde(default = "default_admin_role_string")]
    pub role: String,
    /// The team member's display name, when this was a user-login.
    #[serde(default)]
    pub user_name: Option<String>,
}

fn default_admin_role_string() -> String {
    "ADMIN".to_string()
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrgSummaryListResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<Vec<OrgSummary>>,
}

#[utoipa::path(
    get,
    path = "/api/auth/orgs",
    tag = "Auth",
    responses(
        (status = 200, description = "List of registered organizations for login", body = OrgSummaryListResponse)
    )
)]
#[get("/auth/orgs")]
pub async fn auth_orgs() -> impl Responder {
    match OrgCredentials::list_summaries() {
        Ok(summaries) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: format!("Retrieved {} organizations", summaries.len()),
            data: Some(summaries),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to list organizations: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    post,
    path = "/api/auth/login",
    tag = "Auth",
    request_body = LoginPayload,
    responses(
        (status = 200, description = "Login successful, returns JWT token", body = OrgResponse),
        (status = 401, description = "Invalid credentials", body = EmptyResponse)
    )
)]
#[post("/auth/login")]
pub async fn auth_login(payload: web::Json<LoginPayload>) -> impl Responder {
    match OrgCredentials::verify_login(payload.org_id, &payload.password) {
        Ok(Some(org_name)) => match generate_token(payload.org_id, &org_name) {
            Ok(token) => HttpResponse::Ok().json(ApiResponse {
                success: true,
                message: "Login successful".to_string(),
                data: Some(LoginData {
                    token,
                    org_id: payload.org_id.to_string(),
                    org_name,
                    role: "ADMIN".to_string(),
                    user_name: None,
                }),
            }),
            Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
                success: false,
                message: format!("Failed to generate token: {}", err),
                data: None,
            }),
        },
        Ok(None) => HttpResponse::Unauthorized().json(ApiResponse::<String> {
            success: false,
            message: "Invalid organization ID or password".to_string(),
            data: None,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Authentication error: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    post,
    path = "/api/auth/user-login",
    tag = "Auth",
    request_body = UserLoginPayload,
    responses(
        (status = 200, description = "Login successful, returns a role-scoped JWT", body = OrgResponse),
        (status = 401, description = "Invalid credentials or inactive account", body = EmptyResponse)
    )
)]
#[post("/auth/user-login")]
pub async fn user_login(payload: web::Json<UserLoginPayload>) -> impl Responder {
    let user = match OrgUser::verify_login(&payload.email, &payload.password) {
        Ok(Some(u)) => u,
        Ok(None) => {
            return HttpResponse::Unauthorized().json(ApiResponse::<String> {
                success: false,
                message: "Invalid email or password, or the account is inactive".to_string(),
                data: None,
            });
        }
        Err(err) => {
            return HttpResponse::InternalServerError().json(ApiResponse::<String> {
                success: false,
                message: format!("Authentication error: {}", err),
                data: None,
            });
        }
    };

    let org_name = Organization::get_by_id(user.org_id)
        .ok()
        .flatten()
        .map(|o| o.name)
        .unwrap_or_default();

    match generate_user_token(user.org_id, &org_name, Some(user.id), user.role.as_str()) {
        Ok(token) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Login successful".to_string(),
            data: Some(LoginData {
                token,
                org_id: user.org_id.to_string(),
                org_name,
                role: user.role.as_str().to_string(),
                user_name: Some(user.name),
            }),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to generate token: {}", err),
            data: None,
        }),
    }
}

#[utoipa::path(
    post,
    path = "/api/auth/signup",
    tag = "Auth",
    request_body = SignupPayload,
    responses(
        (status = 201, description = "Organization and its first Admin user created; returns a user-scoped JWT like user-login", body = OrgResponse),
        (status = 400, description = "Missing field, weak password or terms not accepted", body = EmptyResponse),
        (status = 409, description = "Email already registered", body = EmptyResponse),
        (status = 429, description = "Too many signups from this address", body = EmptyResponse)
    )
)]
#[post("/auth/signup")]
pub async fn signup(
    req: actix_web::HttpRequest,
    payload: web::Json<SignupPayload>,
) -> impl Responder {
    let fail = |status: actix_web::http::StatusCode, message: String| {
        HttpResponse::build(status).json(ApiResponse::<String> {
            success: false,
            message,
            data: None,
        })
    };
    if !payload.accept_terms {
        return fail(
            actix_web::http::StatusCode::BAD_REQUEST,
            "you must accept the terms and privacy policy".to_string(),
        );
    }
    let ip = signup_client_ip(&req);
    if !signup_allowed(&ip) {
        return fail(
            actix_web::http::StatusCode::TOO_MANY_REQUESTS,
            "too many signups from this address, try again later".to_string(),
        );
    }

    let (org, user, verify_token) = match OrgUser::signup(
        &payload.org_name,
        &payload.address,
        &payload.owner_name,
        &payload.email,
        &payload.password,
    ) {
        Ok(v) => v,
        Err(UserError::EmailTaken) => {
            return fail(
                actix_web::http::StatusCode::CONFLICT,
                UserError::EmailTaken.to_string(),
            );
        }
        Err(UserError::InvalidInput(why)) => {
            return fail(actix_web::http::StatusCode::BAD_REQUEST, why.to_string());
        }
        Err(err) => {
            return fail(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Signup failed: {err}"),
            );
        }
    };

    queue_verification_email(user.email.clone(), user.name.clone(), verify_token);

    match generate_user_token(org.id, &org.name, Some(user.id), user.role.as_str()) {
        Ok(token) => HttpResponse::Created().json(ApiResponse {
            success: true,
            message: "Organization created".to_string(),
            data: Some(LoginData {
                token,
                org_id: org.id.to_string(),
                org_name: org.name,
                role: user.role.as_str().to_string(),
                user_name: Some(user.name),
            }),
        }),
        Err(err) => fail(
            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to generate token: {err}"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/api/auth/me",
    tag = "Auth",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Current authenticated organization", body = OrgResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse),
        (status = 404, description = "Organization not found", body = EmptyResponse)
    )
)]
#[get("/auth/me")]
pub async fn auth_me(auth: AuthenticatedOrg) -> impl Responder {
    match Organization::get_by_id(auth.org_id) {
        Ok(Some(org)) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Authenticated organization retrieved".to_string(),
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
    path = "/api/auth/verify-email",
    tag = "Auth",
    request_body = VerifyEmailPayload,
    responses(
        (status = 200, description = "Email verified", body = EmptyResponse),
        (status = 400, description = "Unknown, expired or already used token", body = EmptyResponse)
    )
)]
#[post("/auth/verify-email")]
pub async fn verify_email(payload: web::Json<VerifyEmailPayload>) -> impl Responder {
    match OrgUser::verify_email(&payload.token) {
        Ok(Some(_)) => HttpResponse::Ok().json(ApiResponse::<String> {
            success: true,
            message: "Email verified".to_string(),
            data: None,
        }),
        Ok(None) => HttpResponse::BadRequest().json(ApiResponse::<String> {
            success: false,
            message: "That verification link is invalid or has expired".to_string(),
            data: None,
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Verification failed: {err}"),
            data: None,
        }),
    }
}

#[utoipa::path(
    post,
    path = "/api/auth/resend-verification",
    tag = "Auth",
    security(("bearer_auth" = [])),
    responses(
        (status = 202, description = "A new verification email was queued", body = EmptyResponse),
        (status = 400, description = "Email already verified, or not a user login", body = EmptyResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse),
        (status = 429, description = "Asked again within a minute", body = EmptyResponse)
    )
)]
#[post("/auth/resend-verification")]
pub async fn resend_verification(auth: AuthenticatedOrg) -> impl Responder {
    let fail = |status: actix_web::http::StatusCode, message: &str| {
        HttpResponse::build(status).json(ApiResponse::<String> {
            success: false,
            message: message.to_string(),
            data: None,
        })
    };
    let Some(user_id) = auth.user_id else {
        return fail(
            actix_web::http::StatusCode::BAD_REQUEST,
            "sign in as a team member to verify an email address",
        );
    };
    let user = match OrgUser::get_by_id(user_id) {
        Ok(Some(u)) => u,
        Ok(None) => return fail(actix_web::http::StatusCode::UNAUTHORIZED, "unknown user"),
        Err(err) => {
            return fail(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to load user: {err}"),
            );
        }
    };
    match OrgUser::resend_verification(user_id) {
        Ok(ResendOutcome::Issued(token)) => {
            queue_verification_email(user.email, user.name, token);
            HttpResponse::Accepted().json(ApiResponse::<String> {
                success: true,
                message: "Verification email sent".to_string(),
                data: None,
            })
        }
        Ok(ResendOutcome::NotPending) => fail(
            actix_web::http::StatusCode::BAD_REQUEST,
            "email is already verified",
        ),
        Ok(ResendOutcome::TooSoon) => fail(
            actix_web::http::StatusCode::TOO_MANY_REQUESTS,
            "a verification email was just sent, wait a minute before asking again",
        ),
        Err(err) => fail(
            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to resend: {err}"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/api/auth/session",
    tag = "Auth",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Verification and upgrade-prompt state for the dashboard banners", body = SessionResponse),
        (status = 401, description = "Unauthorized", body = EmptyResponse)
    )
)]
#[get("/auth/session")]
pub async fn auth_session(auth: AuthenticatedOrg) -> impl Responder {
    let state = (|| -> Result<SessionData, Box<dyn std::error::Error>> {
        let user = match auth.user_id {
            Some(id) => OrgUser::get_by_id(id)?,
            None => None,
        };
        let email_verified = match &user {
            Some(u) => OrgUser::email_verified(u.id)?,
            None => true,
        };
        Ok(SessionData {
            org_id: auth.org_id.to_string(),
            role: auth.role.as_str().to_string(),
            user_id: auth.user_id.map(|i| i.to_string()),
            email: user.map(|u| u.email),
            email_verified,
            needs_admin_user: !OrgUser::org_has_admin(auth.org_id)?,
        })
    })();
    match state {
        Ok(data) => HttpResponse::Ok().json(ApiResponse {
            success: true,
            message: "Session retrieved".to_string(),
            data: Some(data),
        }),
        Err(err) => HttpResponse::InternalServerError().json(ApiResponse::<String> {
            success: false,
            message: format!("Failed to load session: {err}"),
            data: None,
        }),
    }
}
