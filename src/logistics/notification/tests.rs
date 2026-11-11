use super::notification::{
    Notification, NotificationChannel, NotificationEvent, NotificationStatus,
};
use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::notification::delivery::SmsTemplate;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::test_support::TestDb;
use mysql::TxOpts;
use uuid::Uuid;

fn in_tx<T>(run: impl FnOnce(&mut mysql::Transaction<'_>) -> T) -> T {
    let mut conn = DbConnection::from_env().get_connection().expect("conn");
    Notification::prepare_tables(&mut conn).expect("tables");
    let mut tx = conn.start_transaction(TxOpts::default()).expect("tx");
    let out = run(&mut tx);
    tx.commit().expect("commit");
    out
}

fn record_created(
    org_id: Uuid,
    dispatch_id: Uuid,
    customer: &Customer,
    driver_phone: Option<&str>,
) -> Result<Vec<Notification>, Box<dyn std::error::Error>> {
    let rows = in_tx(|tx| {
        Notification::enqueue_dispatch_created(tx, org_id, dispatch_id, customer, driver_phone)
    })?;
    Notification::index_best_effort(&rows);
    Ok(rows)
}

fn record_delivered(
    org_id: Uuid,
    dispatch_id: Uuid,
    customer: &Customer,
) -> Result<Vec<Notification>, Box<dyn std::error::Error>> {
    in_tx(|tx| Notification::enqueue_dispatch_delivered(tx, org_id, dispatch_id, customer))
}

fn org() -> Organization {
    Organization::create_organization("Notify Co", "1 St").expect("org")
}

#[actix_web::test]
async fn dispatch_created_records_customer_and_driver_and_prefers_email() {
    let _db = TestDb::create();
    let org = org();
    let mut customer =
        Customer::create_customer(org.id, "Asha Rao", "9 Market Rd").expect("customer");
    customer
        .set_contact(
            Some("+91 90000 11111".into()),
            Some("asha@example.com".into()),
        )
        .expect("contact");
    let dispatch_id = Uuid::new_v4();

    let recorded =
        record_created(org.id, dispatch_id, &customer, Some("+91 98888 22222")).expect("record");

    assert_eq!(recorded.len(), 2);
    let cust = recorded
        .iter()
        .find(|n| n.recipient_kind == "customer")
        .unwrap();
    assert_eq!(cust.channel, NotificationChannel::Email);
    assert_eq!(cust.recipient, "asha@example.com");
    assert_eq!(cust.status, NotificationStatus::Queued);
    assert!(cust.body.contains("Asha Rao"));
    assert!(cust.body.contains(&dispatch_id.to_string()[..8]));

    let driver = recorded
        .iter()
        .find(|n| n.recipient_kind == "driver")
        .unwrap();
    assert_eq!(driver.channel, NotificationChannel::Sms);
    assert_eq!(driver.recipient, "+91 98888 22222");

    // Persisted and readable back, oldest first.
    let listed = Notification::list_by_dispatch(dispatch_id).expect("list");
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|n| n.event == NotificationEvent::DispatchCreated)
    );
}

#[actix_web::test]
async fn falls_back_to_sms_then_skips_when_no_contact() {
    let _db = TestDb::create();
    let org = org();

    // Phone only -> SMS.
    let mut phone_only = Customer::create_customer(org.id, "Phone Only", "1 Rd").expect("c1");
    phone_only
        .set_contact(Some("+91 70000 00000".into()), None)
        .expect("contact");
    let n = record_created(org.id, Uuid::new_v4(), &phone_only, None).expect("rec");
    let cust = n.iter().find(|n| n.recipient_kind == "customer").unwrap();
    assert_eq!(cust.channel, NotificationChannel::Sms);
    assert_eq!(cust.recipient, "+91 70000 00000");

    // Driver phone missing -> that row is SKIPPED.
    let driver = n.iter().find(|n| n.recipient_kind == "driver").unwrap();
    assert_eq!(driver.status, NotificationStatus::Skipped);

    // No contact at all -> customer row SKIPPED.
    let no_contact = Customer::create_customer(org.id, "No Contact", "2 Rd").expect("c2");
    let n2 = record_created(org.id, Uuid::new_v4(), &no_contact, None).expect("rec2");
    assert!(n2.iter().all(|n| n.status == NotificationStatus::Skipped));
}

