use super::*;

#[actix_web::test]
async fn test_auth_login_with_valid_credentials_returns_token() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;

    let create_payload = CreateOrgPayload {
        name: "Login Test Org".to_string(),
        address: "1 Auth Road".to_string(),
        password: "login_pass_123".to_string(),
    };
    let req = test::TestRequest::post()
        .uri("/api/orgs")
        .set_json(&create_payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: ApiResponse<Organization> = test::read_body_json(resp).await;
    let org_id = body.data.unwrap().id;

    let login_payload = LoginPayload {
        org_id,
        password: "login_pass_123".to_string(),
    };
    let req = test::TestRequest::post()
        .uri("/api/auth/login")
        .set_json(&login_payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<LoginData> = test::read_body_json(resp).await;
    assert!(body.success);
    assert!(!body.data.unwrap().token.is_empty());
}

#[actix_web::test]
async fn test_auth_login_with_wrong_password_returns_401() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;

    let create_payload = CreateOrgPayload {
        name: "Wrong Pass Org".to_string(),
        address: "2 Auth Road".to_string(),
        password: "correct_pass".to_string(),
    };
    let req = test::TestRequest::post()
        .uri("/api/orgs")
        .set_json(&create_payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: ApiResponse<Organization> = test::read_body_json(resp).await;
    let org_id = body.data.unwrap().id;

    let login_payload = LoginPayload {
        org_id,
        password: "wrong_pass".to_string(),
    };
    let req = test::TestRequest::post()
        .uri("/api/auth/login")
        .set_json(&login_payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 401);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(!body.success);
}

#[actix_web::test]
async fn test_auth_login_with_nonexistent_org_returns_401() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let login_payload = LoginPayload {
        org_id: Uuid::new_v4(),
        password: "any_pass".to_string(),
    };
    let req = test::TestRequest::post()
        .uri("/api/auth/login")
        .set_json(&login_payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 401);
}

#[actix_web::test]
async fn test_auth_login_invalid_payload_returns_400() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::post()
        .uri("/api/auth/login")
        .insert_header(("Content-Type", "application/json"))
        .set_payload("{bad json}")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[actix_web::test]
async fn test_auth_orgs_returns_list_without_auth() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    // First register an org so the list is not empty
    let create_payload = CreateOrgPayload {
        name: "List Orgs Org".to_string(),
        address: "3 Auth Road".to_string(),
        password: "list_pass".to_string(),
    };
    let req = test::TestRequest::post()
        .uri("/api/orgs")
        .set_json(&create_payload)
        .to_request();
    test::call_service(&app, req).await;

    let req = test::TestRequest::get().uri("/api/auth/orgs").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<Vec<OrgSummary>> = test::read_body_json(resp).await;
    assert!(body.success);
    assert!(!body.data.unwrap().is_empty());
}

#[actix_web::test]
async fn test_auth_me_with_valid_token() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;

    let create_payload = CreateOrgPayload {
        name: "Me Endpoint Org".to_string(),
        address: "4 Auth Road".to_string(),
        password: "me_pass".to_string(),
    };
    let req = test::TestRequest::post()
        .uri("/api/orgs")
        .set_json(&create_payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: ApiResponse<Organization> = test::read_body_json(resp).await;
    let org = body.data.unwrap();

    let req = test::TestRequest::get()
        .uri("/api/auth/me")
        .insert_header(("Authorization", make_auth_header(org.id, &org.name)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<Organization> = test::read_body_json(resp).await;
    assert!(body.success);
    assert_eq!(body.data.unwrap().id, org.id);
}

#[actix_web::test]
async fn test_auth_me_without_token_returns_401() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get().uri("/api/auth/me").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 401);
}

#[actix_web::test]
async fn test_user_login_rejects_wrong_password_and_inactive_account() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, admin) = setup_org(&app, "User Login Org").await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/users", org.id))
        .insert_header(("Authorization", admin.clone()))
        .set_json(&CreateUserPayload {
            name: "Dana".to_string(),
            email: "dana@example.com".to_string(),
            password: "dana-password".to_string(),
            role: OrgRole::Dispatcher,
        })
        .to_request();
    let user = test::read_body_json::<ApiResponse<OrgUser>, _>(test::call_service(&app, req).await)
        .await
        .data
        .unwrap();

    let bad = test::TestRequest::post()
        .uri("/api/auth/user-login")
        .set_json(&UserLoginPayload {
            email: "dana@example.com".into(),
            password: "nope".into(),
        })
        .to_request();
    assert_eq!(test::call_service(&app, bad).await.status().as_u16(), 401);

    // Deactivate, then a correct password still fails.
    let req = test::TestRequest::put()
        .uri(&format!("/api/users/{}", user.id))
        .insert_header(("Authorization", admin))
        .set_json(&UpdateUserPayload {
            name: "Dana".into(),
            role: OrgRole::Dispatcher,
            is_active: false,
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);

    let after = test::TestRequest::post()
        .uri("/api/auth/user-login")
        .set_json(&UserLoginPayload {
            email: "dana@example.com".into(),
            password: "dana-password".into(),
        })
        .to_request();
    assert_eq!(test::call_service(&app, after).await.status().as_u16(), 401);
}

