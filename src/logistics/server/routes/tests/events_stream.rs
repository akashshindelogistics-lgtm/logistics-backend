use super::*;
use crate::logistics::events::queue::{self, EventType};
use actix_web::body::MessageBody;
use mysql::params;
use mysql::prelude::Queryable;

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

async fn next_chunk<B: MessageBody + Unpin>(body: &mut B) -> Option<String>
where
    B::Error: std::fmt::Debug,
{
    let chunk = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_next(cx)),
    )
    .await
    .expect("stream stalled")?;
    Some(String::from_utf8(chunk.unwrap().to_vec()).unwrap())
}

#[actix_web::test]
async fn test_stream_pages_past_a_full_second_of_events() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Stream Burst Org").await;

    let req = test::TestRequest::get()
        .uri("/api/events/stream")
        .insert_header(("Authorization", auth))
        .to_request();
    let mut body = test::call_service(&app, req).await.into_body();
    assert!(
        next_chunk(&mut body)
            .await
            .unwrap()
            .starts_with(": connected")
    );

    // More than one page of events in one second, then one more after it,
    // all committed together so the stream sees them at once.
    let second = queue::now_unix();
    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    queue::ensure_tables(&mut conn).unwrap();
    let mut tx = conn.start_transaction(mysql::TxOpts::default()).unwrap();
    for i in 0..150 {
        queue::publish(
            &mut tx,
            org.id,
            EventType::DispatchStatusChanged,
            serde_json::json!({ "status": format!("BURST_{i}") }),
            None,
        )
        .unwrap();
    }
    tx.exec_drop(
        "UPDATE Events SET created_at = :t WHERE org_id = :org",
        params! { "t" => second, "org" => org.id.to_string() },
    )
    .unwrap();
    queue::publish(
        &mut tx,
        org.id,
        EventType::DispatchStatusChanged,
        serde_json::json!({ "status": "AFTER_BURST" }),
        None,
    )
    .unwrap();
    tx.exec_drop(
        "UPDATE Events SET created_at = :t WHERE org_id = :org AND payload LIKE '%AFTER_BURST%'",
        params! { "t" => second + 1, "org" => org.id.to_string() },
    )
    .unwrap();
    tx.commit().unwrap();

    let mut text = String::new();
    while !text.contains("AFTER_BURST") {
        text.push_str(&next_chunk(&mut body).await.expect("stream ended"));
    }
    assert_eq!(text.matches("event: dispatch.status_changed").count(), 151);
}

#[actix_web::test]
async fn test_stream_sends_a_later_event_that_sorts_before_a_sent_one() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, auth) = setup_org(&app, "Stream Same Second Org").await;

    let req = test::TestRequest::get()
        .uri("/api/events/stream")
        .insert_header(("Authorization", auth))
        .to_request();
    let mut body = test::call_service(&app, req).await.into_body();
    assert!(
        next_chunk(&mut body)
            .await
            .unwrap()
            .starts_with(": connected")
    );

    let mut conn = crate::logistics::db::connection::DbConnection::from_env()
        .get_connection()
        .unwrap();
    queue::ensure_tables(&mut conn).unwrap();
    let publish = |conn: &mut mysql::PooledConn, status: &str| {
        let mut tx = conn.start_transaction(mysql::TxOpts::default()).unwrap();
        queue::publish(
            &mut tx,
            org.id,
            EventType::DispatchStatusChanged,
            serde_json::json!({ "status": status }),
            None,
        )
        .unwrap();
        tx.commit().unwrap();
    };

    publish(&mut conn, "FIRST_EVENT");
    let mut text = String::new();
    while !text.contains("FIRST_EVENT") {
        text.push_str(&next_chunk(&mut body).await.expect("stream ended"));
    }
    let first_second: i64 = conn
        .exec_first(
            "SELECT created_at FROM Events WHERE org_id = :org",
            params! { "org" => org.id.to_string() },
        )
        .unwrap()
        .unwrap();

    // Same second as the sent event, and an id that sorts below it.
    publish(&mut conn, "SECOND_EVENT");
    conn.exec_drop(
        "UPDATE Events SET id = '00000000-0000-0000-0000-000000000000', created_at = :t
         WHERE org_id = :org AND payload LIKE '%SECOND_EVENT%'",
        params! { "t" => first_second, "org" => org.id.to_string() },
    )
    .unwrap();

    let mut later = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        while !later.contains("SECOND_EVENT") {
            later.push_str(&next_chunk(&mut body).await.expect("stream ended"));
        }
    })
    .await
    .expect("same-second event with a lower id was never sent");
    assert!(
        later.contains("id: 00000000-0000-0000-0000-000000000000"),
        "{later}"
    );
    // The look-back re-reads the first event but does not send it again.
    assert!(!later.contains("FIRST_EVENT"), "{later}");
}

#[actix_web::test]
async fn test_stream_ends_when_token_expires() {
    let _db = TestDb::create();
    let app = test::init_service(App::new().configure(config_routes)).await;
    let (org, _) = setup_org(&app, "Stream Expiry Org").await;
    let exp = queue::now_unix() as u64 + 2;
    let token =
        crate::logistics::auth::auth::generate_token_expiring_at(org.id, &org.name, exp).unwrap();

    let req = test::TestRequest::get()
        .uri("/api/events/stream")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let mut body = resp.into_body();
    assert!(
        next_chunk(&mut body)
            .await
            .unwrap()
            .starts_with(": connected")
    );
    // No events: the next item is the end of the stream, not a keepalive.
    assert_eq!(next_chunk(&mut body).await, None);
    assert!(queue::now_unix() as u64 >= exp);
}
