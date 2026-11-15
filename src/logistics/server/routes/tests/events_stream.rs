use super::*;
use crate::logistics::events::queue::{self, EventType};
use actix_web::body::MessageBody;

#[actix_web::test]
async fn test_stream_requires_auth() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let req = test::TestRequest::get()
        .uri("/api/events/stream")
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
}

#[actix_web::test]
async fn test_stream_delivers_this_orgs_events_only() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Stream Org").await;
    let (other, _) = setup_org(&app, "Stream Other Org").await;

    let req = test::TestRequest::get()
        .uri("/api/events/stream")
        .insert_header(("Authorization", auth))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("Content-Type").unwrap(),
        "text/event-stream"
    );
    let mut body = resp.into_body();

    // Opening comment, then publish after connecting.
    let first = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_next(cx))
        .await
        .unwrap()
        .unwrap();
    assert!(first.starts_with(b": connected"));

    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    queue::ensure_tables(&mut conn).unwrap();
    for (org_id, status) in [(other.id, "LOADED"), (org.id, "IN_TRANSIT")] {
        let mut tx = conn.start_transaction(mysql::TxOpts::default()).unwrap();
        queue::publish(
            &mut tx,
            org_id,
            EventType::DispatchStatusChanged,
            serde_json::json!({ "status": status }),
            None,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let chunk = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_next(cx))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(chunk.to_vec()).unwrap();
    assert!(text.contains("event: dispatch.status_changed"), "{text}");
    assert!(text.contains("IN_TRANSIT"), "{text}");
    assert!(!text.contains("LOADED"), "{text}");
}
