use super::*;

#[actix_web::test]
async fn test_get_ops_report_summary_without_token_returns_401() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/reports/summary", Uuid::new_v4()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
}

#[actix_web::test]
async fn test_get_ops_report_summary_returns_403_for_a_different_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, _auth) = setup_org(&app, "Report Summary Owner Org").await;
    let (_other, other_auth) = setup_org(&app, "Report Summary Intruder Org").await;

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/reports/summary", org.id))
        .insert_header(("Authorization", other_auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
}

#[actix_web::test]
async fn test_ops_report_reflects_a_dispatch_and_its_vehicle() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, auth) = setup_dispatch(&app, "Report Org").await;

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/reports", org.id))
        .insert_header(("Authorization", auth))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let report = test::read_body_json::<ApiResponse<OpsReport>, _>(resp)
        .await
        .data
        .unwrap();

    // The one dispatch is PENDING (non-terminal) so its vehicle is "on trip".
    assert_eq!(report.vehicle_utilization.vehicles_on_active_trip, 1);
    assert!(report.vehicle_utilization.total_vehicles >= 1);
    assert_eq!(report.delivery_performance.delivered_count, 0);
    assert_eq!(report.dispatch_volume.len(), 14);
    assert_eq!(report.dispatch_volume[13].count, 1, "one dispatch today");
    assert!(report.units_dispatched_recently >= 1);
    let _ = dispatch;
}

#[actix_web::test]
async fn test_ops_report_403_for_a_different_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, _auth) = setup_org(&app, "Report Owner Org").await;
    let (_other, other_auth) = setup_org(&app, "Report Intruder Org").await;

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/reports", org.id))
        .insert_header(("Authorization", other_auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
}

#[actix_web::test]
async fn test_export_csv_returns_org_data_with_header() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, auth) = setup_dispatch(&app, "Export Org").await;

    for (dataset, header) in [
        ("dispatches", "id,customer_id,status,"),
        ("stock", "godown_id,godown_name,"),
        ("invoices", "id,dispatch_id,"),
        ("customers", "id,name,address,phone,email"),
    ] {
        let req = test::TestRequest::get()
            .uri(&format!("/api/orgs/{}/export/{}", org.id, dataset))
            .insert_header(("Authorization", auth.clone()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "{dataset}");
        assert!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/csv")
        );
        let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
        assert!(body.starts_with(header), "{dataset}: {body}");
        if dataset == "dispatches" {
            assert!(body.contains(&dispatch.id.to_string()));
        }
    }
}

#[actix_web::test]
async fn test_export_tally_xml() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, auth) = setup_dispatch(&app, "Tally Org").await;
    for dataset in ["tally-invoices", "tally-stock"] {
        let req = test::TestRequest::get()
            .uri(&format!("/api/orgs/{}/export/{}", org.id, dataset))
            .insert_header(("Authorization", auth.clone()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "{dataset}");
        let headers = resp.headers().clone();
        assert!(
            headers
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("application/xml")
        );
        assert!(
            headers
                .get("content-disposition")
                .unwrap()
                .to_str()
                .unwrap()
                .contains(&format!("{dataset}.xml"))
        );
        let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
        assert!(body.starts_with("<ENVELOPE>"), "{dataset}: {body}");
        if dataset == "tally-stock" {
            // A freshly created dispatch has not left yet, so is not exported.
            assert!(!body.contains(&dispatch.id.to_string()), "{body}");
        }
    }
}

#[actix_web::test]
async fn test_export_csv_403_other_org_404_unknown_dataset_401_no_token() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Export Owner Org").await;
    let (_other, other_auth) = setup_org(&app, "Export Intruder Org").await;

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/export/customers", org.id))
        .insert_header(("Authorization", other_auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/export/users", org.id))
        .insert_header(("Authorization", auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 404);

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/export/customers", org.id))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
}
