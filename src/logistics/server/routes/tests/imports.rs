use super::*;
use crate::logistics::import::csv_import::{ImportKind, ImportReport};

fn import_request(
    org_id: Uuid,
    auth: &str,
    kind: &str,
    dry_run: bool,
    csv: &str,
) -> actix_http::Request {
    let boundary = "ImportBoundary42";
    test::TestRequest::post()
        .uri(&format!(
            "/api/orgs/{org_id}/import/{kind}?dry_run={dry_run}"
        ))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .insert_header(("Authorization", auth.to_string()))
        .set_payload(multipart_body(
            boundary,
            "data.csv",
            "text/csv",
            csv.as_bytes(),
        ))
        .to_request()
}

async fn report_of(resp: actix_web::dev::ServiceResponse) -> ImportReport {
    test::read_body_json::<ApiResponse<ImportReport>, _>(resp)
        .await
        .data
        .unwrap()
}

#[actix_web::test]
async fn test_templates_download_and_import_cleanly() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Import Template Org").await;

    for kind in ["vehicles", "drivers", "customers", "stock"] {
        let req = test::TestRequest::get()
            .uri(&format!("/api/orgs/{}/import/{kind}/template", org.id))
            .insert_header(("Authorization", auth.clone()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "{kind}");
        let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
        assert_eq!(body, ImportKind::parse(kind).unwrap().template());

        let resp =
            test::call_service(&app, import_request(org.id, &auth, kind, false, &body)).await;
        assert_eq!(resp.status().as_u16(), 200, "{kind}");
        let report = report_of(resp).await;
        assert!(report.errors.is_empty());
        assert_eq!(report.total_rows, 0);
    }

    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/import/bogus/template", org.id))
        .insert_header(("Authorization", auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 404);
}

#[actix_web::test]
async fn test_import_requires_admin_and_own_org() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Import Auth Org").await;
    let (_other, other_auth) = setup_org(&app, "Import Intruder Org").await;
    let dispatcher = add_user_and_login(
        &app,
        org.id,
        &auth,
        "import-dispatcher@example.com",
        OrgRole::Dispatcher,
    )
    .await;
    let csv = "name,license_number,phone\nA,L1,1\n";

    let resp = test::call_service(
        &app,
        import_request(org.id, &dispatcher, "drivers", false, csv),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 403);
    let resp = test::call_service(
        &app,
        import_request(org.id, &other_auth, "drivers", false, csv),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 403);
    let resp = test::call_service(&app, import_request(org.id, &auth, "nope", false, csv)).await;
    assert_eq!(resp.status().as_u16(), 404);
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/import/drivers", org.id))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
}

#[actix_web::test]
async fn test_drivers_dry_run_reports_bad_rows_by_line_and_writes_nothing() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Import Driver Org").await;
    let csv = "name,license_number,phone\r\nAsha,DL-1,+91 1\r\n,DL-2,2\r\nRavi,DL-1,3\r\n";

    let resp = test::call_service(&app, import_request(org.id, &auth, "drivers", true, csv)).await;
    assert_eq!(resp.status().as_u16(), 200);
    let report = report_of(resp).await;
    assert!(report.dry_run);
    assert_eq!(report.total_rows, 3);
    assert_eq!(report.valid_rows, 1);
    assert_eq!(report.imported, 0);
    let lines: Vec<(usize, Option<&str>)> = report
        .errors
        .iter()
        .map(|e| (e.line, e.column.as_deref()))
        .collect();
    assert_eq!(lines, vec![(3, Some("name")), (4, Some("license_number"))]);

    // A real import of the same file is rejected whole: 422, nothing written.
    let resp = test::call_service(&app, import_request(org.id, &auth, "drivers", false, csv)).await;
    assert_eq!(resp.status().as_u16(), 422);
    let req = test::TestRequest::get()
        .uri(&format!("/api/drivers"))
        .insert_header(("Authorization", auth))
        .to_request();
    let body: ApiResponse<Vec<Driver>> =
        test::read_body_json(test::call_service(&app, req).await).await;
    assert!(body.data.unwrap().is_empty());
}

