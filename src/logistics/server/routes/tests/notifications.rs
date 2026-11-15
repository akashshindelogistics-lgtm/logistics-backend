use super::*;

#[actix_web::test]
async fn test_org_notifications_feed_is_scoped_to_the_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, _d, auth) = setup_dispatch(&app, "Notify Feed Org").await;
    let (_other, other_auth) = setup_org(&app, "Notify Feed Intruder").await;

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/notifications", org.id))
        .insert_header(("Authorization", auth))
        .to_request();
    let feed = test::read_body_json::<ApiResponse<Vec<Notification>>, _>(
        test::call_service(&app, req).await,
    )
    .await
    .data
    .unwrap();
    assert_eq!(feed.len(), 2);
    assert!(feed.iter().all(|n| n.org_id == org.id));

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/notifications", org.id))
        .insert_header(("Authorization", other_auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
}

#[actix_web::test]
async fn test_msg91_delivery_reports_set_delivery_status() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, d, auth) = setup_dispatch(&app, "Report Org").await;
    // SAFETY: the delivery-report tests all set this to the same value.
    unsafe { std::env::set_var("MSG91_WEBHOOK_SECRET", "s3cret") };

    let sms = Notification::list_by_dispatch(d.id)
        .unwrap()
        .into_iter()
        .find(|n| n.channel == NotificationChannel::Sms && n.status == NotificationStatus::Queued)
        .expect("driver SMS");
    assert_eq!(sms.delivery_status, None);
    Notification::set_provider_ref(sms.id, "req-1").unwrap();

    let post = |uri: &str, body: serde_json::Value| {
        test::TestRequest::post()
            .uri(uri)
            .set_json(body)
            .to_request()
    };
    let url = "/api/public/msg91/delivery-reports";

    // Wrong secret: refused, nothing stored.
    let resp = test::call_service(
        &app,
        post(
            &format!("{url}?secret=nope"),
            serde_json::json!([{"requestId": "req-1", "status": "1"}]),
        ),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 401);

    // Pending and unknown reports are accepted and ignored.
    let resp = test::call_service(
        &app,
        post(
            &format!("{url}?secret=s3cret"),
            serde_json::json!([{"requestId": "req-1", "status": "0"}, {"requestId": "zzz", "status": "1"}]),
        ),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let after = Notification::list_by_dispatch(d.id).unwrap();
    assert!(after.iter().all(|n| n.delivery_status.is_none()));

    let resp = test::call_service(
        &app,
        post(
            &format!("{url}?secret=s3cret"),
            serde_json::json!({"requestId": "req-1", "status": 1}),
        ),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let req = test::TestRequest::get()
        .uri(&format!("/api/dispatches/{}/notifications", d.id))
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    let list = test::read_body_json::<ApiResponse<Vec<Notification>>, _>(
        test::call_service(&app, req).await,
    )
    .await
    .data
    .unwrap();
    let row = list.iter().find(|n| n.id == sms.id).unwrap();
    assert_eq!(row.delivery_status, Some(DeliveryStatus::Delivered));

    let resp = test::call_service(
        &app,
        post(
            &format!("{url}?secret=s3cret"),
            serde_json::json!([{"requestId": "req-1", "status": "16"}]),
        ),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let list = Notification::list_by_org(org.id, 10).unwrap();
    let row = list.iter().find(|n| n.id == sms.id).unwrap();
    assert_eq!(row.delivery_status, Some(DeliveryStatus::Failed));
}

#[actix_web::test]
async fn test_msg91_delivery_reports_accept_nested_and_form_payloads() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, d, _auth) = setup_dispatch(&app, "Report Shapes Org").await;
    // SAFETY: the delivery-report tests all set this to the same value.
    unsafe { std::env::set_var("MSG91_WEBHOOK_SECRET", "s3cret") };

    let sms = Notification::list_by_dispatch(d.id)
        .unwrap()
        .into_iter()
        .find(|n| n.channel == NotificationChannel::Sms && n.status == NotificationStatus::Queued)
        .expect("driver SMS");
    Notification::set_provider_ref(sms.id, "req-n").unwrap();
    let url = "/api/public/msg91/delivery-reports?secret=s3cret";
    let delivery_status = || {
        Notification::list_by_dispatch(d.id)
            .unwrap()
            .into_iter()
            .find(|n| n.id == sms.id)
            .unwrap()
            .delivery_status
    };

    // MSG91's standard JSON report: the per-number result sits in `report[]`.
    let req = test::TestRequest::post()
        .uri(url)
        .set_json(serde_json::json!([{
            "requestId": "req-n",
            "userId": "1",
            "report": [{"number": "919876543210", "status": "1", "desc": "DELIVERED", "date": "2026-11-15 18:00:00"}],
            "senderId": "BRAND"
        }]))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
    assert_eq!(delivery_status(), Some(DeliveryStatus::Delivered));

    // Legacy webhook: form-encoded `data=<json>`, here a blocked number (17).
    let data = r#"[{"requestId":"req-n","report":[{"number":"919876543210","status":"17","desc":"BLOCKED"}]}]"#;
    let encoded: String = data
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    let form = format!("data={encoded}");
    let req = test::TestRequest::post()
        .uri(url)
        .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
        .set_payload(form)
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
    assert_eq!(delivery_status(), Some(DeliveryStatus::Failed));

    // An unknown code falls back to `desc`.
    let req = test::TestRequest::post()
        .uri(url)
        .set_json(serde_json::json!({"requestId": "req-n", "report": [{"status": "99", "desc": "Delivered"}]}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
    assert_eq!(delivery_status(), Some(DeliveryStatus::Delivered));

    // Code 25 (rejected) is a failure.
    let req = test::TestRequest::post()
        .uri(url)
        .set_json(serde_json::json!({"requestId": "req-n", "report": [{"status": 25}]}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
    assert_eq!(delivery_status(), Some(DeliveryStatus::Failed));

    // Neither JSON nor a form `data` field.
    let req = test::TestRequest::post()
        .uri(url)
        .set_payload("not a report")
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 400);
}
