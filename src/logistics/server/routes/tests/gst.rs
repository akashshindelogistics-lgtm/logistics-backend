use super::*;
use crate::logistics::dispatch::dispatch::ProofOfDeliveryInput;
use crate::logistics::stock::category::StockCategory;

fn bearer(auth: &str) -> (&'static str, String) {
    ("Authorization", auth.to_string())
}

#[actix_web::test]
async fn test_org_gstin_is_set_validated_kept_and_cleared() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Gst Org").await;
    let put = |gstin: Option<&str>| {
        test::TestRequest::put()
            .uri(&format!("/api/orgs/{}", org.id))
            .insert_header(bearer(&auth))
            .set_json(&UpdateOrgPayload {
                name: "Gst Org".into(),
                address: "Addr".into(),
                gstin: gstin.map(str::to_string),
            })
            .to_request()
    };

    let resp = test::call_service(&app, put(Some("27aapfu0939f1zv"))).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<Organization> = test::read_body_json(resp).await;
    assert_eq!(body.data.unwrap().gstin.as_deref(), Some("27AAPFU0939F1ZV"));
    assert_eq!(
        Organization::get_by_id(org.id)
            .unwrap()
            .unwrap()
            .gstin
            .as_deref(),
        Some("27AAPFU0939F1ZV")
    );

    // Malformed is a 400 and changes nothing.
    let resp = test::call_service(&app, put(Some("BAD"))).await;
    assert_eq!(resp.status().as_u16(), 400);

    // Omitted keeps it, and the response still shows it.
    let resp = test::call_service(&app, put(None)).await;
    let body: ApiResponse<Organization> = test::read_body_json(resp).await;
    assert_eq!(body.data.unwrap().gstin.as_deref(), Some("27AAPFU0939F1ZV"));

    // Empty clears it.
    let resp = test::call_service(&app, put(Some(""))).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        Organization::get_by_id(org.id).unwrap().unwrap().gstin,
        None
    );
}

#[actix_web::test]
async fn test_customer_gstin_on_create_and_update() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Cust Gst Org").await;
    let create = |gstin: Option<&str>| {
        test::TestRequest::post()
            .uri(&format!("/api/orgs/{}/customers", org.id))
            .insert_header(bearer(&auth))
            .set_json(&CreateCustomerPayload {
                name: "Buyer".into(),
                address: "x".into(),
                latitude: None,
                longitude: None,
                location_address: None,
                phone: None,
                email: None,
                gstin: gstin.map(str::to_string),
            })
            .to_request()
    };

    let resp = test::call_service(&app, create(Some("short"))).await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(Customer::list_by_org(org.id).unwrap().is_empty());

    let resp = test::call_service(&app, create(Some("29abcde1234f1z5"))).await;
    assert_eq!(resp.status().as_u16(), 201);
    let customer = test::read_body_json::<ApiResponse<Customer>, _>(resp)
        .await
        .data
        .unwrap();
    assert_eq!(customer.gstin.as_deref(), Some("29ABCDE1234F1Z5"));
    let listed = Customer::list_by_org(org.id).unwrap();
    assert_eq!(listed[0].gstin.as_deref(), Some("29ABCDE1234F1Z5"));

    let update = |gstin: Option<&str>| {
        test::TestRequest::put()
            .uri(&format!("/api/customers/{}/gstin", customer.id))
            .insert_header(bearer(&auth))
            .set_json(&CustomerGstinPayload {
                gstin: gstin.map(str::to_string),
            })
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, update(Some("nope")))
            .await
            .status()
            .as_u16(),
        400
    );
    let resp = test::call_service(&app, update(Some("27AAPFU0939F1ZV"))).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        Customer::get_by_id(customer.id)
            .unwrap()
            .unwrap()
            .gstin
            .as_deref(),
        Some("27AAPFU0939F1ZV")
    );
    let resp = test::call_service(&app, update(None)).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        Customer::get_by_id(customer.id).unwrap().unwrap().gstin,
        None
    );

    // Another org's token can't touch it.
    let req = test::TestRequest::put()
        .uri(&format!("/api/customers/{}/gstin", customer.id))
        .insert_header(("Authorization", make_auth_header(Uuid::new_v4(), "Other")))
        .set_json(&CustomerGstinPayload { gstin: None })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
}

