use super::*;
use crate::logistics::inbound::{Channel, InboundMessage};

fn webhook(org_id: Uuid, secret: Option<&str>, body: &str) -> test::TestRequest {
    let mut req = test::TestRequest::post()
        .uri(&format!("/api/inbound/{org_id}/messages"))
        .set_json(serde_json::json!({
            "channel": "EMAIL", "sender": "a@b.test", "subject": "Order", "body": body,
        }));
    if let Some(s) = secret {
        req = req.insert_header(("X-Inbound-Secret", s));
    }
    req
}

// One test, because INBOUND_WEBHOOK_SECRET is process-wide.
#[actix_web::test]
async fn test_inbound_webhook_needs_the_shared_secret_and_stores_messages() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Inbound Route Org").await;

    // Not configured: closed.
    unsafe { std::env::remove_var("INBOUND_WEBHOOK_SECRET") };
    let resp = test::call_service(&app, webhook(org.id, Some("x"), "hi").to_request()).await;
    assert_eq!(resp.status().as_u16(), 503);

    unsafe { std::env::set_var("INBOUND_WEBHOOK_SECRET", "s3cret") };
    let resp = test::call_service(&app, webhook(org.id, None, "hi").to_request()).await;
    assert_eq!(resp.status().as_u16(), 401);
    let resp = test::call_service(&app, webhook(org.id, Some("wrong!"), "hi").to_request()).await;
    assert_eq!(resp.status().as_u16(), 401);
    let resp = test::call_service(&app, webhook(org.id, Some("s3cret"), "  ").to_request()).await;
    assert_eq!(resp.status().as_u16(), 400);
    let long = "x".repeat(8_001);
    let resp = test::call_service(&app, webhook(org.id, Some("s3cret"), &long).to_request()).await;
    assert_eq!(resp.status().as_u16(), 400);
    let resp = test::call_service(
        &app,
        webhook(Uuid::new_v4(), Some("s3cret"), "hi").to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 404);

    let resp = test::call_service(
        &app,
        webhook(org.id, Some("s3cret"), "40 cement please").to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);
    unsafe { std::env::remove_var("INBOUND_WEBHOOK_SECRET") };

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/inbound/messages", org.id))
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    let listed = test::read_body_json::<ApiResponse<Vec<InboundMessage>>, _>(
        test::call_service(&app, req).await,
    )
    .await
    .data
    .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].channel, Channel::Email);
    assert_eq!(listed[0].status, "RECEIVED");

    // Another org's token can't read this org's messages.
    let (_other, other_auth) = setup_org(&app, "Other Inbound Org").await;
    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/inbound/messages", org.id))
        .insert_header(("Authorization", other_auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
}
