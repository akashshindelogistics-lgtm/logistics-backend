//! Background delay-alert scan: finds every dispatch still `IN_TRANSIT` past
//! its promised delivery window
//! ([`DispatchOrder::is_running_late`](crate::logistics::dispatch::dispatch::DispatchOrder::is_running_late))
//! and records a "your order is running behind schedule" notification,
//! publishing the same kind of event the dispatch-created and delivered
//! notifications use. The background consumer does the sending. See
//! `docs/delay-alerts.md` and `docs/async-events.md`.
//!
//! [`scan_and_alert`] runs on a timer (see `events::worker`), not per request.
//! It's idempotent: the event's unique dedupe key means concurrent scans can't
//! both alert on one dispatch, and a dispatch that already carries a
//! [`NotificationEvent::DispatchRunningLate`] notification is skipped.

use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::dispatch::dispatch::DispatchOrder;
use crate::logistics::events::worker;
use crate::logistics::notification::notification::{Notification, NotificationEvent};
use crate::logistics::vendor::hire::VehicleHire;
use mysql::TxOpts;
use std::collections::HashMap;
use std::error::Error;
use uuid::Uuid;

/// Scan every `IN_TRANSIT` dispatch across every organization, and for each
/// one that's running late and hasn't already been alerted on, queue a
/// [`NotificationEvent::DispatchRunningLate`] notification to its customer.
/// Returns how many new alerts were queued.
///
/// An error on one dispatch (e.g. its customer has since been deleted) is
/// logged and skipped rather than aborting the whole scan — one bad row must
/// not stop every other late dispatch from being alerted on.
pub async fn scan_and_alert() -> Result<usize, Box<dyn Error>> {
    let late: Vec<DispatchOrder> = DispatchOrder::list_in_transit()?
        .into_iter()
        .filter(|d| d.is_running_late())
        .collect();

    let mut sent = 0;
    for dispatch in &late {
        match alert_one(dispatch).await {
            Ok(true) => sent += 1,
            Ok(false) => {}
            Err(err) => eprintln!("delay-alert scan: dispatch {}: {err}", dispatch.id),
        }
    }

    // A hired trip has one AWAITING_VEHICLE dispatch per stop, all waiting
    // on the same hire, so alert once per hire rather than once per stop.
    let mut by_hire: HashMap<Uuid, Vec<DispatchOrder>> = HashMap::new();
    for dispatch in DispatchOrder::list_awaiting_vehicle()?
        .into_iter()
        .filter(|d| d.is_awaiting_vehicle_too_long())
    {
        match dispatch.hire_id {
            Some(hire_id) => by_hire.entry(hire_id).or_default().push(dispatch),
            None => eprintln!(
                "delay-alert scan: dispatch {} is AWAITING_VEHICLE with no hire",
                dispatch.id
            ),
        }
    }
    for (hire_id, mut stops) in by_hire {
        stops.sort_by_key(|d| d.stop_sequence);
        match alert_awaiting_vehicle(hire_id, &stops).await {
            Ok(true) => sent += 1,
            Ok(false) => {}
            Err(err) => eprintln!("delay-alert scan: hire {hire_id}: {err}"),
        }
    }
    Ok(sent)
}

/// Email the org's admins that a hire's dispatches (one per stop, in trip
/// order) still have no truck. Once per hire. Returns whether a new alert was
/// queued.
async fn alert_awaiting_vehicle(
    hire_id: Uuid,
    dispatches: &[DispatchOrder],
) -> Result<bool, Box<dyn Error>> {
    let Some(first) = dispatches.first() else {
        return Ok(false);
    };
    for dispatch in dispatches {
        let already_alerted = Notification::list_by_dispatch(dispatch.id)?
            .iter()
            .any(|n| n.event == NotificationEvent::VehicleAwaitedTooLong);
        if already_alerted {
            return Ok(false);
        }
    }
    let vendor_name = VehicleHire::get_by_id(hire_id)?
        .map(|h| h.vendor_name)
        .unwrap_or_default();
    let mut stops = Vec::with_capacity(dispatches.len());
    for dispatch in dispatches {
        let name = Customer::get_by_id(dispatch.customer_id)?
            .map(|c| c.name)
            .unwrap_or_else(|| "a deleted customer".to_string());
        stops.push((dispatch.id, name));
    }
    let hours_waiting = dispatches
        .iter()
        .map(|d| d.hours_waiting())
        .max()
        .unwrap_or(0);

    let mut conn = DbConnection::from_env().get_connection()?;
    Notification::prepare_tables(&mut conn)?;
    let mut tx = conn.start_transaction(TxOpts::default())?;
    let Some(notifs) = Notification::enqueue_vehicle_awaited_too_long(
        &mut tx,
        first.org_id,
        hire_id,
        &vendor_name,
        &stops,
        hours_waiting,
    )?
    else {
        return Ok(false);
    };
    tx.commit()?;

    Notification::index_best_effort(&notifs);
    worker::wake();
    Ok(true)
}

