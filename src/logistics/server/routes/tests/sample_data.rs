use super::*;

fn count(table: &str, org_id: Uuid) -> i64 {
    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    use mysql::prelude::*;
    conn.exec_first(
        format!("SELECT COUNT(*) FROM {table} WHERE org_id = ?"),
        (org_id.to_string(),),
    )
    .unwrap()
    .unwrap()
}

fn stock_rows() -> i64 {
    use mysql::prelude::*;
    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    conn.query_first("SELECT COUNT(*) FROM Stock")
        .unwrap()
        .unwrap()
}

#[actix_web::test]
async fn test_sample_data_loads_dispatches_and_removes_cleanly() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);
    let call = |req: test::TestRequest| req.insert_header(("Authorization", auth.clone()));

    let resp =
        test::call_service(&app, call(test::TestRequest::post().uri(&uri)).to_request()).await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: ApiResponse<SampleDataSummary> = test::read_body_json(resp).await;
    let summary = body.data.unwrap();
    assert_eq!(
        (
            summary.godowns,
            summary.vehicles,
            summary.drivers,
            summary.customers
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(stock_rows(), 2);

    // A second load is a conflict and adds nothing.
    let resp =
        test::call_service(&app, call(test::TestRequest::post().uri(&uri)).to_request()).await;
    assert_eq!(resp.status().as_u16(), 409);
    assert_eq!(count("Godowns", org.id), 1);

    let resp =
        test::call_service(&app, call(test::TestRequest::get().uri(&uri)).to_request()).await;
    let body: ApiResponse<SampleDataStatus> = test::read_body_json(resp).await;
    assert!(body.data.unwrap().loaded);

    // The sample data is enough to run a dispatch.
    let customer = Customer::list_by_org(org.id).unwrap().remove(0);
    let org_full = Organization::get_by_id(org.id).unwrap().unwrap();
    org_full
        .dispatch_stock_to_customer(
            &customer,
            &[DispatchLineItemInput {
                stock_description: "Sample Rice Bags".into(),
                requested_quantity: 10,
            }],
        )
        .expect("dispatch against sample data");
    assert_eq!(count("Dispatches", org.id), 1);

    // A real record next to it must survive removal.
    let real = Customer::create_customer(org.id, "Real Customer", "Somewhere").unwrap();

    let resp = test::call_service(
        &app,
        call(test::TestRequest::delete().uri(&uri)).to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: ApiResponse<SampleDataSummary> = test::read_body_json(resp).await;
    assert_eq!(body.data.unwrap().dispatches, 1);

    for table in ["Godowns", "Vehicle", "Drivers", "Dispatches", "SampleData"] {
        assert_eq!(count(table, org.id), 0, "{table} left rows behind");
    }
    assert_eq!(stock_rows(), 0);
    let left = Customer::list_by_org(org.id).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, real.id);

    // Removing again is a harmless no-op, and it can be loaded again.
    let resp = test::call_service(
        &app,
        call(test::TestRequest::delete().uri(&uri)).to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let resp =
        test::call_service(&app, call(test::TestRequest::post().uri(&uri)).to_request()).await;
    assert_eq!(resp.status().as_u16(), 201);
}

#[actix_web::test]
async fn test_sample_data_is_admin_only_and_org_scoped() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, admin) = setup_org(&app, "Sample Roles Org").await;
    let (other, other_auth) = setup_org(&app, "Other Sample Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);

    let dispatcher =
        add_user_and_login(&app, org.id, &admin, "d@example.com", OrgRole::Dispatcher).await;
    for (method, auth) in [("post", &dispatcher), ("delete", &dispatcher)] {
        let req = match method {
            "post" => test::TestRequest::post(),
            _ => test::TestRequest::delete(),
        }
        .uri(&uri)
        .insert_header(("Authorization", auth.clone()))
        .to_request();
        assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
    }

    // Another org's admin cannot touch this org.
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("Authorization", other_auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
    assert_eq!(count("Godowns", org.id) + count("Godowns", other.id), 0);
}

fn exec(sql: &str, args: Vec<mysql::Value>) {
    use mysql::prelude::*;
    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    conn.exec_drop(sql, args).unwrap();
}

fn sample_ids(org_id: Uuid, kind: &str) -> Vec<String> {
    use mysql::prelude::*;
    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    conn.exec(
        "SELECT ref_id FROM SampleData WHERE org_id = ? AND kind = ?",
        (org_id.to_string(), kind),
    )
    .unwrap()
}

/// Per-table row counts for the org, to show a refused removal deleted nothing.
fn snapshot(org_id: Uuid) -> Vec<i64> {
    let mut counts: Vec<i64> = [
        "Godowns",
        "Vehicle",
        "Drivers",
        "Customers",
        "Dispatches",
        "SampleData",
    ]
    .iter()
    .map(|t| count(t, org_id))
    .collect();
    counts.push(stock_rows());
    counts
}

async fn delete_status(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    auth: &str,
) -> u16 {
    let req = test::TestRequest::delete()
        .uri(uri)
        .insert_header(("Authorization", auth.to_string()))
        .to_request();
    test::call_service(app, req).await.status().as_u16()
}

#[actix_web::test]
async fn test_sample_data_remove_refuses_when_real_dispatch_uses_sample_vehicle() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Vehicle Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);

    // A real customer's dispatch goes out on the sample vehicle.
    let mut real = Customer::create_customer(org.id, "Real Customer", "Somewhere").unwrap();
    real.update_location(18.52, 73.86, Some("Somewhere"))
        .expect("location");
    Organization::get_by_id(org.id)
        .unwrap()
        .unwrap()
        .dispatch_stock_to_customer(
            &real,
            &[DispatchLineItemInput {
                stock_description: "Sample Rice Bags".into(),
                requested_quantity: 10,
            }],
        )
        .expect("dispatch on the sample vehicle");

    let before = snapshot(org.id);
    assert_eq!(delete_status(&app, &uri, &auth).await, 409);
    assert_eq!(snapshot(org.id), before, "a refused removal deleted rows");
}

