//! The new-organisation setup checklist (`docs/onboarding.md`, section 3).
//!
//! Step completion is *derived* from the org's real data on every call, so it
//! can never disagree with reality and data added through any route ticks the
//! checklist too. The only stored state is `Orgs.onboarding_dismissed`.

use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::dispatch::dispatch;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::user::user::OrgUser;
use mysql::params;
use mysql::prelude::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// A vehicle location counts as "recent" for the `gps_tracker` step when it
/// was updated within this many seconds.
const RECENT_LOCATION_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OnboardingStep {
    /// Stable machine key: `org_location`, `godown`, `stock`, `vehicle`,
    /// `driver`, `driver_assigned`, `customer`, `first_dispatch`,
    /// `invite_team` or `gps_tracker`.
    pub key: String,
    /// Required steps must be done before a first dispatch is possible.
    pub required: bool,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OnboardingStatus {
    /// Required steps first, in dependency order, then the optional ones.
    pub steps: Vec<OnboardingStep>,
    /// Whether an Admin has hidden the checklist.
    pub dismissed: bool,
}

impl Organization {
    /// Add `onboarding_dismissed` to an `Orgs` table that predates onboarding.
    fn ensure_onboarding_column(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
        let present: Option<i64> = conn.exec_first(
            "SELECT 1 FROM information_schema.columns
             WHERE table_schema = DATABASE() AND table_name = 'Orgs'
               AND column_name = 'onboarding_dismissed'",
            (),
        )?;
        if present.is_none() {
            conn.query_drop(
                "ALTER TABLE Orgs ADD COLUMN onboarding_dismissed BOOLEAN NOT NULL DEFAULT FALSE",
            )?;
        }
        Ok(())
    }

    /// Compute the setup checklist for `org_id`.
    pub fn onboarding_status(org_id: Uuid) -> Result<OnboardingStatus, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_tables(&mut conn)?;
        Self::ensure_onboarding_column(&mut conn)?;
        Customer::ensure_table(&mut conn)?;
        dispatch::ensure_tables(&mut conn)?;
        OrgUser::ensure_table(&mut conn)?;

        let id = org_id.to_string();
        let recent_after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            - RECENT_LOCATION_SECS;

        let exists = |conn: &mut mysql::PooledConn, sql: &str| -> Result<bool, Box<dyn Error>> {
            let row: Option<i64> = conn.exec_first(
                sql,
                params! { "org_id" => &id, "recent_after" => recent_after },
            )?;
            Ok(row.is_some())
        };

        let (dismissed, has_location): (bool, bool) = conn
            .exec_first(
                "SELECT onboarding_dismissed, latitude IS NOT NULL AND longitude IS NOT NULL
                 FROM Orgs WHERE id = :org_id",
                params! { "org_id" => &id },
            )?
            .ok_or("Organization not found")?;

        let godown = exists(
            &mut conn,
            "SELECT 1 FROM Godowns WHERE org_id = :org_id LIMIT 1",
        )?;
        let stock = exists(
            &mut conn,
            "SELECT 1 FROM Stock s JOIN Godowns g ON s.godown_id = g.id
             WHERE g.org_id = :org_id AND s.quantity > 0 LIMIT 1",
        )?;
        let vehicle = exists(
            &mut conn,
            "SELECT 1 FROM Vehicle WHERE org_id = :org_id LIMIT 1",
        )?;
        let driver = exists(
            &mut conn,
            "SELECT 1 FROM Drivers WHERE org_id = :org_id LIMIT 1",
        )?;
        let driver_assigned = exists(
            &mut conn,
            "SELECT 1 FROM Vehicle v JOIN Drivers d ON v.assigned_driver_id = d.id
             WHERE v.org_id = :org_id AND d.org_id = :org_id AND d.is_active = TRUE LIMIT 1",
        )?;
        let customer = exists(
            &mut conn,
            "SELECT 1 FROM Customers WHERE org_id = :org_id
               AND latitude IS NOT NULL AND longitude IS NOT NULL LIMIT 1",
        )?;
        let first_dispatch = exists(
            &mut conn,
            "SELECT 1 FROM Dispatches WHERE org_id = :org_id LIMIT 1",
        )?;
        let invite_team = exists(
            &mut conn,
            "SELECT 1 FROM OrgUsers WHERE org_id = :org_id AND role <> 'ADMIN' LIMIT 1",
        )?;
        let gps_tracker = exists(
            &mut conn,
            "SELECT 1 FROM Vehicle WHERE org_id = :org_id AND last_updated_at >= :recent_after LIMIT 1",
        )?;

        let step = |key: &str, required: bool, done: bool| OnboardingStep {
            key: key.to_string(),
            required,
            done,
        };
        Ok(OnboardingStatus {
            steps: vec![
                step("org_location", true, has_location),
                step("godown", true, godown),
                step("stock", true, stock),
                step("vehicle", true, vehicle),
                step("driver", true, driver),
                step("driver_assigned", true, driver_assigned),
                step("customer", true, customer),
                step("first_dispatch", false, first_dispatch),
                step("invite_team", false, invite_team),
                step("gps_tracker", false, gps_tracker),
            ],
            dismissed,
        })
    }

    /// Hide the setup checklist for `org_id`. Idempotent.
    pub fn dismiss_onboarding(org_id: Uuid) -> Result<(), Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_tables(&mut conn)?;
        Self::ensure_onboarding_column(&mut conn)?;
        conn.exec_drop(
            "UPDATE Orgs SET onboarding_dismissed = TRUE WHERE id = :id",
            params! { "id" => org_id.to_string() },
        )?;
        Ok(())
    }
}
