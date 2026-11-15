use super::*;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::test_support::TestDb;

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