fn signup_body(email: &str, password: &str) -> SignupPayload {
    SignupPayload {
        org_name: "Signup Org".to_string(),
        address: "9 Signup Road".to_string(),
        owner_name: "Owner".to_string(),
        email: email.to_string(),
        password: password.to_string(),
        accept_terms: true,
    }
}

async fn post_signup(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
        Error = actix_web::Error,
    >,
    body: &SignupPayload,
    ip: &str,
) -> u16 {
    let req = test::TestRequest::post()
        .uri("/api/auth/signup")
        .peer_addr(format!("{ip}:4000").parse().unwrap())
        .set_json(body)
        .to_request();
    test::call_service(app, req).await.status().as_u16()
}

#[actix_web::test]
async fn test_signup_creates_org_and_admin_and_logs_in() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::post()
        .uri("/api/auth/signup")
        .peer_addr("10.1.0.1:4000".parse().unwrap())
        .set_json(signup_body("  Owner@Example.com ", "passw0rd1"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: ApiResponse<LoginData> = test::read_body_json(resp).await;
    let data = body.data.unwrap();
    assert_eq!(data.role, "ADMIN");
    assert!(!data.token.is_empty());

    let users = OrgUser::list_by_org(data.org_id.parse().unwrap()).unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].email, "owner@example.com");
    assert_eq!(users[0].role, OrgRole::Admin);
    assert!(OrgUser::terms_accepted_at(users[0].id).unwrap().is_some());

    let req = test::TestRequest::post()
        .uri("/api/auth/user-login")
        .set_json(&UserLoginPayload {
            email: "owner@example.com".to_string(),
            password: "passw0rd1".to_string(),
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
}

#[actix_web::test]
async fn test_signup_duplicate_email_is_409_and_creates_no_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let body = signup_body("dup-signup@example.com", "passw0rd1");
    assert_eq!(post_signup(&app, &body, "10.1.0.2").await, 201);
    assert_eq!(post_signup(&app, &body, "10.1.0.2").await, 409);
    assert_eq!(Organization::list_all().unwrap().len(), 1);
}

#[actix_web::test]
async fn test_signup_rejects_weak_password_and_missing_terms() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    for pw in ["short1", "allletters", "12345678"] {
        let body = signup_body("weak@example.com", pw);
        assert_eq!(post_signup(&app, &body, "10.1.0.3").await, 400, "{pw}");
    }
    let mut body = signup_body("terms@example.com", "passw0rd1");
    body.accept_terms = false;
    assert_eq!(post_signup(&app, &body, "10.1.0.3").await, 400);
    let mut body = signup_body("long@example.com", "passw0rd1");
    body.org_name = "x".repeat(256);
    assert_eq!(post_signup(&app, &body, "10.1.0.3").await, 400);
    assert!(Organization::list_all().unwrap().is_empty());
}

#[actix_web::test]
async fn test_signup_is_rate_limited_per_ip() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let limit = super::super::auth::signup_limit();
    for i in 0..limit {
        let body = signup_body(&format!("rl{i}@example.com"), "passw0rd1");
        assert_eq!(post_signup(&app, &body, "10.1.0.4").await, 201);
    }
    let body = signup_body("rl-over@example.com", "passw0rd1");
    assert_eq!(post_signup(&app, &body, "10.1.0.4").await, 429);
    // another address is unaffected
    assert_eq!(post_signup(&app, &body, "10.1.0.5").await, 201);
}