#[actix_web::test]
async fn dispatch_delivered_notifies_only_the_customer() {
    let _db = TestDb::create();
    let org = org();
    let mut customer = Customer::create_customer(org.id, "Beed Traders", "3 Rd").expect("customer");
    customer
        .set_contact(None, Some("beed@example.com".into()))
        .expect("contact");
    let dispatch_id = Uuid::new_v4();

    let recorded = record_delivered(org.id, dispatch_id, &customer).expect("rec");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].recipient_kind, "customer");
    assert_eq!(recorded[0].event, NotificationEvent::DispatchDelivered);
    assert!(recorded[0].body.contains("delivered"));
}

#[actix_web::test]
async fn list_by_org_is_scoped_and_newest_first() {
    let _db = TestDb::create();
    let a = Organization::create_organization("Org A", "1").expect("a");
    let b = Organization::create_organization("Org B", "2").expect("b");
    let mut ca = Customer::create_customer(a.id, "CA", "1").expect("ca");
    ca.set_contact(None, Some("ca@example.com".into()))
        .expect("contact");
    let cb = Customer::create_customer(b.id, "CB", "2").expect("cb");

    record_created(a.id, Uuid::new_v4(), &ca, None).expect("a1");
    record_delivered(a.id, Uuid::new_v4(), &ca).expect("a2");
    record_created(b.id, Uuid::new_v4(), &cb, None).expect("b1");

    let a_list = Notification::list_by_org(a.id, 100).expect("list a");
    assert_eq!(a_list.len(), 3); // 2 created (cust+driver) + 1 delivered
    assert!(a_list.iter().all(|n| n.org_id == a.id));
    // DESC by created_at: the delivered event (recorded last) is at or near the top.
    assert!(
        a_list
            .iter()
            .take(1)
            .any(|n| n.event == NotificationEvent::DispatchDelivered)
    );

    assert_eq!(
        Notification::list_by_org(b.id, 100).expect("list b").len(),
        2
    );
}

#[actix_web::test]
async fn deleting_the_org_cascades_to_its_notifications() {
    let _db = TestDb::create();
    let org = org();
    let c = Customer::create_customer(org.id, "Gone", "0").expect("c");
    let d = Uuid::new_v4();
    record_created(org.id, d, &c, None).expect("rec");

    org.remove_organization().expect("remove org");
    assert!(
        Notification::list_by_org(org.id, 100)
            .expect("list")
            .is_empty()
    );
}

#[actix_web::test]
async fn notifications_are_recorded_with_the_fixed_template() {
    // SAFETY: this module is the only test file that reads ANTHROPIC_API_KEY
    // concurrently in this way — see ai/status.rs's equivalent test for the
    // full rationale.
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    let _db = TestDb::create();
    let org = org();
    let mut customer =
        Customer::create_customer(org.id, "Asha Rao", "9 Market Rd").expect("customer");
    customer
        .set_contact(None, Some("asha@example.com".into()))
        .expect("contact");
    let dispatch_id = Uuid::new_v4();
    let short_id = &dispatch_id.to_string()[..8];

    let created = record_created(org.id, dispatch_id, &customer, Some("+91 98888 22222"))
        .expect("record created");
    let cust = created
        .iter()
        .find(|n| n.recipient_kind == "customer")
        .unwrap();
    assert_eq!(
        cust.body,
        format!("Hi Asha Rao, your order (ref {short_id}) has been dispatched and is on its way.")
    );
    let driver = created
        .iter()
        .find(|n| n.recipient_kind == "driver")
        .unwrap();
    assert_eq!(
        driver.body,
        format!("New trip assigned: dispatch {short_id} to Asha Rao.")
    );

    let delivered = record_delivered(org.id, dispatch_id, &customer).expect("record delivered");
    assert_eq!(
        delivered[0].body,
        format!("Hi Asha Rao, your order (ref {short_id}) has been delivered. Thank you!")
    );
}

