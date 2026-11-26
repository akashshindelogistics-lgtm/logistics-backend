use super::*;

/// Walk a fresh dispatch through PENDING -> CONFIRMED -> LOADED ->
/// IN_TRANSIT via the API, returning its auth header and id so a test
/// can attempt the final IN_TRANSIT -> DELIVERED move itself.
async fn advance_to_in_transit(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    org_name: &str,
) -> (Uuid, String) {
    let (_org, dispatch, auth) = setup_dispatch(app, org_name).await;
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
        let resp = test::call_service(app, req).await;
        assert_eq!(
            resp.status().as_u16(),
            200,
            "advancing to {status} should succeed"
        );
    }
    (dispatch.id, auth)
}

async fn dispatch_notifications(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    dispatch_id: Uuid,
    auth: &str,
) -> Vec<Notification> {
    let req = test::TestRequest::get()
        .uri(&format!("/api/dispatches/{}/notifications", dispatch_id))
        .insert_header(("Authorization", auth.to_string()))
        .to_request();
    let resp = test::call_service(app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    test::read_body_json::<ApiResponse<Vec<Notification>>, _>(resp)
        .await
        .data
        .unwrap()
}

#[actix_web::test]
async fn test_list_dispatches_without_token_returns_401() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get().uri("/api/dispatches").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 401);
}

#[actix_web::test]
async fn test_dispatch_rejects_customer_from_another_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org_a, auth_a) = setup_org(&app, "Dispatch Cust A").await;
    let (org_b, auth_b) = setup_org(&app, "Dispatch Cust B").await;
    let foreign_customer = create_customer_via_api(&app, org_b.id, &auth_b, "B's Customer").await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org_a.id))
        .insert_header(("Authorization", auth_a))
        .set_json(&DispatchRequestPayload {
            vehicle_source: VehicleSource::Own,
            vendor_id: None,
            vehicle_registration_number: None,
            customer_id: foreign_customer.id,
            line_items: vec![DispatchLineItemPayload {
                stock_description: "Anything".to_string(),
                requested_quantity: 1,
            }],
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(
        body.message.contains("different organization"),
        "{}",
        body.message
    );
}

#[actix_web::test]
async fn test_dispatch_stock_no_stock_returns_error() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let org_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    let payload = DispatchRequestPayload {
        vehicle_source: VehicleSource::Own,
        vendor_id: None,
        vehicle_registration_number: None,
        customer_id,
        line_items: vec![DispatchLineItemPayload {
            stock_description: "Nonexistent Stock Description".to_string(),
            requested_quantity: 10,
        }],
    };
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org_id))
        .insert_header(("Authorization", make_auth_header(org_id, "Test")))
        .set_json(&payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(!body.success);
}

#[actix_web::test]
async fn test_dispatch_stock_invalid_payload() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let org_id = Uuid::new_v4();
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org_id))
        .insert_header(("Content-Type", "application/json"))
        .insert_header(("Authorization", make_auth_header(org_id, "Test")))
        .set_payload("{bad json}")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[actix_web::test]
async fn test_dispatch_stock_carries_multiple_line_items() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    // setup_dispatch already dispatched "Dispatch Test Goods" x10 on the
    // one godown+vehicle; reuse its org/customer for a second, multi-line
    // dispatch. The vehicle is now on an active trip, so add a second
    // vehicle + driver and more stock first.
    let (org, first, auth) = setup_dispatch(&app, "Multi Line Dispatch Org").await;
    assert_eq!(first.line_items.len(), 1);
    // The dispatch route response carries the stock's category through.
    assert_eq!(first.line_items[0].category, "General");

    // Second vehicle + active driver.
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/vehicles", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateVehiclePayload {
            registration_number: "DISP-VH-002".to_string(),
            capacity: 100_000,
            unit: "MetricTon".to_string(),
            vehicle_type: None,
        })
        .to_request();
    test::call_service(&app, req).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/drivers", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateDriverPayload {
            name: "Second Driver".to_string(),
            license_number: "DISP-LIC-002".to_string(),
            phone: "+91 90000 00001".to_string(),
        })
        .to_request();
    let driver: ApiResponse<Driver> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let req = test::TestRequest::put()
        .uri("/api/vehicles/DISP-VH-002/driver")
        .insert_header(("Authorization", auth.clone()))
        .set_json(&AssignDriverPayload {
            driver_id: Some(driver.data.unwrap().id),
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);

    // A second stock item in the same org (a fresh godown).
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/godowns", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateGodownPayload {
            name: "Second Godown".to_string(),
            address: "9 Second Road".to_string(),
            max_capacity: None,
        })
        .to_request();
    let godown2: ApiResponse<Godown> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let godown2 = godown2.data.unwrap();
    for (desc, qty) in [("Bricks", 500i64), ("Tiles", 300i64)] {
        let req = test::TestRequest::post()
            .uri(&format!("/api/godowns/{}/stock", godown2.id))
            .insert_header(("Authorization", auth.clone()))
            .set_json(&CreateStockPayload {
                volume_in_size: 1,
                quantity: qty,
                description: desc.to_string(),
                category: "General".to_string(),
                reorder_threshold: None,
            })
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);
    }

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&DispatchRequestPayload {
            vehicle_source: VehicleSource::Own,
            vendor_id: None,
            vehicle_registration_number: None,
            customer_id: first.customer_id,
            line_items: vec![
                DispatchLineItemPayload {
                    stock_description: "Bricks".to_string(),
                    requested_quantity: 120,
                },
                DispatchLineItemPayload {
                    stock_description: "Tiles".to_string(),
                    requested_quantity: 40,
                },
            ],
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<DispatchOrder> = test::read_body_json(resp).await;
    let order = body.data.unwrap();
    assert_eq!(order.line_items.len(), 2);
    assert_eq!(order.line_items[0].stock_description, "Bricks");
    assert_eq!(order.line_items[1].stock_description, "Tiles");
    assert_eq!(
        order.line_items.iter().map(|li| li.quantity).sum::<i64>(),
        160
    );

    // A duplicated description is rejected with 400.
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org.id))
        .insert_header(("Authorization", auth))
        .set_json(&DispatchRequestPayload {
            vehicle_source: VehicleSource::Own,
            vendor_id: None,
            vehicle_registration_number: None,
            customer_id: first.customer_id,
            line_items: vec![
                DispatchLineItemPayload {
                    stock_description: "Bricks".to_string(),
                    requested_quantity: 1,
                },
                DispatchLineItemPayload {
                    stock_description: "Bricks".to_string(),
                    requested_quantity: 2,
                },
            ],
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 400);
}