#[actix_web::test]
async fn test_signup_limit_keys_on_rightmost_forwarded_ip_only_behind_trusted_proxy() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let limit = super::super::auth::signup_limit();
    let post = |n: String, xff: &'static str| {
        let app = &app;
        async move {
            let req = test::TestRequest::post()
                .uri("/api/auth/signup")
                .peer_addr("10.1.0.6:4000".parse().unwrap())
                .insert_header(("X-Forwarded-For", xff))
                .set_json(signup_body(&format!("xff{n}@example.com"), "passw0rd1"))
                .to_request();
            test::call_service(app, req).await.status().as_u16()
        }
    };
    // Only this test touches TRUSTED_PROXY, and every other signup test
    // sends no X-Forwarded-For, so flipping it here cannot affect them.
    // SAFETY: as in the notification tests that set and clear env keys.
    unsafe { std::env::set_var("TRUSTED_PROXY", "1") };
    for i in 0..limit {
        // The forged leftmost entry varies; the proxy-appended one does not.
        let xff = if i % 2 == 0 {
            "1.1.1.1, 203.0.113.1"
        } else {
            "2.2.2.2, 203.0.113.1"
        };
        assert_eq!(post(format!("a{i}"), xff).await, 201);
    }
    assert_eq!(post("a-over".into(), "203.0.113.1").await, 429);
    // Same peer, different forwarded client: its own bucket.
    assert_eq!(post("b".into(), "203.0.113.2").await, 201);

    // With the setting off the header is ignored and the peer is the key.
    unsafe { std::env::remove_var("TRUSTED_PROXY") };
    for i in 0..limit {
        assert_eq!(post(format!("c{i}"), "203.0.113.3").await, 201);
    }
    assert_eq!(post("c-over".into(), "203.0.113.4").await, 429);
}

fn bearer_for(auth: &LoginData) -> (&'static str, String) {
    ("Authorization", format!("Bearer {}", auth.token))
}

#[actix_web::test]
async fn test_verify_email_flow_and_session_banner_state() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_, user, token) = OrgUser::signup(
        "Banner Org",
        "1 St",
        "Owner",
        "banner@example.com",
        "passw0rd1",
    )
    .unwrap();
    let login = test::TestRequest::post()
        .uri("/api/auth/user-login")
        .set_json(&UserLoginPayload {
            email: "banner@example.com".into(),
            password: "passw0rd1".into(),
        })
        .to_request();
    let login: ApiResponse<LoginData> =
        test::read_body_json(test::call_service(&app, login).await).await;
    let login = login.data.unwrap();

    macro_rules! session {
        () => {{
            let req = test::TestRequest::get()
                .uri("/api/auth/session")
                .insert_header(bearer_for(&login))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status().as_u16(), 200);
            let body: ApiResponse<SessionData> = test::read_body_json(resp).await;
            body.data.unwrap()
        }};
    }
    let s = session!();
    assert!(!s.email_verified);
    assert!(!s.needs_admin_user, "the owner is an Admin user");
    assert_eq!(s.email.as_deref(), Some("banner@example.com"));

    // Just signed up: a resend is rate limited.
    let req = test::TestRequest::post()
        .uri("/api/auth/resend-verification")
        .insert_header(bearer_for(&login))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 429);

    for (t, want) in [("nope", 400), (token.as_str(), 200), (token.as_str(), 400)] {
        let req = test::TestRequest::post()
            .uri("/api/auth/verify-email")
            .set_json(&VerifyEmailPayload { token: t.into() })
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status().as_u16(), want);
    }
    assert!(session!().email_verified);
    let req = test::TestRequest::post()
        .uri("/api/auth/resend-verification")
        .insert_header(bearer_for(&login))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 400);
    let _ = user;
}

#[actix_web::test]
async fn test_org_without_admin_user_is_prompted_then_creates_one() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, admin) = setup_org(&app, "Legacy Session Org").await;

    let get_session = || {
        test::TestRequest::get()
            .uri("/api/auth/session")
            .insert_header(("Authorization", admin.clone()))
            .to_request()
    };
    let body: ApiResponse<SessionData> =
        test::read_body_json(test::call_service(&app, get_session()).await).await;
    let s = body.data.unwrap();
    assert!(s.needs_admin_user);
    assert!(
        s.email_verified,
        "org-password login has no email to verify"
    );
    assert!(s.user_id.is_none());

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/users", org.id))
        .insert_header(("Authorization", admin.clone()))
        .set_json(&CreateUserPayload {
            name: "First Admin".into(),
            email: "first-admin@example.com".into(),
            password: "passw0rd1".into(),
            role: OrgRole::Admin,
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);
    let body: ApiResponse<SessionData> =
        test::read_body_json(test::call_service(&app, get_session()).await).await;
    assert!(!body.data.unwrap().needs_admin_user);
}

#[actix_web::test]
async fn test_session_requires_a_token() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get()
        .uri("/api/auth/session")
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
}
