use super::*;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::test_support::TestDb;
use mysql::params;
use mysql::prelude::Queryable;

fn customer(org: &Organization, name: &str, phone: Option<&str>, email: Option<&str>) -> Uuid {
    let mut c = Customer::create_customer(org.id, name, "1 Road").unwrap();
    c.set_contact(phone.map(Into::into), email.map(Into::into))
        .unwrap();
    c.id
}

#[test]
fn senders_match_by_email_ignoring_case_or_by_phone_ignoring_format() {
    let _db = TestDb::create();
    let org = Organization::create_organization("Inbound Org", "1 Road").unwrap();
    let acme = customer(
        &org,
        "Acme",
        Some("+91 98765 43210"),
        Some("Buyer@Acme.test"),
    );

    assert_eq!(
        match_customer(org.id, Channel::Email, " buyer@acme.TEST ").unwrap(),
        Some(acme)
    );
    assert_eq!(
        match_customer(org.id, Channel::Whatsapp, "9876543210").unwrap(),
        Some(acme)
    );
    assert_eq!(
        match_customer(org.id, Channel::Email, "other@acme.test").unwrap(),
        None
    );
    assert_eq!(
        match_customer(org.id, Channel::Whatsapp, "123").unwrap(),
        None
    );
}

#[test]
fn a_sender_shared_by_two_customers_matches_nobody() {
    let _db = TestDb::create();
    let org = Organization::create_organization("Shared Org", "1 Road").unwrap();
    customer(&org, "One", None, Some("shared@x.test"));
    customer(&org, "Two", None, Some("shared@x.test"));
    assert_eq!(
        match_customer(org.id, Channel::Email, "shared@x.test").unwrap(),
        None
    );
}

#[test]
fn another_orgs_customer_never_matches() {
    let _db = TestDb::create();
    let mine = Organization::create_organization("Mine", "1 Road").unwrap();
    let theirs = Organization::create_organization("Theirs", "1 Road").unwrap();
    customer(&theirs, "Acme", None, Some("buyer@acme.test"));
    let m = receive(mine.id, Channel::Email, "buyer@acme.test", None, "hello").unwrap();
    assert_eq!(m.customer_id, None);
    assert_eq!(count_received(mine.id).unwrap(), 1);
    assert_eq!(count_received(theirs.id).unwrap(), 0);
}

#[test]
fn a_repeated_external_id_returns_the_stored_message_instead_of_a_second_one() {
    let _db = TestDb::create();
    let org = Organization::create_organization("Retry Org", "1 Road").unwrap();
    let other = Organization::create_organization("Other Retry Org", "1 Road").unwrap();
    let email = |org_id, ext| receive_once(org_id, Channel::Email, "a@b.test", None, "hi", ext);

    let (first, created) = email(org.id, Some("msg-1")).unwrap();
    assert!(created);
    let (again, created) = email(org.id, Some(" msg-1 ")).unwrap();
    assert!(!created);
    assert_eq!(again.id, first.id);
    assert_eq!(count_received(org.id).unwrap(), 1);

    // The id is only unique per org and channel, and no id never dedupes.
    let whatsapp = receive_once(
        org.id,
        Channel::Whatsapp,
        "9876543210",
        None,
        "hi",
        Some("msg-1"),
    );
    assert!(whatsapp.unwrap().1);
    assert!(email(other.id, Some("msg-1")).unwrap().1);
    assert!(email(org.id, None).unwrap().1);
    assert!(email(org.id, None).unwrap().1);
    assert_eq!(count_received(org.id).unwrap(), 4);
}

#[test]
fn the_external_id_migration_keeps_existing_rows_and_can_run_twice() {
    let _db = TestDb::create();
    let org = Organization::create_organization("Migrate Org", "1 Road").unwrap();
    let mut conn = DbConnection::from_env().get_connection().unwrap();
    // The table as it was before external_id, with a message in it.
    conn.query_drop("ALTER TABLE InboundMessages DROP INDEX uq_inbound_external")
        .unwrap();
    conn.query_drop("ALTER TABLE InboundMessages DROP COLUMN external_id")
        .unwrap();
    conn.exec_drop(
        "INSERT INTO InboundMessages (id, org_id, channel, sender, body, status, received_at)
         VALUES (:id, :org_id, 'EMAIL', 'old@b.test', 'old', 'RECEIVED', 1)",
        params! { "id" => Uuid::new_v4().to_string(), "org_id" => org.id.to_string() },
    )
    .unwrap();

    ensure_tables(&mut conn).unwrap();
    ensure_tables(&mut conn).unwrap();

    let listed = list(org.id, false, 10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].external_id, None);
    let email = || receive_once(org.id, Channel::Email, "a@b.test", None, "hi", Some("m"));
    assert!(email().unwrap().1);
    assert!(!email().unwrap().1);
}