#[actix_web::test]
async fn test_list_dispatches_with_valid_token_returns_200() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let org_id = Uuid::new_v4();
    let req = test::TestRequest::get()
        .uri("/api/dispatches")
        .insert_header(("Authorization", make_auth_header(org_id, "Test")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<Vec<DispatchOrder>> = test::read_body_json(resp).await;
    assert!(body.success);
    // No dispatches for a brand-new UUID org_id — empty list is valid
    assert_eq!(body.data.unwrap().len(), 0);
}

#[actix_web::test]
async fn test_update_dispatch_status_without_token_returns_401() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", Uuid::new_v4()))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Confirmed,
            proof_of_delivery: None,
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 401);
}

#[actix_web::test]
async fn test_update_dispatch_status_nonexistent_dispatch_returns_404() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", Uuid::new_v4()))
        .insert_header(("Authorization", make_auth_header(Uuid::new_v4(), "Test")))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Confirmed,
            proof_of_delivery: None,
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 404);
}

#[actix_web::test]
async fn test_update_dispatch_status_invalid_payload() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", Uuid::new_v4()))
        .insert_header(("Content-Type", "application/json"))
        .insert_header(("Authorization", make_auth_header(Uuid::new_v4(), "Test")))
        .set_payload("{bad json}")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[actix_web::test]
async fn test_update_dispatch_status_returns_403_for_different_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, _auth) = setup_dispatch(&app, "Status Owner Org").await;

    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch.id))
        .insert_header((
            "Authorization",
            make_auth_header(Uuid::new_v4(), "Attacker"),
        ))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Confirmed,
            proof_of_delivery: None,
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403);
}

#[actix_web::test]
async fn test_update_dispatch_status_valid_transition_succeeds() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, auth) = setup_dispatch(&app, "Status Transition Org").await;
    assert_eq!(dispatch.status, DispatchStatus::Pending);

    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch.id))
        .insert_header(("Authorization", auth))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Confirmed,
            proof_of_delivery: None,
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<DispatchOrder> = test::read_body_json(resp).await;
    assert!(body.success);
    let updated = body.data.unwrap();
    assert_eq!(updated.status, DispatchStatus::Confirmed);
    assert_eq!(updated.status_history.len(), 2);
    assert_eq!(updated.status_history[0].status, DispatchStatus::Pending);
    assert_eq!(updated.status_history[1].status, DispatchStatus::Confirmed);
}

#[actix_web::test]
async fn test_update_dispatch_status_rejects_illegal_transition() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, auth) = setup_dispatch(&app, "Status Illegal Org").await;

    // PENDING -> DELIVERED skips CONFIRMED/LOADED/IN_TRANSIT — must be rejected,
    // even with proof of delivery attached (the state-machine check runs first).
    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch.id))
        .insert_header(("Authorization", auth))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Delivered,
            proof_of_delivery: Some(ProofOfDeliveryPayload {
                receiver_name: "Priya Sharma".to_string(),
                signature_or_photo_url: "https://example.com/sig.png".to_string(),
            }),
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(!body.success);
}

