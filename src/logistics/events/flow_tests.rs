//! The event queue wired into real dispatch flows: what gets published, and
//! that a failure part-way leaves nothing behind.

use super::worker::{drain_once, registered_consumers};
use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::dispatch::dispatch::{
    DispatchLineItemInput, DispatchOrder, DispatchStatus, ProofOfDeliveryInput,
};
use crate::logistics::driver::driver::Driver;
use crate::logistics::godown::godown::Godown;
use crate::logistics::notification::notification::{
    Notification, NotificationEvent, NotificationStatus,
};
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::stock::stock::Stock;
use crate::logistics::test_support::TestDb;
use crate::logistics::vehicle::vehicle::{Unit, Vehicle};
use crate::logistics::vendor::hire::{HireAssignment, VehicleHire};
use crate::logistics::vendor::vendor::{VehicleVendor, VendorInput};
use mysql::prelude::*;
use uuid::Uuid;

const START_STOCK: i64 = 1000;

fn scalar(sql: &str) -> i64 {
    let mut conn = DbConnection::from_env().get_connection().expect("conn");
    conn.query_first(sql).expect("query").expect("a row")
}

fn stock_left() -> i64 {
    scalar("SELECT COALESCE(SUM(quantity), 0) FROM Stock")
}

fn pending_events(event_type: &str) -> i64 {
    scalar(&format!(
        "SELECT COUNT(*) FROM EventDeliveries d JOIN Events e ON e.id = d.event_id
         WHERE d.status = 'PENDING' AND e.event_type = '{event_type}'"
    ))
}

/// Make every insert into `table` fail, simulating a crash part-way through.
fn break_inserts_into(table: &str) {
    let mut conn = DbConnection::from_env().get_connection().expect("conn");
    conn.query_drop(format!(
        "CREATE TRIGGER fail_insert BEFORE INSERT ON {table} FOR EACH ROW
         SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'simulated failure'"
    ))
    .expect("create trigger");
}

fn line() -> Vec<DispatchLineItemInput> {
    vec![DispatchLineItemInput {
        stock_description: "Widget".to_string(),
        requested_quantity: 5,
    }]
}

fn customer(org: &Organization, name: &str) -> Customer {
    let mut c = Customer::create_customer(org.id, name, "Market Rd").expect("customer");
    c.update_location(19.1, 72.1, Some("Market"))
        .expect("location");
    c.set_contact(None, Some(format!("{name}@example.com")))
        .expect("contact");
    c
}

fn setup() -> (Organization, Customer) {
    let mut org = Organization::create_organization("Flow Co", "1 Depot Rd").expect("org");
    org.update_location(19.0, 72.0, Some("HQ"))
        .expect("org loc");

    let driver = Driver::create(org.id, "Driver", "LIC1", "+91 90000 00001").expect("driver");
    let mut vehicle = Vehicle::new("MH00 FL 0001".to_string(), 1_000_000, Unit::MetricTon);
    vehicle.add_new_vehicle_to_org(&org).expect("vehicle");
    vehicle
        .update_location(19.0, 72.0, Some("HQ"))
        .expect("vehicle loc");
    vehicle.assign_driver(Some(driver.id)).expect("assign");

    let godown = Godown::create(org.id, "Main", "Dock 1", Some(1_000_000)).expect("godown");
    Stock::new(1, START_STOCK, "Widget")
        .add_to_godown(godown.id)
        .expect("stock");

    let c = customer(&org, "buyer");
    (org, c)
}

fn delivered(order: &mut DispatchOrder) -> Result<(), Box<dyn std::error::Error>> {
    order.transition_to(
        DispatchStatus::Delivered,
        Some(ProofOfDeliveryInput {
            receiver_name: "R".to_string(),
            signature_or_photo_url: "https://x/y.png".to_string(),
        }),
        None,
    )
}

fn to_in_transit(order: &mut DispatchOrder) {
    for status in [
        DispatchStatus::Confirmed,
        DispatchStatus::Loaded,
        DispatchStatus::InTransit,
    ] {
        order.transition_to(status, None, None).expect("advance");
    }
}