#[actix_web::test]
async fn test_sample_data_remove_refuses_after_stock_transfer() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Transfer Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);

    let real = Godown::create(org.id, "Real Godown", "Somewhere", None).unwrap();
    let sample = sample_ids(org.id, "godown").remove(0);
    exec(
        "INSERT INTO StockTransfers
         (id, org_id, from_godown_id, to_godown_id, description, quantity, volume_in_size, transferred_at)
         VALUES (?, ?, ?, ?, 'Sample Rice Bags', 5, 10, 0)",
        vec![
            Uuid::new_v4().to_string().into(),
            org.id.to_string().into(),
            sample.into(),
            real.id.to_string().into(),
        ],
    );

    let before = snapshot(org.id);
    assert_eq!(delete_status(&app, &uri, &auth).await, 409);
    assert_eq!(snapshot(org.id), before, "a refused removal deleted rows");
}

#[actix_web::test]
async fn test_sample_data_remove_refuses_sample_customer_dispatch_with_real_data() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Real Stock Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);

    // Real stock from a real godown, dispatched to the sample customer.
    let godown = Godown::create(org.id, "Real Godown", "Somewhere", None).unwrap();
    Stock::new(5, 100, "Cement")
        .add_to_godown(godown.id)
        .unwrap();
    let customer = Customer::list_by_org(org.id).unwrap().remove(0);
    let dispatch = Organization::get_by_id(org.id)
        .unwrap()
        .unwrap()
        .dispatch_stock_to_customer(
            &customer,
            &[DispatchLineItemInput {
                stock_description: "Cement".into(),
                requested_quantity: 10,
            }],
        )
        .expect("dispatch real stock to the sample customer");
    let id = || -> mysql::Value { dispatch.id.to_string().into() };

    let before = snapshot(org.id);
    assert_eq!(delete_status(&app, &uri, &auth).await, 409);
    assert_eq!(snapshot(org.id), before, "a refused removal deleted rows");

    // Make it sample stock only; an invoice still blocks removal.
    exec(
        "UPDATE DispatchLineItems SET stock_description = 'Sample Rice Bags' WHERE dispatch_id = ?",
        vec![id()],
    );
    exec(
        "INSERT INTO Invoices (id, org_id, dispatch_id, customer_id, amount, issued_on, due_on)
         VALUES (?, ?, ?, ?, 100, '2026-01-01', '2026-01-31')",
        vec![
            Uuid::new_v4().to_string().into(),
            org.id.to_string().into(),
            id(),
            customer.id.to_string().into(),
        ],
    );
    assert_eq!(delete_status(&app, &uri, &auth).await, 409);
    assert_eq!(snapshot(org.id), before, "a refused removal deleted rows");
    assert_eq!(count("Invoices", org.id), 1);
    exec("DELETE FROM Invoices WHERE dispatch_id = ?", vec![id()]);

    // So does a vehicle hire.
    exec(
        "UPDATE Dispatches SET hire_id = ? WHERE id = ?",
        vec![Uuid::new_v4().to_string().into(), id()],
    );
    assert_eq!(delete_status(&app, &uri, &auth).await, 409);
    assert_eq!(snapshot(org.id), before, "a refused removal deleted rows");
    exec(
        "UPDATE Dispatches SET hire_id = NULL WHERE id = ?",
        vec![id()],
    );

    // Once it is sample data through and through it goes, and the real
    // godown and its stock stay.
    assert_eq!(delete_status(&app, &uri, &auth).await, 200);
    assert_eq!(count("Dispatches", org.id), 0);
    assert_eq!(count("Godowns", org.id), 1);
    assert_eq!(stock_rows(), 1);
}