#[actix_web::test]
async fn test_update_dispatch_status_returned_credits_stock_back_into_a_godown() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, auth) = setup_dispatch(&app, "Return Flow Org").await;
    // setup_dispatch stocked "Dispatch Test Goods" x100 and dispatched 10 of it.

    async fn goods_qty(
        app: &impl actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
        org_id: Uuid,
        auth: &str,
    ) -> i64 {
        let req = test::TestRequest::get()
            .uri(&format!("/api/orgs/{org_id}/godowns"))
            .insert_header(("Authorization", auth.to_string()))
            .to_request();
        let body: ApiResponse<Vec<Godown>> =
            test::read_body_json(test::call_service(app, req).await).await;
        body.data
            .unwrap()
            .iter()
            .flat_map(|g| &g.stock)
            .filter(|s| s.description == "Dispatch Test Goods")
            .map(|s| s.quantity)
            .sum()
    }

    assert_eq!(
        goods_qty(&app, org.id, &auth).await,
        90,
        "10 units were dispatched"
    );

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
        .uri(&format!("/api/dispatches/{}/status", dispatch.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Returned,
            proof_of_delivery: None,
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<DispatchOrder> = test::read_body_json(resp).await;
    assert_eq!(body.data.unwrap().status, DispatchStatus::Returned);

    // The 10 returned units are back in the godown.
    assert_eq!(goods_qty(&app, org.id, &auth).await, 100);
}

#[actix_web::test]
async fn test_update_dispatch_status_delivered_without_proof_returns_400() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (dispatch_id, auth) = advance_to_in_transit(&app, "POD Missing Org").await;

    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch_id))
        .insert_header(("Authorization", auth))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Delivered,
            proof_of_delivery: None,
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(!body.success);
    assert!(body.message.contains("Proof of delivery"));
}

#[actix_web::test]
async fn test_update_dispatch_status_delivered_with_proof_succeeds() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (dispatch_id, auth) = advance_to_in_transit(&app, "POD Present Org").await;

    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch_id))
        .insert_header(("Authorization", auth))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Delivered,
            proof_of_delivery: Some(ProofOfDeliveryPayload {
                receiver_name: "Priya Sharma".to_string(),
                signature_or_photo_url: "https://example.com/sig.png".to_string(),
            }),
            return_to_godown_id: None,
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<DispatchOrder> = test::read_body_json(resp).await;
    assert!(body.success);
    let updated = body.data.unwrap();
    assert_eq!(updated.status, DispatchStatus::Delivered);
    let proof = updated.proof_of_delivery.expect("proof should be set");
    assert_eq!(proof.receiver_name, "Priya Sharma");
    assert_eq!(proof.signature_or_photo_url, "https://example.com/sig.png");
    assert!(proof.delivered_at > 0);

    // And it round-trips through a fresh fetch, not just the response.
    let fetched = DispatchOrder::get_by_id(dispatch_id).unwrap().unwrap();
    assert!(fetched.proof_of_delivery.is_some());
}

/// Upload a (fake) PNG for `org_id` via `POST /api/orgs/{id}/uploads` and
/// return its `/api/uploads/{id}` URL.
async fn upload_pod_png(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    org_id: Uuid,
    auth: &str,
) -> String {
    let boundary = "PodBoundary1234";
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/uploads", org_id))
        .insert_header(("Authorization", auth.to_string()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(multipart_body(
            boundary,
            "pod.png",
            "image/png",
            b"fake png bytes for a pod test",
        ))
        .to_request();
    let resp = test::call_service(app, req).await;
    assert_eq!(resp.status().as_u16(), 200, "upload should succeed");
    test::read_body_json::<ApiResponse<UploadedFileRef>, _>(resp)
        .await
        .data
        .unwrap()
        .url
}

async fn deliver_with_proof_url(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    dispatch_id: Uuid,
    auth: &str,
    url: &str,
) -> actix_web::dev::ServiceResponse {
    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch_id))
        .insert_header(("Authorization", auth.to_string()))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Delivered,
            proof_of_delivery: Some(ProofOfDeliveryPayload {
                receiver_name: "Priya Sharma".to_string(),
                signature_or_photo_url: url.to_string(),
            }),
            return_to_godown_id: None,
        })
        .to_request();
    test::call_service(app, req).await
}

// The proof-of-delivery vision check fails open. CI has no
// ANTHROPIC_API_KEY, and with one set the fake PNG bytes are rejected by the
// API, so either way the check can't run and the delivery must go through.
#[actix_web::test]
async fn test_delivered_with_own_org_upload_url_succeeds_when_the_check_cannot_run() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (dispatch_id, auth) = advance_to_in_transit(&app, "POD Upload Org").await;
    let org_id = DispatchOrder::get_by_id(dispatch_id)
        .unwrap()
        .unwrap()
        .org_id;
    let url = upload_pod_png(&app, org_id, &auth).await;
    assert!(url.starts_with("/api/uploads/"));

    let resp = deliver_with_proof_url(&app, dispatch_id, &auth, &url).await;
    assert_eq!(resp.status().as_u16(), 200);
    let updated = test::read_body_json::<ApiResponse<DispatchOrder>, _>(resp)
        .await
        .data
        .unwrap();
    assert_eq!(updated.status, DispatchStatus::Delivered);
    assert_eq!(
        updated.proof_of_delivery.unwrap().signature_or_photo_url,
        url
    );
}