#[actix_web::test]
async fn test_stock_category_hsn_crud_is_per_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Hsn Org").await;
    let (_other, other_auth) = setup_org(&app, "Other Hsn Org").await;
    let set = |auth: &str, name: &str, hsn: &str| {
        test::TestRequest::put()
            .uri(&format!("/api/stock-categories/{name}/hsn"))
            .insert_header(bearer(auth))
            .set_json(&SetHsnPayload {
                hsn_code: hsn.into(),
            })
            .to_request()
    };

    assert_eq!(
        test::call_service(&app, set(&auth, "Cement", "abc"))
            .await
            .status()
            .as_u16(),
        400
    );
    let resp = test::call_service(&app, set(&auth, "Cement", "2523")).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<StockCategory> = test::read_body_json(resp).await;
    assert_eq!(body.data.unwrap().hsn_code, "2523");

    let list = |auth: &str| {
        test::TestRequest::get()
            .uri("/api/stock-categories")
            .insert_header(bearer(auth))
            .to_request()
    };
    let body: ApiResponse<Vec<StockCategory>> =
        test::read_body_json(test::call_service(&app, list(&auth)).await).await;
    let cats = body.data.unwrap();
    assert_eq!((cats.len(), cats[0].org_id), (1, org.id));
    let body: ApiResponse<Vec<StockCategory>> =
        test::read_body_json(test::call_service(&app, list(&other_auth)).await).await;
    assert!(body.data.unwrap().is_empty());

    let del = |auth: &str| {
        test::TestRequest::delete()
            .uri("/api/stock-categories/Cement/hsn")
            .insert_header(bearer(auth))
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, del(&other_auth))
            .await
            .status()
            .as_u16(),
        404
    );
    assert_eq!(
        test::call_service(&app, del(&auth)).await.status().as_u16(),
        200
    );
    assert_eq!(
        test::call_service(&app, del(&auth)).await.status().as_u16(),
        404
    );
}

#[actix_web::test]
async fn test_dispatch_eway_bill_set_validated_cleared_and_locked_once_delivered() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (_org, dispatch, auth) = setup_dispatch(&app, "Eway Org").await;
    assert_eq!(dispatch.eway_bill_no, None);
    let put = |n: Option<&str>| {
        test::TestRequest::put()
            .uri(&format!("/api/dispatches/{}/eway-bill", dispatch.id))
            .insert_header(bearer(&auth))
            .set_json(&UpdateEwayBillPayload {
                eway_bill_no: n.map(str::to_string),
            })
            .to_request()
    };

    for bad in ["12345", "12345678901a", "1234567890123"] {
        assert_eq!(
            test::call_service(&app, put(Some(bad)))
                .await
                .status()
                .as_u16(),
            400
        );
    }
    let resp = test::call_service(&app, put(Some(" 123456789012 "))).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<DispatchOrder> = test::read_body_json(resp).await;
    assert_eq!(
        body.data.unwrap().eway_bill_no.as_deref(),
        Some("123456789012")
    );
    assert_eq!(
        DispatchOrder::get_by_id(dispatch.id)
            .unwrap()
            .unwrap()
            .eway_bill_no
            .as_deref(),
        Some("123456789012")
    );
    // It also shows up in the dispatch list.
    let req = test::TestRequest::get()
        .uri("/api/dispatches")
        .insert_header(bearer(&auth))
        .to_request();
    let body: ApiResponse<Vec<DispatchOrder>> =
        test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(
        body.data.unwrap()[0].eway_bill_no.as_deref(),
        Some("123456789012")
    );

    // Another org can't edit it.
    let req = test::TestRequest::put()
        .uri(&format!("/api/dispatches/{}/eway-bill", dispatch.id))
        .insert_header(("Authorization", make_auth_header(Uuid::new_v4(), "Other")))
        .set_json(&UpdateEwayBillPayload { eway_bill_no: None })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);

    // Delivered: locked.
    let mut live = DispatchOrder::get_by_id(dispatch.id).unwrap().unwrap();
    for next in [
        DispatchStatus::Confirmed,
        DispatchStatus::Loaded,
        DispatchStatus::InTransit,
    ] {
        live.transition_to(next, None, None).unwrap();
    }
    live.transition_to(
        DispatchStatus::Delivered,
        Some(ProofOfDeliveryInput {
            receiver_name: "R".into(),
            signature_or_photo_url: "u".into(),
        }),
        None,
    )
    .unwrap();
    assert_eq!(
        test::call_service(&app, put(None)).await.status().as_u16(),
        400
    );
    assert_eq!(
        DispatchOrder::get_by_id(dispatch.id)
            .unwrap()
            .unwrap()
            .eway_bill_no
            .as_deref(),
        Some("123456789012")
    );
}