fn hire_vendor(org: &Organization) -> Uuid {
    VehicleVendor::create(
        org.id,
        VendorInput {
            name: "Hauliers".to_string(),
            contact_person: None,
            phone: "+91 90000 00002".to_string(),
            gstin: None,
            notes: None,
        },
    )
    .expect("vendor")
    .id
}

#[actix_web::test]
async fn a_dispatch_records_notifications_and_publishes_one_event() {
    let _db = TestDb::create();
    let (org, c) = setup();

    let order = org
        .dispatch_stock_to_customer(&c, &line())
        .expect("dispatch");

    let rows = Notification::list_by_dispatch(order.id).expect("list");
    assert_eq!(rows.len(), 2, "customer and driver: {rows:?}");
    assert!(rows.iter().all(|n| n.status == NotificationStatus::Queued));
    assert!(
        rows.iter()
            .any(|n| n.recipient_kind == "driver" && n.recipient == "+91 90000 00001")
    );
    assert_eq!(pending_events("dispatch.created"), 1);
}

#[actix_web::test]
async fn a_trip_publishes_one_event_per_stop() {
    let _db = TestDb::create();
    let (org, c1) = setup();
    let c2 = customer(&org, "second");
    let (l1, l2) = (line(), line());

    let trip = org
        .dispatch_trip_to_customers(&[(&c1, l1.as_slice()), (&c2, l2.as_slice())], false)
        .expect("trip");

    assert_eq!(trip.stops.len(), 2);
    assert_eq!(pending_events("dispatch.created"), 2);
    assert_eq!(stock_left(), START_STOCK - 10);
}

#[actix_web::test]
async fn a_hired_dispatch_stays_silent_until_its_truck_is_assigned() {
    let _db = TestDb::create();
    let (org, c) = setup();

    let order = org
        .dispatch_stock_on_hired_vehicle(&c, &line(), hire_vendor(&org))
        .expect("hired dispatch");
    assert_eq!(pending_events("dispatch.created"), 0);
    assert!(
        Notification::list_by_dispatch(order.id)
            .expect("list")
            .is_empty()
    );

    let mut hire = VehicleHire::get_by_id(order.hire_id.expect("hire id"))
        .expect("get hire")
        .expect("hire");
    hire.assign(HireAssignment {
        registration_number: "MH00 HR 0001".to_string(),
        capacity: 1_000_000,
        unit: Unit::MetricTon,
        driver_name: "Hired Driver".to_string(),
        driver_phone: "+91 90000 00003".to_string(),
        driver_license: None,
        freight_amount: 1000,
        advance_paid: 0,
    })
    .expect("assign");

    assert_eq!(pending_events("dispatch.created"), 1);
    let rows = Notification::list_by_dispatch(order.id).expect("list");
    assert!(
        rows.iter()
            .any(|n| n.recipient_kind == "driver" && n.recipient == "+91 90000 00003"),
        "{rows:?}"
    );
}

#[actix_web::test]
async fn delivery_publishes_only_for_the_delivered_status() {
    let _db = TestDb::create();
    let (org, c) = setup();
    let mut order = org
        .dispatch_stock_to_customer(&c, &line())
        .expect("dispatch");

    to_in_transit(&mut order);
    assert_eq!(pending_events("dispatch.delivered"), 0);

    delivered(&mut order).expect("delivered");
    assert_eq!(pending_events("dispatch.delivered"), 1);
    assert!(
        Notification::list_by_dispatch(order.id)
            .expect("list")
            .iter()
            .any(|n| n.event == NotificationEvent::DispatchDelivered)
    );
}

