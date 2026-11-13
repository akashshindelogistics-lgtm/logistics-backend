use super::*;

fn expense_payload(trip_id: Option<Uuid>, amount: i64) -> VehicleExpensePayload {
    VehicleExpensePayload {
        expense_type: ExpenseType::Fuel,
        amount,
        incurred_on: iso_date_offset(0),
        trip_id,
        litres: Some(40.5),
        notes: None,
    }
}

#[actix_web::test]
async fn test_vehicle_expense_lifecycle_and_report() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, dispatch, auth) = setup_dispatch(&app, "Expense Co").await;
    let reg = "DISP-VH-001";

    // Tag a fuel fill to the standalone dispatch's one-stop trip.
    let req = test::TestRequest::post()
        .uri(&format!("/api/vehicles/{reg}/expenses"))
        .insert_header(("Authorization", auth.clone()))
        .set_json(expense_payload(Some(dispatch.id), 4_200))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 201);
    let created: ApiResponse<VehicleExpense> = test::read_body_json(resp).await;
    let expense = created.data.unwrap();
    assert_eq!(expense.litres, Some(40.5));

    for uri in [
        format!("/api/vehicles/{reg}/expenses"),
        format!("/api/orgs/{}/vehicle-expenses", org.id),
    ] {
        let req = test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", auth.clone()))
            .to_request();
        let list: ApiResponse<Vec<VehicleExpense>> =
            test::read_body_json(test::call_service(&app, req).await).await;
        assert_eq!(list.data.unwrap().len(), 1);
    }

    // It shows up as the trip's cost in the ops report.
    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/reports", org.id))
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    let report: ApiResponse<OpsReport> =
        test::read_body_json(test::call_service(&app, req).await).await;
    let costs = report.data.unwrap().trip_costs;
    assert_eq!(costs.trips[0].trip_id, dispatch.id);
    assert_eq!(costs.trips[0].fuel, 4_200);

    let req = test::TestRequest::delete()
        .uri(&format!("/api/vehicle-expenses/{}", expense.id))
        .insert_header(("Authorization", auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
    let req = test::TestRequest::delete()
        .uri(&format!("/api/vehicle-expenses/{}", expense.id))
        .insert_header(("Authorization", auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 404);
}

#[actix_web::test]
async fn test_vehicle_expense_validation_and_org_isolation() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Expense Valid Co").await;
    add_vehicle_via_api(&app, org.id, &auth, "KA07-EX-1111").await;
    let (_other, other_auth) = setup_org(&app, "Expense Intruder Co").await;

    let post = |auth: &str, payload: VehicleExpensePayload| {
        test::TestRequest::post()
            .uri("/api/vehicles/KA07-EX-1111/expenses")
            .insert_header(("Authorization", auth.to_string()))
            .set_json(payload)
            .to_request()
    };

    // Non-positive amount -> 400; unknown trip -> 404; other org's vehicle -> 404.
    let resp = test::call_service(&app, post(&auth, expense_payload(None, 0))).await;
    assert_eq!(resp.status().as_u16(), 400);
    let resp =
        test::call_service(&app, post(&auth, expense_payload(Some(Uuid::new_v4()), 10))).await;
    assert_eq!(resp.status().as_u16(), 404);
    let resp = test::call_service(&app, post(&other_auth, expense_payload(None, 10))).await;
    assert_eq!(resp.status().as_u16(), 404);

    // Another org can't list this org's expenses, or delete one.
    let req = test::TestRequest::get()
        .uri(&format!("/api/orgs/{}/vehicle-expenses", org.id))
        .insert_header(("Authorization", other_auth.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
    let resp = test::call_service(&app, post(&auth, expense_payload(None, 10))).await;
    let e: ApiResponse<VehicleExpense> = test::read_body_json(resp).await;
    let req = test::TestRequest::delete()
        .uri(&format!("/api/vehicle-expenses/{}", e.data.unwrap().id))
        .insert_header(("Authorization", other_auth))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 403);
}