#[actix_web::test]
async fn test_delivered_with_absolute_upload_url_succeeds_when_the_check_cannot_run() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (dispatch_id, auth) = advance_to_in_transit(&app, "POD Abs Upload Org").await;
    let org_id = DispatchOrder::get_by_id(dispatch_id)
        .unwrap()
        .unwrap()
        .org_id;
    let url = format!(
        "https://api.example.com{}",
        upload_pod_png(&app, org_id, &auth).await
    );

    let resp = deliver_with_proof_url(&app, dispatch_id, &auth, &url).await;
    assert_eq!(resp.status().as_u16(), 200);
    let fetched = DispatchOrder::get_by_id(dispatch_id).unwrap().unwrap();
    assert_eq!(fetched.status, DispatchStatus::Delivered);
}

#[actix_web::test]
async fn test_delivered_with_another_orgs_upload_id_succeeds_without_a_check() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (other_org, other_auth) = setup_org(&app, "POD Other Org").await;
    let foreign_url = upload_pod_png(&app, other_org.id, &other_auth).await;
    let (dispatch_id, auth) = advance_to_in_transit(&app, "POD Own Org").await;

    // Pins current behaviour: another org's upload is never read or sent to
    // the model, and the URL is stored as an opaque proof string.
    let resp = deliver_with_proof_url(&app, dispatch_id, &auth, &foreign_url).await;
    assert_eq!(resp.status().as_u16(), 200);
    let fetched = DispatchOrder::get_by_id(dispatch_id).unwrap().unwrap();
    assert_eq!(fetched.status, DispatchStatus::Delivered);
}

#[actix_web::test]
async fn test_redelivering_with_an_upload_gets_the_transition_error() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (dispatch_id, auth) = advance_to_in_transit(&app, "POD Retry Org").await;
    let org_id = DispatchOrder::get_by_id(dispatch_id)
        .unwrap()
        .unwrap()
        .org_id;
    let url = upload_pod_png(&app, org_id, &auth).await;
    let resp = deliver_with_proof_url(&app, dispatch_id, &auth, &url).await;
    assert_eq!(resp.status().as_u16(), 200);

    // A retried "Confirm Delivery" is an illegal DELIVERED -> DELIVERED move:
    // the state-machine error, never the "upload a clearer one" message.
    let resp = deliver_with_proof_url(&app, dispatch_id, &auth, &url).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(!body.success);
    assert!(
        !body.message.contains("clearer"),
        "unexpected: {}",
        body.message
    );
}

#[actix_web::test]
async fn test_dispatch_stock_returns_403_for_different_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let target_org_id = Uuid::new_v4();
    let attacker_org_id = Uuid::new_v4();
    let payload = DispatchRequestPayload {
        vehicle_source: VehicleSource::Own,
        vendor_id: None,
        vehicle_registration_number: None,
        customer_id: Uuid::new_v4(),
        line_items: vec![DispatchLineItemPayload {
            stock_description: "Stolen Stock".to_string(),
            requested_quantity: 10,
        }],
    };
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", target_org_id))
        .insert_header((
            "Authorization",
            make_auth_header(attacker_org_id, "Attacker"),
        ))
        .set_json(&payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(!body.success);
}

#[actix_web::test]
async fn test_creating_a_dispatch_records_customer_and_driver_notifications() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, auth) = setup_dispatch(&app, "Notify Create Org").await;

    let notifs = dispatch_notifications(&app, dispatch.id, &auth).await;
    assert_eq!(notifs.len(), 2);
    assert!(
        notifs
            .iter()
            .all(|n| n.event == NotificationEvent::DispatchCreated)
    );

    // setup_dispatch's customer has no contact details → SKIPPED.
    let customer = notifs
        .iter()
        .find(|n| n.recipient_kind == "customer")
        .unwrap();
    assert_eq!(customer.status, NotificationStatus::Skipped);

    // The assigned driver has a phone → a QUEUED SMS.
    let driver = notifs
        .iter()
        .find(|n| n.recipient_kind == "driver")
        .unwrap();
    assert_eq!(driver.status, NotificationStatus::Queued);
    assert_eq!(driver.channel, NotificationChannel::Sms);
}

#[actix_web::test]
async fn test_delivering_a_dispatch_records_a_delivered_notification() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (dispatch_id, auth) = advance_to_in_transit(&app, "Notify Deliver Org").await;

    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch_id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Delivered,
            proof_of_delivery: Some(ProofOfDeliveryPayload {
                receiver_name: "R".to_string(),
                signature_or_photo_url: "https://x/y.png".to_string(),
            }),
            return_to_godown_id: None,
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);

    let notifs = dispatch_notifications(&app, dispatch_id, &auth).await;
    let delivered: Vec<_> = notifs
        .iter()
        .filter(|n| n.event == NotificationEvent::DispatchDelivered)
        .collect();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].recipient_kind, "customer");
    assert!(delivered[0].body.contains("delivered"));
}

