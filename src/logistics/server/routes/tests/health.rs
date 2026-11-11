use super::*;

#[actix_web::test]
async fn test_health_check_endpoint() {
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get().uri("/api/health").to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: ApiResponse<String> = test::read_body_json(resp).await;
    assert!(body.success);
    assert_eq!(body.message, "Logistics system REST API operational");
}

#[actix_web::test]
async fn test_swagger_ui_endpoint() {
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get().uri("/swagger-ui/").to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success() || resp.status().is_redirection());
}

#[actix_web::test]
async fn test_openapi_json_spec_endpoint() {
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get()
        .uri("/api-docs/openapi.json")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[actix_web::test]
async fn test_metrics_count_api_requests_by_route_pattern() {
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get().uri("/api/health").to_request();
    test::call_service(&app, req).await;
    let req = test::TestRequest::get().uri("/metrics").to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    // Other tests share the process-wide registry, so check presence, not counts.
    assert!(
        body.contains("http_requests_total{method=\"GET\",route=\"/api/health\",status=\"200\"}")
    );
    assert!(body.contains(
        "http_request_duration_seconds_bucket{method=\"GET\",route=\"/api/health\",le=\"+Inf\"}"
    ));
    assert!(body.contains("delay_alert_scans_total{result=\"ok\"}"));
}