#[actix_web::test]
async fn recording_a_notification_indexes_it_for_the_assistant() {
    use crate::logistics::ai::chunk;

    let _db = TestDb::create();
    let org = org();
    let mut customer =
        Customer::create_customer(org.id, "Priya Sharma", "5 Market Rd").expect("customer");
    customer
        .set_contact(None, Some("priya@example.com".into()))
        .expect("contact");
    let dispatch_id = Uuid::new_v4();

    record_created(org.id, dispatch_id, &customer, None).expect("record");

    let results = chunk::search_by_org(org.id, "Priya Sharma dispatched", 8).expect("search");
    assert!(
        results.iter().any(|c| c.text.contains("Priya Sharma")),
        "the recorded notification should be indexed and findable: {results:?}"
    );
}

#[actix_web::test]
async fn sms_notifications_use_the_fixed_template_text_and_carry_its_variables() {
    let _db = TestDb::create();
    let org = org();
    let mut customer =
        Customer::create_customer(org.id, "Asha Rao", "9 Market Rd").expect("customer");
    customer
        .set_contact(Some("+91 98888 11111".into()), None)
        .expect("contact");
    let dispatch_id = Uuid::new_v4();
    let short_id = dispatch_id.to_string()[..8].to_string();

    let created = record_created(org.id, dispatch_id, &customer, Some("+91 98888 22222"))
        .expect("record created");
    let cust = created
        .iter()
        .find(|n| n.recipient_kind == "customer")
        .unwrap();
    assert_eq!(cust.channel, NotificationChannel::Sms);
    assert_eq!(
        cust.body,
        format!("Hi Asha Rao, your order (ref {short_id}) has been dispatched and is on its way.")
    );
    assert_eq!(cust.sms_template(), SmsTemplate::DispatchCreatedCustomer);
    assert_eq!(
        cust.sms_vars(&customer.name),
        ["Asha Rao", short_id.as_str()]
    );
    let driver = created
        .iter()
        .find(|n| n.recipient_kind == "driver")
        .unwrap();
    assert_eq!(
        driver.body,
        format!("New trip assigned: dispatch {short_id} to Asha Rao.")
    );
    assert_eq!(driver.sms_template(), SmsTemplate::DispatchCreatedDriver);
    assert_eq!(
        driver.sms_vars(&customer.name),
        [short_id.as_str(), "Asha Rao"]
    );

    let late = in_tx(|tx| {
        Notification::enqueue_dispatch_running_late(tx, org.id, dispatch_id, &customer, 7)
    })
    .expect("record late")
    .expect("first late alert");
    assert_eq!(
        late[0].body,
        format!(
            "Hi Asha Rao, your order (ref {short_id}) is running behind its expected delivery time. We're sorry for the delay."
        )
    );
    assert_eq!(late[0].sms_template(), SmsTemplate::DispatchRunningLate);
    assert_eq!(
        late[0].sms_vars(&customer.name),
        ["Asha Rao", short_id.as_str()]
    );
}

#[actix_web::test]
async fn a_long_customer_name_is_cut_to_the_dlt_limit_in_the_text_and_the_variable() {
    let _db = TestDb::create();
    let org = org();
    let long_name = "A".repeat(60);
    let mut customer = Customer::create_customer(org.id, &long_name, "1 Rd").expect("customer");
    customer
        .set_contact(Some("9888811111".into()), None)
        .expect("contact");
    let dispatch_id = Uuid::new_v4();

    let delivered = record_delivered(org.id, dispatch_id, &customer).expect("record");
    let cut = "A".repeat(40);
    assert_eq!(delivered[0].sms_vars(&customer.name)[0], cut);
    assert!(delivered[0].body.starts_with(&format!("Hi {cut},")));
}

#[test]
fn tracking_link_is_appended_only_when_there_is_one() {
    use super::notification::with_tracking_link;
    use crate::logistics::dispatch::dispatch::{set_test_public_app_url, tracking_url};

    let token = Uuid::new_v4();
    set_test_public_app_url(None);
    assert_eq!(tracking_url(token), None);
    set_test_public_app_url(Some("  "));
    assert_eq!(tracking_url(token), None);
    set_test_public_app_url(Some("https://x.example/ "));
    let url = tracking_url(token).unwrap();
    assert_eq!(url, format!("https://x.example/track/{token}"));
    set_test_public_app_url(None);

    assert_eq!(with_tracking_link("Hi".into(), None), "Hi");
    assert_eq!(
        with_tracking_link("Hi".into(), Some(&url)),
        format!("Hi\n\nTrack your order: {url}")
    );
}
