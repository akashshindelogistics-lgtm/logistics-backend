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
