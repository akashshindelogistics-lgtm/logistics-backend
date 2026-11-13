use super::user::{OrgRole, OrgUser, ResendOutcome, UserError};
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::test_support::TestDb;

fn make_org(name: &str) -> Organization {
    Organization::create_organization(name, "1 St").expect("create org")
}

#[test]
fn role_strings_round_trip_and_unknown_is_least_privilege() {
    for role in [OrgRole::Admin, OrgRole::Dispatcher, OrgRole::WarehouseStaff] {
        assert_eq!(OrgRole::from_str(role.as_str()), role);
    }
    assert_eq!(OrgRole::from_str("dispatch"), OrgRole::Dispatcher);
    assert_eq!(
        OrgRole::from_str("warehouse staff"),
        OrgRole::WarehouseStaff
    );
    assert_eq!(OrgRole::from_str("nonsense"), OrgRole::WarehouseStaff);
    assert_eq!(OrgRole::from_str(""), OrgRole::WarehouseStaff);
}

#[test]
fn create_then_authenticate_and_reload() {
    let _db = TestDb::create();
    let org = make_org("Team Org");

    let user = OrgUser::create(
        org.id,
        "Priya",
        "PRIYA@Example.com ",
        "hunter2pw",
        OrgRole::Dispatcher,
    )
    .expect("create user");
    assert_eq!(user.email, "priya@example.com", "email is normalised");
    assert_eq!(user.role, OrgRole::Dispatcher);
    assert!(user.is_active);

    let ok = OrgUser::verify_login("priya@example.com", "hunter2pw")
        .expect("verify")
        .expect("some");
    assert_eq!(ok.id, user.id);
    assert!(
        OrgUser::verify_login("priya@example.com", "wrong")
            .expect("verify")
            .is_none()
    );
    assert!(
        OrgUser::verify_login("nobody@example.com", "hunter2pw")
            .expect("verify")
            .is_none()
    );

    let reloaded = OrgUser::get_by_id(user.id).expect("get").expect("some");
    assert_eq!(reloaded.name, "Priya");
    assert_eq!(reloaded.org_id, org.id);
}

#[test]
fn email_must_be_unique_across_orgs() {
    let _db = TestDb::create();
    let a = make_org("Org A");
    let b = make_org("Org B");
    OrgUser::create(a.id, "A", "dup@example.com", "password1", OrgRole::Admin).expect("first");
    let err =
        OrgUser::create(b.id, "B", "DUP@example.com", "password1", OrgRole::Admin).unwrap_err();
    assert!(matches!(err, UserError::EmailTaken));
}

#[test]
fn rejects_bad_email_and_short_password() {
    let _db = TestDb::create();
    let org = make_org("Validation Org");
    assert!(matches!(
        OrgUser::create(org.id, "X", "not-an-email", "password1", OrgRole::Admin).unwrap_err(),
        UserError::InvalidInput(_)
    ));
    assert!(matches!(
        OrgUser::create(org.id, "X", "x@example.com", "short", OrgRole::Admin).unwrap_err(),
        UserError::InvalidInput(_)
    ));
}

#[test]
fn inactive_user_cannot_log_in() {
    let _db = TestDb::create();
    let org = make_org("Deactivate Org");
    let mut user = OrgUser::create(
        org.id,
        "Sam",
        "sam@example.com",
        "password1",
        OrgRole::WarehouseStaff,
    )
    .expect("create");

    user.update("Sam", OrgRole::WarehouseStaff, false)
        .expect("deactivate");
    assert!(
        OrgUser::verify_login("sam@example.com", "password1")
            .expect("verify")
            .is_none()
    );

    user.update("Sam", OrgRole::Dispatcher, true)
        .expect("reactivate + promote");
    let back = OrgUser::verify_login("sam@example.com", "password1")
        .expect("verify")
        .expect("some");
    assert_eq!(back.role, OrgRole::Dispatcher);
}

#[test]
fn list_is_scoped_to_the_org_and_delete_removes_one() {
    let _db = TestDb::create();
    let a = make_org("Scoped A");
    let b = make_org("Scoped B");
    OrgUser::create(a.id, "A1", "a1@example.com", "password1", OrgRole::Admin).expect("a1");
    let a2 = OrgUser::create(
        a.id,
        "A2",
        "a2@example.com",
        "password1",
        OrgRole::Dispatcher,
    )
    .expect("a2");
    OrgUser::create(b.id, "B1", "b1@example.com", "password1", OrgRole::Admin).expect("b1");

    assert_eq!(OrgUser::list_by_org(a.id).expect("list a").len(), 2);
    assert_eq!(OrgUser::list_by_org(b.id).expect("list b").len(), 1);

    a2.delete().expect("delete a2");
    assert_eq!(OrgUser::list_by_org(a.id).expect("list a again").len(), 1);
}