#[actix_web::test]
async fn test_dispatcher_can_dispatch_but_warehouse_staff_cannot() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, admin) = setup_dispatch(&app, "Role Dispatch Org").await;
    let customer_id = dispatch.customer_id;

    // setup_dispatch already put the one vehicle on a PENDING trip; cancel
    // it (as Admin) so a fresh dispatch has a free vehicle to pick.
    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/status", dispatch.id))
        .insert_header(("Authorization", admin.clone()))
        .set_json(&UpdateDispatchStatusPayload {
            status: DispatchStatus::Cancelled,
            proof_of_delivery: None,
            return_to_godown_id: None,
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);

    let dispatcher =
        add_user_and_login(&app, org.id, &admin, "d@example.com", OrgRole::Dispatcher).await;
    let warehouse = add_user_and_login(
        &app,
        org.id,
        &admin,
        "w@example.com",
        OrgRole::WarehouseStaff,
    )
    .await;

    let body = DispatchRequestPayload {
        vehicle_source: VehicleSource::Own,

        vendor_id: None,
        vehicle_registration_number: None,
        customer_id,
        line_items: vec![DispatchLineItemPayload {
            stock_description: "Dispatch Test Goods".to_string(),
            requested_quantity: 1,
        }],
    };

    // Warehouse staff: 403.
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org.id))
        .insert_header(("Authorization", warehouse))
        .set_json(&body)
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);

    // Dispatcher: allowed (200 — the setup org has stock + a free vehicle).
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org.id))
        .insert_header(("Authorization", dispatcher))
        .set_json(&body)
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
}

#[actix_web::test]
async fn test_dispatch_rejected_when_no_vehicle_has_an_active_driver() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "NoDriver Co").await;

    // Vehicle, godown+stock, customer with a location — but no driver.
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/vehicles", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateVehiclePayload {
            registration_number: "ND-VH-1".to_string(),
            capacity: 20,
            unit: "MetricTon".to_string(),
            vehicle_type: None,
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/godowns", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateGodownPayload {
            name: "ND Godown".to_string(),
            address: "1 ND Road".to_string(),
            max_capacity: None,
        })
        .to_request();
    let godown: ApiResponse<Godown> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let godown = godown.data.unwrap();

    let req = test::TestRequest::post()
        .uri(&format!("/api/godowns/{}/stock", godown.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateStockPayload {
            volume_in_size: 1,
            quantity: 100,
            description: "Widgets".to_string(),
            category: "General".to_string(),
            reorder_threshold: None,
        })
        .to_request();
    test::call_service(&app, req).await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/customers", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateCustomerPayload {
            name: "ND Customer".to_string(),
            address: "2 ND Lane".to_string(),
            latitude: None,
            longitude: None,
            location_address: None,
            phone: None,
            email: None,
            gstin: None,
        })
        .to_request();
    let customer: ApiResponse<Customer> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let customer = customer.data.unwrap();

    let req = test::TestRequest::put()
        .uri(&format!("/api/customers/{}/location", customer.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&LocationPayload {
            latitude: 19.07,
            longitude: 72.87,
            address: Some("Mumbai".to_string()),
        })
        .to_request();
    test::call_service(&app, req).await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch", org.id))
        .insert_header(("Authorization", auth))
        .set_json(&DispatchRequestPayload {
            vehicle_source: VehicleSource::Own,
            vendor_id: None,
            vehicle_registration_number: None,
            customer_id: customer.id,
            line_items: vec![DispatchLineItemPayload {
                stock_description: "Widgets".to_string(),
                requested_quantity: 5,
            }],
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(
        body.message.contains("active assigned driver"),
        "{}",
        body.message
    );
}

async fn get_challan(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    dispatch_id: Uuid,
    auth: Option<&str>,
) -> actix_web::dev::ServiceResponse {
    let mut req =
        test::TestRequest::get().uri(&format!("/api/dispatches/{dispatch_id}/challan.pdf"));
    if let Some(auth) = auth {
        req = req.insert_header(("Authorization", auth.to_string()));
    }
    test::call_service(app, req.to_request()).await
}

#[actix_web::test]
async fn test_dispatch_challan_is_a_pdf_with_the_dispatch_details() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, auth) = setup_dispatch(&app, "Challan Org").await;

    let resp = get_challan(&app, dispatch.id, Some(&auth)).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/pdf"
    );
    let short = dispatch.id.to_string()[..8].to_ascii_uppercase();
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        &format!("inline; filename=\"challan-DC-{short}.pdf\"")
    );
    let body = test::read_body(resp).await;
    let text: String = body.iter().map(|&b| b as char).collect();
    assert!(text.starts_with("%PDF-1.4"));
    for expected in [
        format!("(No. DC-{short})"),
        "(Challan Org)".to_string(),
        "(Challan Org Customer)".to_string(),
        "(DISP-VH-001)".to_string(),
        "(Challan Org Driver)".to_string(),
        "(DISP-LIC-001)".to_string(),
        "(Dispatch Test Goods)".to_string(),
    ] {
        assert!(text.contains(&expected), "challan is missing {expected}");
    }
}

#[actix_web::test]
async fn test_dispatch_challan_requires_auth_and_the_owning_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, _auth) = setup_dispatch(&app, "Challan Owner").await;
    let (_other, other_auth) = setup_org(&app, "Challan Stranger").await;

    let resp = get_challan(&app, dispatch.id, None).await;
    assert_eq!(resp.status().as_u16(), 401);
    let resp = get_challan(&app, dispatch.id, Some(&other_auth)).await;
    assert_eq!(resp.status().as_u16(), 403);
    let resp = get_challan(&app, Uuid::new_v4(), Some(&other_auth)).await;
    assert_eq!(resp.status().as_u16(), 404);
}