#[actix_web::test]
async fn test_sample_data_remove_unassigns_sample_driver_from_real_vehicle() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Driver Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);

    let driver: Uuid = sample_ids(org.id, "driver").remove(0).parse().unwrap();
    let mut truck = Vehicle::new("REAL-TRUCK-1", 1000, Unit::MetricTon);
    truck
        .add_new_vehicle_to_org(&Organization::get_by_id(org.id).unwrap().unwrap())
        .unwrap();
    truck.assign_driver(Some(driver)).unwrap();

    assert_eq!(delete_status(&app, &uri, &auth).await, 200);
    assert_eq!(count("Drivers", org.id), 0);
    let left = Vehicle::list_by_org(org.id).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].registration_number, "REAL-TRUCK-1");
    assert_eq!(left[0].assigned_driver_id, None, "dangling driver id");
}

#[actix_web::test]
async fn test_sample_data_failed_load_rolls_back_only_its_own_rows() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Race Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);
    let post = || {
        test::TestRequest::post()
            .uri(&uri)
            .insert_header(("Authorization", auth.clone()))
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, post()).await.status().as_u16(),
        201
    );
    let first = snapshot(org.id);

    // Stand in for a second request of a double click that got past the
    // guard: without the marker it starts creating rows, then fails on the
    // sample vehicle the first request already tagged. Its clean-up must not
    // touch the first request's rows.
    exec(
        "DELETE FROM SampleData WHERE org_id = ? AND kind = 'loaded'",
        vec![org.id.to_string().into()],
    );
    assert_eq!(
        test::call_service(&app, post()).await.status().as_u16(),
        500
    );
    for (table, n) in [
        ("Godowns", 1),
        ("Vehicle", 1),
        ("Drivers", 1),
        ("Customers", 1),
        ("SampleData", 4),
    ] {
        assert_eq!(count(table, org.id), n, "{table}");
    }
    assert_eq!(stock_rows(), first[6]);
    let vehicles = Vehicle::list_by_org(org.id).unwrap();
    assert!(vehicles[0].assigned_driver_id.is_some());
}

#[actix_web::test]
async fn test_sample_data_marker_blocks_a_second_load() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Marker Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);

    // A load in progress holds only the marker so far; another load is a
    // conflict and creates nothing.
    exec(
        "INSERT INTO SampleData (org_id, kind, ref_id) VALUES (?, 'loaded', '')",
        vec![org.id.to_string().into()],
    );
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 409);
    assert_eq!(count("Godowns", org.id), 0);
    assert_eq!(count("Drivers", org.id), 0);
}

#[actix_web::test]
async fn test_sample_data_remove_clears_pod_photo_flags_of_its_dispatches() {
    use crate::logistics::dispatch::pod_review;
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Sample Pod Flags Org").await;
    let uri = format!("/api/orgs/{}/sample-data", org.id);
    let req = test::TestRequest::post()
        .uri(&uri)
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 201);

    let customer = Customer::list_by_org(org.id).unwrap().remove(0);
    let dispatch = Organization::get_by_id(org.id)
        .unwrap()
        .unwrap()
        .dispatch_stock_to_customer(
            &customer,
            &[DispatchLineItemInput {
                stock_description: "Sample Rice Bags".into(),
                requested_quantity: 10,
            }],
        )
        .expect("dispatch against sample data");

    // The sample dispatch's photo was flagged, and so was a real one's.
    let real_dispatch = Uuid::new_v4();
    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    pod_review::ensure_table(&mut conn).expect("table");
    for id in [dispatch.id, real_dispatch] {
        exec(
            "INSERT INTO PodPhotoChecks (dispatch_id, org_id, verdict, reason, checked_at)
             VALUES (?, ?, 'DOUBTFUL', 'x', 1)",
            vec![id.to_string().into(), org.id.to_string().into()],
        );
    }
    assert_eq!(pod_review::list_flagged(org.id).unwrap().len(), 2);

    assert_eq!(delete_status(&app, &uri, &auth).await, 200);
    let left = pod_review::list_flagged(org.id).unwrap();
    assert_eq!(left.len(), 1, "a deleted dispatch is still flagged");
    assert_eq!(left[0].dispatch_id, real_dispatch);
}
