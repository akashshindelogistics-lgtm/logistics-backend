use super::*;
use crate::logistics::dispatch::dispatch::set_test_public_app_url;

async fn get_public(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
) -> (u16, String) {
    // Deliberately no Authorization header: the link is opened by the customer.
    let req = test::TestRequest::get()
        .uri(&format!("/api/public/track/{token}"))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    (status, body)
}

async fn notifications(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    dispatch_id: Uuid,
    auth: &str,
) -> Vec<Notification> {
    let req = test::TestRequest::get()
        .uri(&format!("/api/dispatches/{dispatch_id}/notifications"))
        .insert_header(("Authorization", auth.to_string()))
        .to_request();
    test::read_body_json::<ApiResponse<Vec<Notification>>, _>(test::call_service(app, req).await)
        .await
        .data
        .unwrap()
}

#[actix_web::test]
async fn test_tracking_link_reaches_the_customer_and_opens_without_login() {
    let _db = TestDb::create();
    set_test_public_app_url(Some("https://track.example/"));
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, auth) = setup_dispatch(&app, "Track Link Org").await;

    let notifs = notifications(&app, dispatch.id, &auth).await;
    let customer = notifs
        .iter()
        .find(|n| n.recipient_kind == "customer")
        .unwrap();
    let driver = notifs
        .iter()
        .find(|n| n.recipient_kind == "driver")
        .unwrap();
    let prefix = "Track your order: https://track.example/track/";
    let at = customer
        .body
        .find(prefix)
        .expect("customer message carries the link");
    let token = customer.body[at + prefix.len()..].trim().to_string();
    assert!(Uuid::parse_str(&token).is_ok(), "token is a UUID: {token}");
    assert!(
        !driver.body.contains("track"),
        "driver gets no customer link"
    );

    let (status, body) = get_public(&app, &token).await;
    assert_eq!(status, 200);
    let tracking: PublicTrackingResponse = serde_json::from_str(&body).unwrap();
    let data = tracking.data.unwrap();
    assert_eq!(data.reference, dispatch.id.to_string()[..8]);
    assert_eq!(data.shipper, org.name);
    assert_eq!(data.status, DispatchStatus::Pending);
    assert_eq!(data.expected_by, dispatch.dispatched_at + 72 * 3600);
    assert!(data.vehicle_position.is_none(), "nothing has moved yet");
    // Only what the customer needs: no customer, vehicle number or goods.
    for private in [
        "Track Link Org Customer",
        "DISP-VH-001",
        "Dispatch Test Goods",
    ] {
        assert!(!body.contains(private), "public response leaks {private}");
    }

    // Once it's moving, the vehicle's last fix shows up.
    for status in [
        DispatchStatus::Confirmed,
        DispatchStatus::Loaded,
        DispatchStatus::InTransit,
    ] {
        let req = test::TestRequest::put()
            .uri(&format!("/api/dispatches/{}/status", dispatch.id))
            .insert_header(("Authorization", auth.clone()))
            .set_json(&UpdateDispatchStatusPayload {
                status,
                proof_of_delivery: None,
                return_to_godown_id: None,
            })
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
    }
    let req = test::TestRequest::put()
        .uri("/api/vehicles/DISP-VH-001/location")
        .insert_header(("Authorization", auth.clone()))
        .set_json(&LocationPayload {
            latitude: 18.52,
            longitude: 73.85,
            address: None,
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);

    let (_, body) = get_public(&app, &token).await;
    let data = serde_json::from_str::<PublicTrackingResponse>(&body)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(data.status, DispatchStatus::InTransit);
    assert_eq!(data.status_history.len(), 4);
    let position = data.vehicle_position.expect("position while in transit");
    assert_eq!((position.latitude, position.longitude), (18.52, 73.85));
    set_test_public_app_url(None);
}

#[actix_web::test]
async fn test_unknown_or_malformed_tracking_token_is_not_found() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, _auth) = setup_dispatch(&app, "Track Missing Org").await;

    assert_eq!(get_public(&app, &Uuid::new_v4().to_string()).await.0, 404);
    assert_eq!(get_public(&app, "not-a-token").await.0, 404);
    // The dispatch id is not a tracking token.
    assert_eq!(get_public(&app, &dispatch.id.to_string()).await.0, 404);
}

#[actix_web::test]
async fn test_no_link_in_messages_without_public_app_url() {
    let _db = TestDb::create();
    set_test_public_app_url(None);
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, auth) = setup_dispatch(&app, "Track Unset Org").await;

    let notifs = notifications(&app, dispatch.id, &auth).await;
    assert!(notifs.iter().all(|n| !n.body.contains("Track your order")));
}