#[actix_web::test]
async fn test_any_role_can_print_a_challan() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, auth) = setup_dispatch(&app, "Challan Roles").await;
    let staff = add_user_and_login(
        &app,
        org.id,
        &auth,
        "challan-staff@example.com",
        OrgRole::WarehouseStaff,
    )
    .await;

    let resp = get_challan(&app, dispatch.id, Some(&staff)).await;
    assert_eq!(resp.status().as_u16(), 200);
}

/// An org with a located customer, 100 units of 1-volume goods and one
/// vehicle per `(reg, capacity, has_driver)`. Returns `(org, auth, customer_id)`.
async fn setup_fleet(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    org_name: &str,
    vehicles: &[(&str, i64, bool)],
) -> (Organization, String, Uuid) {
    let (org, auth) = setup_org(app, org_name).await;
    for (i, (reg, capacity, has_driver)) in vehicles.iter().enumerate() {
        let req = test::TestRequest::post()
            .uri(&format!("/api/orgs/{}/vehicles", org.id))
            .insert_header(("Authorization", auth.clone()))
            .set_json(&CreateVehiclePayload {
                registration_number: reg.to_string(),
                capacity: *capacity,
                unit: "MetricTon".to_string(),
                vehicle_type: None,
            })
            .to_request();
        assert_eq!(test::call_service(app, req).await.status().as_u16(), 201);
        if !has_driver {
            continue;
        }
        let req = test::TestRequest::post()
            .uri(&format!("/api/orgs/{}/drivers", org.id))
            .insert_header(("Authorization", auth.clone()))
            .set_json(&CreateDriverPayload {
                name: format!("Driver {i}"),
                license_number: format!("{org_name}-LIC-{i}"),
                phone: "+91 90000 00000".to_string(),
            })
            .to_request();
        let body: ApiResponse<Driver> =
            test::read_body_json(test::call_service(app, req).await).await;
        let req = test::TestRequest::put()
            .uri(&format!("/api/vehicles/{reg}/driver"))
            .insert_header(("Authorization", auth.clone()))
            .set_json(&AssignDriverPayload {
                driver_id: Some(body.data.unwrap().id),
            })
            .to_request();
        assert_eq!(test::call_service(app, req).await.status().as_u16(), 200);
    }
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/godowns", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateGodownPayload {
            name: "Fleet Godown".to_string(),
            address: "1 Godown Road".to_string(),
            max_capacity: None,
        })
        .to_request();
    let body: ApiResponse<Godown> = test::read_body_json(test::call_service(app, req).await).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/godowns/{}/stock", body.data.unwrap().id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateStockPayload {
            volume_in_size: 1,
            quantity: 100,
            description: "Fleet Goods".to_string(),
            category: "General".to_string(),
            reorder_threshold: None,
        })
        .to_request();
    assert!(test::call_service(app, req).await.status().is_success());
    let customer = located_customer_at(app, org.id, &auth, "Fleet Customer", 19.07, 72.87).await;
    (org, auth, customer)
}

fn fleet_dispatch(
    customer_id: Uuid,
    quantity: i64,
    vehicle: Option<&str>,
) -> DispatchRequestPayload {
    DispatchRequestPayload {
        customer_id,
        line_items: vec![DispatchLineItemPayload {
            stock_description: "Fleet Goods".to_string(),
            requested_quantity: quantity,
        }],
        vehicle_source: VehicleSource::Own,
        vendor_id: None,
        vehicle_registration_number: vehicle.map(str::to_string),
    }
}

async fn post_fleet_dispatch(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    org_id: Uuid,
    auth: &str,
    payload: &DispatchRequestPayload,
) -> (u16, ApiResponse<DispatchOrder>) {
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{org_id}/dispatch"))
        .insert_header(("Authorization", auth.to_string()))
        .set_json(payload)
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    (status, test::read_body_json(resp).await)
}

#[actix_web::test]
async fn test_dispatch_with_chosen_vehicle_uses_it() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(
        &app,
        "Override Ok",
        &[("OV-A", 1000, true), ("OV-B", 1000, true)],
    )
    .await;
    for reg in ["OV-A", "OV-B"] {
        // Each run must see the other vehicle free, so only one dispatch per
        // vehicle: pick the second one after the first is taken.
        let (status, body) =
            post_fleet_dispatch(&app, org.id, &auth, &fleet_dispatch(customer, 1, Some(reg))).await;
        assert_eq!(status, 200, "{}", body.message);
        assert_eq!(
            body.data.unwrap().vehicle_registration_number.as_deref(),
            Some(reg)
        );
    }
}