/// Alert on one dispatch, unless it's already been alerted on or its
/// customer can no longer be found. Returns whether a new alert was queued.
async fn alert_one(dispatch: &DispatchOrder) -> Result<bool, Box<dyn Error>> {
    let already_alerted = Notification::list_by_dispatch(dispatch.id)?
        .iter()
        .any(|n| n.event == NotificationEvent::DispatchRunningLate);
    if already_alerted {
        return Ok(false);
    }

    let Some(customer) = Customer::get_by_id(dispatch.customer_id)? else {
        return Ok(false);
    };
    let hours_late = dispatch.hours_late().unwrap_or(0);

    let mut conn = DbConnection::from_env().get_connection()?;
    Notification::prepare_tables(&mut conn)?;
    let mut tx = conn.start_transaction(TxOpts::default())?;
    let Some(notifs) = Notification::enqueue_dispatch_running_late(
        &mut tx,
        dispatch.org_id,
        dispatch.id,
        &customer,
        hours_late,
    )?
    else {
        return Ok(false);
    };
    tx.commit()?;

    Notification::index_best_effort(&notifs);
    worker::wake();
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::dispatch::dispatch::{
        DispatchLineItemInput, DispatchStatus, ProofOfDeliveryInput,
    };
    use crate::logistics::dispatch::trip::Trip;
    use crate::logistics::driver::driver::Driver;
    use crate::logistics::godown::godown::Godown;
    use crate::logistics::notification::notification::NotificationStatus;
    use crate::logistics::orgs::orgs::Organization;
    use crate::logistics::stock::stock::Stock;
    use crate::logistics::test_support::TestDb;
    use crate::logistics::user::user::{OrgRole, OrgUser};
    use crate::logistics::vehicle::vehicle::{Unit, Vehicle};
    use crate::logistics::vendor::hire::HireAssignment;
    use crate::logistics::vendor::vendor::{VehicleVendor, VendorInput};
    use mysql::params;
    use mysql::prelude::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// A saved, `IN_TRANSIT` dispatch for a fresh org/customer/vehicle/godown,
    /// with `dispatched_at` backdated `hours_ago` hours so its lateness is
    /// under the test's control.
    fn in_transit_dispatch_hours_ago(hours_ago: i64) -> (Organization, Customer, DispatchOrder) {
        let mut org = Organization::create_organization("Delay Co", "1 Depot Rd").expect("org");
        org.update_location(19.0, 72.0, Some("HQ"))
            .expect("org loc");

        let driver = Driver::create(org.id, "Driver", "LIC1", "0").expect("driver");
        let mut vehicle = Vehicle::new("MH00 DL 0001".to_string(), 1_000_000, Unit::MetricTon);
        vehicle.add_new_vehicle_to_org(&org).expect("vehicle");
        vehicle
            .update_location(19.0, 72.0, Some("HQ"))
            .expect("v loc");
        vehicle.assign_driver(Some(driver.id)).expect("assign");

        let godown = Godown::create(org.id, "Main", "Dock 1", Some(1_000_000)).expect("godown");
        Stock::new(1, 1000, "Widget")
            .add_to_godown(godown.id)
            .expect("stock");

        let mut customer =
            Customer::create_customer(org.id, "Buyer", "Market Rd").expect("customer");
        customer
            .update_location(19.1, 72.1, Some("Market"))
            .expect("cust loc");
        customer
            .set_contact(None, Some("buyer@example.com".into()))
            .expect("contact");

        let mut order = org
            .dispatch_stock_to_customer(
                &customer,
                &[DispatchLineItemInput {
                    stock_description: "Widget".to_string(),
                    requested_quantity: 5,
                }],
            )
            .expect("dispatch");
        order
            .transition_to(DispatchStatus::Confirmed, None, None)
            .expect("confirmed");
        order
            .transition_to(DispatchStatus::Loaded, None, None)
            .expect("loaded");
        order
            .transition_to(DispatchStatus::InTransit, None, None)
            .expect("in transit");

        let backdated = now_unix() - hours_ago * 3600;
        let mut conn = DbConnection::from_env().get_connection().unwrap();
        conn.exec_drop(
            "UPDATE Dispatches SET dispatched_at = :t WHERE id = :id",
            params! { "t" => backdated, "id" => order.id.to_string() },
        )
        .unwrap();
        order.dispatched_at = backdated;

        (org, customer, order)
    }

    #[actix_web::test]
    async fn scan_and_alert_notifies_the_customer_of_an_overdue_in_transit_dispatch() {
        let _db = TestDb::create();
        let (_, _, order) = in_transit_dispatch_hours_ago(100); // 100h > the 72h target

        let sent = scan_and_alert().await.expect("scan");
        assert_eq!(sent, 1);

        let notifs = Notification::list_by_dispatch(order.id).expect("list");
        let alert = notifs
            .iter()
            .find(|n| n.event == NotificationEvent::DispatchRunningLate)
            .expect("alert recorded");
        assert_eq!(alert.recipient_kind, "customer");
        assert_eq!(alert.status, NotificationStatus::Queued);
        assert!(
            alert.body.to_lowercase().contains("behind"),
            "{}",
            alert.body
        );
    }

    #[actix_web::test]
    async fn scan_and_alert_never_sends_the_same_alert_twice() {
        let _db = TestDb::create();
        let (_, _, order) = in_transit_dispatch_hours_ago(100);

        assert_eq!(scan_and_alert().await.expect("first scan"), 1);
        assert_eq!(scan_and_alert().await.expect("second scan"), 0);

        let notifs = Notification::list_by_dispatch(order.id).expect("list");
        assert_eq!(
            notifs
                .iter()
                .filter(|n| n.event == NotificationEvent::DispatchRunningLate)
                .count(),
            1,
            "should never double-send: {notifs:?}"
        );
    }

    #[actix_web::test]
    async fn concurrent_scans_are_stopped_by_the_dedupe_key() {
        let _db = TestDb::create();
        let (org, customer, order) = in_transit_dispatch_hours_ago(100);
        let enqueue = || {
            let mut conn = DbConnection::from_env().get_connection().unwrap();
            Notification::prepare_tables(&mut conn).unwrap();
            let mut tx = conn.start_transaction(TxOpts::default()).unwrap();
            let rows = Notification::enqueue_dispatch_running_late(
                &mut tx, org.id, order.id, &customer, 28,
            )
            .unwrap();
            tx.commit().unwrap();
            rows
        };
        // Both scans passed the "already alerted?" check; only one may win.
        assert!(enqueue().is_some());
        assert!(enqueue().is_none());
    }

    #[actix_web::test]
    async fn scan_and_alert_skips_a_dispatch_still_within_its_promised_window() {
        let _db = TestDb::create();
        let (_, _, order) = in_transit_dispatch_hours_ago(10); // well under 72h

        assert_eq!(scan_and_alert().await.expect("scan"), 0);
        assert!(
            Notification::list_by_dispatch(order.id)
                .expect("list")
                .iter()
                .all(|n| n.event != NotificationEvent::DispatchRunningLate)
        );
    }

    #[actix_web::test]
    async fn scan_and_alert_skips_dispatches_that_are_no_longer_in_transit() {
        let _db = TestDb::create();
        let (_, _, mut order) = in_transit_dispatch_hours_ago(100);

        // Delivered — no longer IN_TRANSIT, so it drops out of the scan even
        // though it was created well over 72h ago.
        order
            .transition_to(
                DispatchStatus::Delivered,
                Some(ProofOfDeliveryInput {
                    receiver_name: "R".to_string(),
                    signature_or_photo_url: "https://x/y.png".to_string(),
                }),
                None,
            )
            .expect("delivered");

        assert_eq!(scan_and_alert().await.expect("scan"), 0);
    }

    /// A two-stop trip on one truck hired from a vendor, for an org with one
    /// active admin and no own vehicles, every stop backdated `hours_ago`
    /// hours. Returns the trip (stops in order) and its hire.
    fn hired_trip_hours_ago(hours_ago: i64) -> (Trip, VehicleHire) {
        let org = Organization::create_organization("Hire Co", "Pune").expect("org");
        OrgUser::create(
            org.id,
            "Owner",
            "owner@example.com",
            "password1",
            OrgRole::Admin,
        )
        .expect("admin");
        let godown = Godown::create(org.id, "Main", "MIDC", None).expect("godown");
        Stock::new(2, 100, "Cement")
            .add_to_godown(godown.id)
            .expect("stock");
        let mut first = Customer::create_customer(org.id, "First", "Baner").expect("customer");
        first
            .update_location(18.56, 73.78, Some("Baner"))
            .expect("location");
        let mut second = Customer::create_customer(org.id, "Second", "Aundh").expect("customer");
        second
            .update_location(18.55, 73.80, Some("Aundh"))
            .expect("location");
        let vendor = VehicleVendor::create(
            org.id,
            VendorInput {
                name: "Sharma Roadlines".into(),
                phone: "+91 1".into(),
                ..Default::default()
            },
        )
        .expect("vendor");

        let line = |quantity| {
            [DispatchLineItemInput {
                stock_description: "Cement".to_string(),
                requested_quantity: quantity,
            }]
        };
        let (lines_a, lines_b) = (line(3), line(4));
        let stops: Vec<(&Customer, &[DispatchLineItemInput])> =
            vec![(&first, &lines_a[..]), (&second, &lines_b[..])];
        let trip = org
            .dispatch_trip_on_hired_vehicle(&stops, false, vendor.id)
            .expect("hired trip");
        assert_eq!(trip.stops.len(), 2);

        let backdated = now_unix() - hours_ago * 3600;
        let mut conn = DbConnection::from_env().get_connection().unwrap();
        for stop in &trip.stops {
            conn.exec_drop(
                "UPDATE Dispatches SET dispatched_at = :t WHERE id = :id",
                params! { "t" => backdated, "id" => stop.id.to_string() },
            )
            .unwrap();
        }
        let trip = Trip::get_by_id(trip.id).unwrap().unwrap();
        let hire = VehicleHire::get_by_id(trip.hire_id.expect("hire id"))
            .unwrap()
            .unwrap();
        (trip, hire)
    }

    fn awaiting_alerts(dispatch_id: Uuid) -> Vec<Notification> {
        Notification::list_by_dispatch(dispatch_id)
            .expect("list")
            .into_iter()
            .filter(|n| n.event == NotificationEvent::VehicleAwaitedTooLong)
            .collect()
    }

    #[actix_web::test]
    async fn scan_and_alert_sends_one_awaiting_vehicle_alert_per_hired_trip() {
        let _db = TestDb::create();
        let (trip, hire) = hired_trip_hours_ago(6); // 6h > the 4h window

        assert_eq!(scan_and_alert().await.expect("scan"), 1, "one per hire");

        let alerts = awaiting_alerts(trip.stops[0].id);
        assert_eq!(alerts.len(), 1, "one email to the one admin: {alerts:?}");
        assert_eq!(alerts[0].recipient_kind, "org_admin");
        assert_eq!(alerts[0].recipient, "owner@example.com");
        let body = &alerts[0].body;
        assert!(body.contains(&hire.id.to_string()[..8]), "{body}");
        assert!(body.contains("Sharma Roadlines"), "{body}");
        assert!(body.contains("6h"), "{body}");
        for stop in &trip.stops {
            assert!(body.contains(&stop.id.to_string()[..8]), "{body}");
        }
        assert!(body.contains("First") && body.contains("Second"), "{body}");
        assert!(awaiting_alerts(trip.stops[1].id).is_empty());

        assert_eq!(scan_and_alert().await.expect("second scan"), 0);
        assert_eq!(awaiting_alerts(trip.stops[0].id).len(), 1);
    }

    #[actix_web::test]
    async fn scan_and_alert_skips_a_hired_trip_once_its_truck_is_assigned() {
        let _db = TestDb::create();
        let (trip, mut hire) = hired_trip_hours_ago(6);
        hire.assign(HireAssignment {
            registration_number: "MH12 TRIP 1".into(),
            capacity: 14,
            unit: Unit::MetricTon,
            driver_name: "Hired Driver".into(),
            driver_phone: "+91 98000 00000".into(),
            driver_license: None,
            freight_amount: 12_000,
            advance_paid: 0,
        })
        .expect("assign");

        assert_eq!(scan_and_alert().await.expect("scan"), 0);
        for stop in &trip.stops {
            assert!(awaiting_alerts(stop.id).is_empty());
        }
    }
}