#[actix_web::test]
async fn test_import_drivers_vehicles_customers_and_stock() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Import Full Org").await;

    let csv = "name,license_number,phone\nAsha,DL-1,+91 1\nRavi,DL-2,2\n";
    let resp = test::call_service(&app, import_request(org.id, &auth, "drivers", false, csv)).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(report_of(resp).await.imported, 2);

    // Vehicle 2 references an unknown licence, vehicle 3 repeats a reg number.
    let bad = "registration_number,capacity,unit,vehicle_type,driver_license_number\n\
               IMP-1,10,Kg,Tipper,DL-1\nIMP-2,5,,,DL-9\nimp-1,5,,,\nIMP-4,x,Bogus,,\n";
    let resp = test::call_service(&app, import_request(org.id, &auth, "vehicles", true, bad)).await;
    let report = report_of(resp).await;
    assert_eq!(report.valid_rows, 1);
    let error_lines: Vec<usize> = report.errors.iter().map(|e| e.line).collect();
    assert_eq!(error_lines, vec![3, 4, 5, 5]);

    let good = "registration_number,capacity,unit,vehicle_type,driver_license_number\n\
                IMP-1,10,Kg,Tipper,DL-1\nIMP-2,5,,,\n";
    let resp =
        test::call_service(&app, import_request(org.id, &auth, "vehicles", false, good)).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(report_of(resp).await.imported, 2);
    // Re-importing the same file is refused: the vehicles already exist.
    let resp =
        test::call_service(&app, import_request(org.id, &auth, "vehicles", false, good)).await;
    assert_eq!(resp.status().as_u16(), 422);

    let req = test::TestRequest::get()
        .uri(&format!("/api/vehicles"))
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    let body: ApiResponse<Vec<Vehicle>> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let vehicles = body.data.unwrap();
    assert_eq!(vehicles.len(), 2);
    let v1 = vehicles
        .iter()
        .find(|v| v.registration_number == "IMP-1")
        .unwrap();
    assert!(v1.assigned_driver_id.is_some());
    assert_eq!(v1.vehicle_type, VehicleType::Tipper);
    assert_eq!(v1.unit, Unit::Kg);

    let csv = "name,address,phone,email,gstin\n\
               Acme,\"1 Main St, Pune\",'+91 22,a@b.com,27AAPFU0939F1ZV\nBeta,2 Side St,,,\n";
    let resp =
        test::call_service(&app, import_request(org.id, &auth, "customers", false, csv)).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(report_of(resp).await.imported, 2);
    let req = test::TestRequest::get()
        .uri(&format!("/api/customers"))
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    let body: ApiResponse<Vec<Customer>> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let customers = body.data.unwrap();
    let acme = customers.iter().find(|c| c.name == "Acme").unwrap();
    assert_eq!(acme.address, "1 Main St, Pune");
    assert_eq!(acme.phone.as_deref(), Some("+91 22"));
    assert_eq!(acme.gstin.as_deref(), Some("27AAPFU0939F1ZV"));

    // Stock: needs a godown by name; capacity is enforced across the file.
    let req = test::TestRequest::post()
        .uri(&format!("/api/orgs/{}/godowns", org.id))
        .insert_header(("Authorization", auth.clone()))
        .set_json(&CreateGodownPayload {
            name: "Main Godown".to_string(),
            address: "1 Yard".to_string(),
            max_capacity: Some(100),
        })
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);
    let header = "godown_name,description,category,quantity,volume_in_size,reorder_threshold\n";
    let bad = format!(
        "{header}Nowhere,Rice,,1,1,\nmain godown,Rice,Food,10,5,20\nMain Godown,Wheat,,10,6,\n"
    );
    let resp = test::call_service(&app, import_request(org.id, &auth, "stock", true, &bad)).await;
    let report = report_of(resp).await;
    let lines: Vec<(usize, Option<&str>)> = report
        .errors
        .iter()
        .map(|e| (e.line, e.column.as_deref()))
        .collect();
    assert_eq!(lines, vec![(2, Some("godown_name")), (4, Some("quantity"))]);

    let good = format!("{header}main godown,Rice,Food,10,5,20\n");
    let resp = test::call_service(&app, import_request(org.id, &auth, "stock", false, &good)).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(report_of(resp).await.imported, 1);
    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/godowns", org.id))
        .insert_header(("Authorization", auth))
        .to_request();
    let body: ApiResponse<Vec<Godown>> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let stock = &body.data.unwrap()[0].stock;
    assert_eq!(stock.len(), 1);
    assert_eq!(stock[0].category, "Food");
    assert_eq!(stock[0].reorder_threshold, Some(20));
}

#[actix_web::test]
async fn test_import_rejects_wrong_header_and_too_many_rows() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Import File Org").await;

    let resp = test::call_service(
        &app,
        import_request(org.id, &auth, "drivers", true, "name,phone\nA,1\n"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);

    let mut csv = String::from("name,license_number,phone\n");
    for i in 0..1001 {
        csv.push_str(&format!("D{i},L{i},1\n"));
    }
    let resp = test::call_service(&app, import_request(org.id, &auth, "drivers", true, &csv)).await;
    assert_eq!(resp.status().as_u16(), 400);
}