#[actix_web::test]
async fn test_dispatch_chosen_vehicle_matches_case_and_whitespace_insensitively() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) =
        setup_fleet(&app, "Override Case", &[("MH12AB1234", 1000, true)]).await;
    let (status, body) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 1, Some("  mh12ab1234 ")),
    )
    .await;
    assert_eq!(status, 200, "{}", body.message);
    assert_eq!(
        body.data.unwrap().vehicle_registration_number.as_deref(),
        Some("MH12AB1234")
    );
}

#[actix_web::test]
async fn test_dispatch_without_vehicle_still_auto_selects() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(
        &app,
        "Override Auto",
        &[("AU-A", 1000, true), ("AU-B", 1000, true)],
    )
    .await;
    let (status, body) =
        post_fleet_dispatch(&app, org.id, &auth, &fleet_dispatch(customer, 1, None)).await;
    assert_eq!(status, 200, "{}", body.message);
    let reg = body.data.unwrap().vehicle_registration_number.unwrap();
    assert!(reg == "AU-A" || reg == "AU-B", "{reg}");
}

#[actix_web::test]
async fn test_dispatch_rejects_chosen_vehicle_of_another_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(&app, "Override Mine", &[("MY-A", 1000, true)]).await;
    setup_fleet(&app, "Override Theirs", &[("THEIR-A", 1000, true)]).await;
    let (status, body) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 1, Some("THEIR-A")),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        body.message.contains("not found in this organization"),
        "{}",
        body.message
    );
}

#[actix_web::test]
async fn test_dispatch_rejects_busy_chosen_vehicle() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(
        &app,
        "Override Busy",
        &[("BU-A", 1000, true), ("BU-B", 1000, true)],
    )
    .await;
    let (status, _) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 1, Some("BU-A")),
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 1, Some("BU-A")),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        body.message.contains("already on an active dispatch"),
        "{}",
        body.message
    );
}

#[actix_web::test]
async fn test_dispatch_rejects_over_capacity_chosen_vehicle() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(
        &app,
        "Override Small",
        &[("SM-A", 5, true), ("SM-B", 1000, true)],
    )
    .await;
    let (status, body) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 10, Some("SM-A")),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body.message.contains("too small"), "{}", body.message);
}

#[actix_web::test]
async fn test_dispatch_rejects_chosen_vehicle_without_driver() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(
        &app,
        "Override NoDrv",
        &[("ND-A", 1000, false), ("ND-B", 1000, true)],
    )
    .await;
    let (status, body) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 1, Some("ND-A")),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        body.message.contains("no active assigned driver"),
        "{}",
        body.message
    );
}

#[actix_web::test]
async fn test_vehicle_preview_lists_only_eligible_vehicles() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(
        &app,
        "Preview Fleet",
        &[
            ("PV-A", 1000, true),
            ("PV-B", 1000, true),
            ("PV-SMALL", 5, true),
            ("PV-NODRV", 1000, false),
        ],
    )
    .await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch/vehicle-preview", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&VehiclePreviewPayload {
            customer_id: customer,
            line_items: vec![DispatchLineItemPayload {
                stock_description: "Fleet Goods".to_string(),
                requested_quantity: 10,
            }],
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<VehiclePreview> = test::read_body_json(resp).await;
    let preview = body.data.unwrap();
    let mut regs: Vec<_> = preview
        .eligible
        .iter()
        .map(|v| v.registration_number.clone())
        .collect();
    assert_eq!(preview.suggested.registration_number, regs[0]);
    regs.sort();
    assert_eq!(regs, ["PV-A", "PV-B"]);

    // Previewing reserves nothing: stock and vehicles are untouched.
    let (status, _) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 10, Some(&preview.suggested.registration_number)),
    )
    .await;
    assert_eq!(status, 200);
}

#[actix_web::test]
async fn test_vehicle_preview_without_eligible_vehicle_is_400() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(&app, "Preview None", &[("PN-A", 5, true)]).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch/vehicle-preview", org.id))
        .insert_header(("Authorization", auth))
        .set_json(&VehiclePreviewPayload {
            customer_id: customer,
            line_items: vec![DispatchLineItemPayload {
                stock_description: "Fleet Goods".to_string(),
                requested_quantity: 10,
            }],
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 400);
}

#[actix_web::test]
async fn test_dispatch_rejects_chosen_vehicle_when_customer_has_no_location() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, _) = setup_fleet(&app, "Override NoLoc", &[("NL-A", 1000, true)]).await;
    let unlocated = create_customer_via_api(&app, org.id, &auth, "Nowhere").await;
    let (status, body) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(unlocated.id, 1, Some("NL-A")),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        body.message.contains("location is not set"),
        "{}",
        body.message
    );
}

#[actix_web::test]
async fn test_dispatch_with_blank_vehicle_auto_selects() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(&app, "Override Blank", &[("BL-A", 1000, true)]).await;
    let (status, body) = post_fleet_dispatch(
        &app,
        org.id,
        &auth,
        &fleet_dispatch(customer, 1, Some("  ")),
    )
    .await;
    assert_eq!(status, 200, "{}", body.message);
    assert_eq!(
        body.data.unwrap().vehicle_registration_number.as_deref(),
        Some("BL-A")
    );
}