#[test]
fn deleting_the_org_cascades_to_its_users() {
    let _db = TestDb::create();
    let org = make_org("Cascade Org");
    OrgUser::create(
        org.id,
        "Gone",
        "gone@example.com",
        "password1",
        OrgRole::Admin,
    )
    .expect("create");

    org.remove_organization().expect("remove org");
    assert!(OrgUser::list_by_org(org.id).expect("list").is_empty());
}

#[test]
fn validate_password_enforces_the_signup_rules() {
    use crate::logistics::user::user::validate_password;
    assert!(validate_password("passw0rd1", "a@b.com").is_ok());
    assert!(validate_password("sh0rt", "a@b.com").is_err());
    assert!(validate_password("nodigitshere", "a@b.com").is_err());
    assert!(validate_password("12345678", "a@b.com").is_err());
    assert!(validate_password(&format!("a1{}", "x".repeat(71)), "a@b.com").is_err());
}

fn signup(email: &str) -> (OrgUser, String) {
    let (_, user, token) =
        OrgUser::signup("Verify Org", "1 St", "Owner", email, "passw0rd1").expect("signup");
    (user, token)
}

#[test]
fn signup_user_must_verify_and_the_token_is_single_use() {
    let _db = TestDb::create();
    let (user, token) = signup("verify@example.com");
    assert!(!OrgUser::email_verified(user.id).unwrap());
    assert_eq!(OrgUser::unverified_ids(user.org_id).unwrap(), vec![user.id]);

    assert!(OrgUser::verify_email("not-the-token").unwrap().is_none());
    let verified = OrgUser::verify_email(&token).unwrap().expect("verified");
    assert_eq!(verified.id, user.id);
    assert!(OrgUser::email_verified(user.id).unwrap());
    assert!(OrgUser::unverified_ids(user.org_id).unwrap().is_empty());
    assert!(
        OrgUser::verify_email(&token).unwrap().is_none(),
        "a used token is dead"
    );
}

#[test]
fn expired_token_is_rejected_and_resend_replaces_it() {
    use crate::logistics::db::connection::DbConnection;
    use mysql::{params, prelude::*};
    let _db = TestDb::create();
    let (user, token) = signup("expired@example.com");

    // Right after signup a resend is too soon.
    assert_eq!(
        OrgUser::resend_verification(user.id).unwrap(),
        ResendOutcome::TooSoon
    );
    // Age the token past both the cooldown and its expiry.
    let mut conn = DbConnection::from_env().get_connection().unwrap();
    conn.exec_drop(
        "UPDATE OrgUsers SET email_verification_expires_at = 1 WHERE id = :id",
        params! { "id" => user.id.to_string() },
    )
    .unwrap();
    assert!(OrgUser::verify_email(&token).unwrap().is_none());

    let ResendOutcome::Issued(fresh) = OrgUser::resend_verification(user.id).unwrap() else {
        panic!("expected a fresh token");
    };
    assert_ne!(fresh, token);
    assert!(OrgUser::verify_email(&token).unwrap().is_none());
    assert!(OrgUser::verify_email(&fresh).unwrap().is_some());
    assert_eq!(
        OrgUser::resend_verification(user.id).unwrap(),
        ResendOutcome::NotPending
    );
}

#[test]
fn admin_created_users_need_no_verification_and_org_has_admin_tracks_users() {
    let _db = TestDb::create();
    let org = make_org("Legacy Org");
    assert!(!OrgUser::org_has_admin(org.id).unwrap());
    let dispatcher = OrgUser::create(
        org.id,
        "D",
        "d@example.com",
        "hunter2pw",
        OrgRole::Dispatcher,
    )
    .unwrap();
    assert!(OrgUser::email_verified(dispatcher.id).unwrap());
    assert!(
        !OrgUser::org_has_admin(org.id).unwrap(),
        "a dispatcher is not an admin"
    );
    OrgUser::create(org.id, "A", "a@example.com", "hunter2pw", OrgRole::Admin).unwrap();
    assert!(OrgUser::org_has_admin(org.id).unwrap());
}