#[actix_web::test]
async fn the_consumer_finishes_a_real_event_without_any_provider() {
    // SAFETY: matches the other notification tests that clear these keys.
    unsafe {
        for key in ["ANTHROPIC_API_KEY", "TWILIO_ACCOUNT_SID", "RESEND_API_KEY"] {
            std::env::remove_var(key);
        }
    }
    let _db = TestDb::create();
    let (org, c) = setup();
    let order = org
        .dispatch_stock_to_customer(&c, &line())
        .expect("dispatch");

    let claimed = drain_once(&registered_consumers(), "w")
        .await
        .expect("drain");

    assert_eq!(claimed, 1);
    assert_eq!(pending_events("dispatch.created"), 0);
    assert_eq!(
        scalar("SELECT COUNT(*) FROM EventDeliveries WHERE status = 'DONE'"),
        1
    );
    // Nothing could be sent, so the rows wait for a provider.
    assert!(
        Notification::list_by_dispatch(order.id)
            .expect("list")
            .iter()
            .all(|n| n.status == NotificationStatus::Queued)
    );
}

#[actix_web::test]
async fn a_failed_publish_undoes_the_whole_dispatch() {
    let _db = TestDb::create();
    let (org, c) = setup();
    break_inserts_into("Events");

    assert!(org.dispatch_stock_to_customer(&c, &line()).is_err());

    assert_eq!(stock_left(), START_STOCK, "stock must not be drawn down");
    assert_eq!(scalar("SELECT COUNT(*) FROM Dispatches"), 0);
    assert_eq!(scalar("SELECT COUNT(*) FROM DispatchLineItems"), 0);
    assert_eq!(scalar("SELECT COUNT(*) FROM Notifications"), 0);
}

#[actix_web::test]
async fn a_failure_on_a_later_stop_undoes_the_whole_trip() {
    let _db = TestDb::create();
    let (org, c1) = setup();
    let c2 = customer(&org, "second");
    let (l1, l2) = (line(), line());
    break_inserts_into("Events");

    assert!(
        org.dispatch_trip_to_customers(&[(&c1, l1.as_slice()), (&c2, l2.as_slice())], false)
            .is_err()
    );

    assert_eq!(stock_left(), START_STOCK);
    assert_eq!(scalar("SELECT COUNT(*) FROM Trips"), 0);
    assert_eq!(scalar("SELECT COUNT(*) FROM Dispatches"), 0);
}

#[actix_web::test]
async fn a_failed_publish_undoes_the_delivered_status() {
    let _db = TestDb::create();
    let (org, c) = setup();
    let mut order = org
        .dispatch_stock_to_customer(&c, &line())
        .expect("dispatch");
    to_in_transit(&mut order);
    let history_before = order.status_history.len();
    break_inserts_into("Events");

    assert!(delivered(&mut order).is_err());

    // The in-memory value and the database both stay as they were.
    assert_eq!(order.status, DispatchStatus::InTransit);
    assert_eq!(order.status_history.len(), history_before);
    assert!(order.proof_of_delivery.is_none());
    let stored = DispatchOrder::get_by_id(order.id)
        .expect("get")
        .expect("row");
    assert_eq!(stored.status, DispatchStatus::InTransit);
    assert!(stored.proof_of_delivery.is_none());
}

#[actix_web::test]
async fn a_failure_after_the_credit_undoes_a_return() {
    let _db = TestDb::create();
    let (org, c) = setup();
    let mut order = org
        .dispatch_stock_to_customer(&c, &line())
        .expect("dispatch");
    to_in_transit(&mut order);
    let drawn_down = stock_left();
    break_inserts_into("DispatchStatusHistory");

    assert!(
        order
            .transition_to(DispatchStatus::Returned, None, None)
            .is_err()
    );

    assert_eq!(stock_left(), drawn_down, "the credit must be rolled back");
    assert_eq!(order.status, DispatchStatus::InTransit);
}

#[actix_web::test]
async fn search_indexing_sees_committed_stock_after_a_dispatch() {
    use crate::logistics::ai::chunk;

    let _db = TestDb::create();
    let (org, c) = setup();
    org.dispatch_stock_to_customer(&c, &line())
        .expect("dispatch");

    let results = chunk::search_by_org(org.id, "Widget", 20).expect("search");
    let expected = (START_STOCK - 5).to_string();
    assert!(
        results.iter().any(|r| r.text.contains(&expected)),
        "the index must hold the post-dispatch quantity {expected}: {results:?}"
    );
}