#[actix_web::test]
async fn test_vehicle_preview_measures_a_vehicle_without_a_fix_from_the_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    // The customer from setup_fleet is in Mumbai (19.07, 72.87).
    let (org, auth, customer) = setup_fleet(
        &app,
        "Preview OrgLoc",
        &[("OL-NOFIX", 1000, true), ("OL-DELHI", 1000, true)],
    )
    .await;
    let put_location = |uri: String, latitude: f64, longitude: f64| {
        test::TestRequest::put()
            .uri(&uri)
            .insert_header(("Authorization", auth.clone()))
            .set_json(&LocationPayload {
                latitude,
                longitude,
                address: None,
            })
            .to_request()
    };
    let req = put_location(format!("/api/orgs/{}/location", org.id), 19.07, 72.87);
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
    let req = put_location("/api/vehicles/OL-DELHI/location".to_string(), 28.61, 77.21);
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);

    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/dispatch/vehicle-preview", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&VehiclePreviewPayload {
            customer_id: customer,
            line_items: vec![DispatchLineItemPayload {
                stock_description: "Fleet Goods".to_string(),
                requested_quantity: 1,
            }],
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let preview = test::read_body_json::<ApiResponse<VehiclePreview>, _>(resp)
        .await
        .data
        .unwrap();
    // Parked at the org, next to the customer, not at 0°N 0°E.
    assert_eq!(preview.suggested.registration_number, "OL-NOFIX");
    assert!(
        preview.suggested.distance_km < 1.0,
        "{}",
        preview.suggested.distance_km
    );
}

#[actix_web::test]
async fn test_two_simultaneous_dispatches_on_one_vehicle_only_one_succeeds() {
    use crate::logistics::db::connection::set_test_db_override;
    use crate::logistics::dispatch::dispatch::DispatchLineItemInput;
    use std::sync::{Arc, Barrier};

    let db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, _, customer) = setup_fleet(&app, "Override Race", &[("RC-A", 1000, true)]).await;
    let org = Organization::get_by_id(org.id).unwrap().unwrap();
    let customer = Customer::get_by_id(customer).unwrap().unwrap();
    let line_items = vec![DispatchLineItemInput {
        stock_description: "Fleet Goods".to_string(),
        requested_quantity: 1,
    }];

    let barrier = Arc::new(Barrier::new(2));
    let results: Vec<bool> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let (org, customer, line_items) = (&org, &customer, &line_items);
                let name = db.name().to_string();
                s.spawn(move || {
                    set_test_db_override(Some(name));
                    barrier.wait();
                    let ok = org
                        .dispatch_stock_on_vehicle(customer, line_items, "RC-A")
                        .is_ok();
                    set_test_db_override(None);
                    ok
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(results.iter().filter(|ok| **ok).count(), 1, "{results:?}");
}

/// An own-fleet dispatch and a hired one drawing from the same godown at the
/// same time must both go through. The own-fleet claim used to lock every
/// `Dispatches` row before drawing stock, while the hired path draws stock
/// and then inserts, so the two could deadlock (MySQL error 1213).
#[actix_web::test]
async fn test_simultaneous_own_fleet_and_hired_dispatches_both_succeed() {
    use crate::logistics::db::connection::set_test_db_override;
    use crate::logistics::dispatch::dispatch::DispatchLineItemInput;
    use std::sync::{Arc, Barrier};

    const ROUNDS: usize = 4;
    let db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth, customer) = setup_fleet(
        &app,
        "Mixed Race",
        &[
            ("MX-A", 1000, true),
            ("MX-B", 1000, true),
            ("MX-C", 1000, true),
            ("MX-D", 1000, true),
        ],
    )
    .await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/vendors", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(vendor_payload("Mixed Race Vendor"))
        .to_request();
    let vendor: ApiResponse<VehicleVendor> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let vendor_id = vendor.data.expect("vendor").id;
    let org = Organization::get_by_id(org.id).unwrap().unwrap();
    let customer = Customer::get_by_id(customer).unwrap().unwrap();
    let line_items = vec![DispatchLineItemInput {
        stock_description: "Fleet Goods".to_string(),
        requested_quantity: 1,
    }];

    // Each round puts one more own vehicle on the road, so there are as many
    // vehicles as rounds.
    for round in 0..ROUNDS {
        let barrier = Arc::new(Barrier::new(2));
        let results: Vec<Result<(), String>> = std::thread::scope(|s| {
            let handles: Vec<_> = [false, true]
                .into_iter()
                .map(|hired| {
                    let barrier = barrier.clone();
                    let (org, customer, line_items) = (&org, &customer, &line_items);
                    let name = db.name().to_string();
                    s.spawn(move || {
                        set_test_db_override(Some(name));
                        barrier.wait();
                        let result = if hired {
                            org.dispatch_stock_on_hired_vehicle(customer, line_items, vendor_id)
                        } else {
                            org.dispatch_stock_to_customer(customer, line_items)
                        };
                        set_test_db_override(None);
                        result.map(|_| ()).map_err(|e| e.to_string())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(
            results.iter().all(Result::is_ok),
            "round {round}: {results:?}"
        );
    }
}
