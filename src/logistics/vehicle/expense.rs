//! Per-vehicle running costs — fuel fills, tolls, driver advances and other
//! trip expenses — so the ops report can show what each trip cost. See
//! `docs/vehicle-expenses.md`.
//!
//! An expense belongs to a [`Vehicle`](super::vehicle::Vehicle) (by
//! registration, cascade-deleted with it and the org) and may optionally be
//! tagged to a trip. `trip_id` is a `Trips` id for a multi-stop trip, or the
//! dispatch id for a standalone dispatch (its one-stop trip), matching the id
//! `GET /api/orgs/{id}/trips` returns. Amounts are whole currency units, like
//! invoice amounts. Dates are ISO `YYYY-MM-DD` strings.

use crate::logistics::db::connection::DbConnection;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExpenseType {
    Fuel,
    Toll,
    DriverAdvance,
    Other,
}

impl ExpenseType {
    pub fn as_db(&self) -> &'static str {
        match self {
            ExpenseType::Fuel => "FUEL",
            ExpenseType::Toll => "TOLL",
            ExpenseType::DriverAdvance => "DRIVER_ADVANCE",
            ExpenseType::Other => "OTHER",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "FUEL" => ExpenseType::Fuel,
            "TOLL" => ExpenseType::Toll,
            "DRIVER_ADVANCE" => ExpenseType::DriverAdvance,
            _ => ExpenseType::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct VehicleExpense {
    pub id: Uuid,
    pub org_id: Uuid,
    pub vehicle_registration: String,
    /// The trip this cost belongs to, if any (see the module doc).
    pub trip_id: Option<Uuid>,
    pub expense_type: ExpenseType,
    /// Whole currency units, greater than zero.
    pub amount: i64,
    /// ISO `YYYY-MM-DD`.
    pub incurred_on: String,
    /// Litres filled — only meaningful for `FUEL`.
    pub litres: Option<f64>,
    pub notes: Option<String>,
}

#[derive(Debug)]
pub enum VehicleExpenseError {
    InvalidDate(String),
    NonPositiveAmount,
    NonPositiveLitres,
    Db(Box<dyn Error>),
}

impl fmt::Display for VehicleExpenseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VehicleExpenseError::InvalidDate(s) => {
                write!(f, "'{s}' is not a valid ISO date (expected YYYY-MM-DD)")
            }
            VehicleExpenseError::NonPositiveAmount => write!(f, "amount must be greater than zero"),
            VehicleExpenseError::NonPositiveLitres => write!(f, "litres must be greater than zero"),
            VehicleExpenseError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl Error for VehicleExpenseError {}

impl From<mysql::Error> for VehicleExpenseError {
    fn from(e: mysql::Error) -> Self {
        VehicleExpenseError::Db(Box::new(e))
    }
}

/// Is `s` a real `YYYY-MM-DD` calendar date?
fn valid_iso_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (
        parts[0].parse::<i64>(),
        parts[1].parse::<i64>(),
        parts[2].parse::<i64>(),
    ) else {
        return false;
    };
    if !(1..=12).contains(&m) || d < 1 {
        return false;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    d <= dim[(m - 1) as usize]
}

const SELECT_COLS: &str =
    "id, org_id, vehicle_registration, trip_id, expense_type, amount, incurred_on, litres, notes";

type ExpenseRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    i64,
    String,
    Option<f64>,
    Option<String>,
);

impl VehicleExpense {
    /// Create the `VehicleExpenses` table if it does not exist. Kept in sync
    /// with `test_support::migrate`.
    pub fn ensure_table(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS VehicleExpenses (
                id VARCHAR(36) PRIMARY KEY,
                org_id VARCHAR(36) NOT NULL,
                vehicle_registration VARCHAR(255) NOT NULL,
                trip_id VARCHAR(36) DEFAULT NULL,
                expense_type VARCHAR(20) NOT NULL,
                amount BIGINT NOT NULL,
                incurred_on VARCHAR(10) NOT NULL,
                litres DOUBLE DEFAULT NULL,
                notes TEXT DEFAULT NULL,
                CONSTRAINT fk_vehicleexp_org
                    FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE,
                CONSTRAINT fk_vehicleexp_vehicle
                    FOREIGN KEY (vehicle_registration)
                    REFERENCES Vehicle(registration_number) ON DELETE CASCADE
            )",
        )?;
        Ok(())
    }

    fn hydrate(row: ExpenseRow) -> Self {
        let (id, org_id, vehicle_registration, trip_id, ty, amount, incurred_on, litres, notes) =
            row;
        VehicleExpense {
            id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
            org_id: Uuid::parse_str(&org_id).unwrap_or_else(|_| Uuid::new_v4()),
            vehicle_registration,
            trip_id: trip_id.and_then(|t| Uuid::parse_str(&t).ok()),
            expense_type: ExpenseType::from_db(&ty),
            amount,
            incurred_on,
            litres,
            notes,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        org_id: Uuid,
        vehicle_registration: impl Into<String>,
        trip_id: Option<Uuid>,
        expense_type: ExpenseType,
        amount: i64,
        incurred_on: String,
        litres: Option<f64>,
        notes: Option<String>,
    ) -> Result<Self, VehicleExpenseError> {
        if amount <= 0 {
            return Err(VehicleExpenseError::NonPositiveAmount);
        }
        if litres.is_some_and(|l| l.is_nan() || l <= 0.0) {
            return Err(VehicleExpenseError::NonPositiveLitres);
        }
        if !valid_iso_date(&incurred_on) {
            return Err(VehicleExpenseError::InvalidDate(incurred_on));
        }

        let mut conn = DbConnection::from_env()
            .get_connection()
            .map_err(VehicleExpenseError::Db)?;
        Self::ensure_table(&mut conn).map_err(VehicleExpenseError::Db)?;

        let expense = VehicleExpense {
            id: Uuid::new_v4(),
            org_id,
            vehicle_registration: vehicle_registration.into(),
            trip_id,
            expense_type,
            amount,
            incurred_on,
            litres,
            notes,
        };
        conn.exec_drop(
            "INSERT INTO VehicleExpenses
               (id, org_id, vehicle_registration, trip_id, expense_type, amount,
                incurred_on, litres, notes)
             VALUES
               (:id, :org_id, :reg, :trip_id, :ty, :amount, :on, :litres, :notes)",
            params! {
                "id" => expense.id.to_string(),
                "org_id" => org_id.to_string(),
                "reg" => &expense.vehicle_registration,
                "trip_id" => expense.trip_id.map(|t| t.to_string()),
                "ty" => expense.expense_type.as_db(),
                "amount" => amount,
                "on" => &expense.incurred_on,
                "litres" => litres,
                "notes" => &expense.notes,
            },
        )?;
        Ok(expense)
    }

    pub fn get_by_id(id: Uuid) -> Result<Option<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let row: Option<ExpenseRow> = conn.exec_first(
            format!("SELECT {SELECT_COLS} FROM VehicleExpenses WHERE id = :id"),
            params! { "id" => id.to_string() },
        )?;
        Ok(row.map(Self::hydrate))
    }

    /// One vehicle's expenses, newest first.
    pub fn list_by_vehicle(vehicle_registration: &str) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let rows: Vec<ExpenseRow> = conn.exec(
            format!(
                "SELECT {SELECT_COLS} FROM VehicleExpenses
                 WHERE vehicle_registration = :reg
                 ORDER BY incurred_on DESC, id"
            ),
            params! { "reg" => vehicle_registration },
        )?;
        Ok(rows.into_iter().map(Self::hydrate).collect())
    }

    /// Every expense across the org's fleet, newest first.
    pub fn list_by_org(org_id: Uuid) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let rows: Vec<ExpenseRow> = conn.exec(
            format!(
                "SELECT {SELECT_COLS} FROM VehicleExpenses
                 WHERE org_id = :org_id
                 ORDER BY incurred_on DESC, id"
            ),
            params! { "org_id" => org_id.to_string() },
        )?;
        Ok(rows.into_iter().map(Self::hydrate).collect())
    }

    pub fn delete(&self) -> Result<(), Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        conn.exec_drop(
            "DELETE FROM VehicleExpenses WHERE id = :id",
            params! { "id" => self.id.to_string() },
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::orgs::orgs::Organization;
    use crate::logistics::test_support::TestDb;
    use crate::logistics::vehicle::vehicle::{Unit, Vehicle};

    fn org_with_vehicle(reg: &str) -> Organization {
        let org = Organization::create_organization("Expense Test Org", "1 Depot Road")
            .expect("create org");
        Vehicle::new(reg, 20, Unit::MetricTon)
            .add_new_vehicle_to_org(&org)
            .expect("add vehicle");
        org
    }

    fn make(
        org: &Organization,
        reg: &str,
        ty: ExpenseType,
        amount: i64,
        on: &str,
    ) -> Result<VehicleExpense, VehicleExpenseError> {
        VehicleExpense::create(org.id, reg, None, ty, amount, on.to_string(), None, None)
    }

    #[test]
    fn rejects_bad_amount_litres_and_date() {
        let _db = TestDb::create();
        let org = org_with_vehicle("KA01-EX-0001");
        assert!(matches!(
            make(&org, "KA01-EX-0001", ExpenseType::Toll, 0, "2026-01-01"),
            Err(VehicleExpenseError::NonPositiveAmount)
        ));
        assert!(matches!(
            make(&org, "KA01-EX-0001", ExpenseType::Toll, 10, "2026-02-30"),
            Err(VehicleExpenseError::InvalidDate(_))
        ));
        assert!(matches!(
            VehicleExpense::create(
                org.id,
                "KA01-EX-0001",
                None,
                ExpenseType::Fuel,
                10,
                "2026-01-01".into(),
                Some(0.0),
                None
            ),
            Err(VehicleExpenseError::NonPositiveLitres)
        ));
    }

    #[test]
    fn create_list_newest_first_and_delete() {
        let _db = TestDb::create();
        let org = org_with_vehicle("KA01-EX-0002");
        let old = make(&org, "KA01-EX-0002", ExpenseType::Fuel, 5000, "2026-01-01").unwrap();
        let new = make(
            &org,
            "KA01-EX-0002",
            ExpenseType::DriverAdvance,
            2000,
            "2026-01-05",
        )
        .unwrap();

        let list = VehicleExpense::list_by_vehicle("KA01-EX-0002").unwrap();
        assert_eq!(
            list.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![new.id, old.id]
        );
        assert_eq!(list[0].expense_type, ExpenseType::DriverAdvance);
        assert_eq!(VehicleExpense::list_by_org(org.id).unwrap().len(), 2);

        old.delete().unwrap();
        assert!(VehicleExpense::get_by_id(old.id).unwrap().is_none());
        assert_eq!(VehicleExpense::list_by_org(org.id).unwrap().len(), 1);
    }

    #[test]
    fn cascade_deletes_with_the_vehicle() {
        let _db = TestDb::create();
        let org = org_with_vehicle("KA01-EX-0003");
        let e = make(&org, "KA01-EX-0003", ExpenseType::Toll, 300, "2026-01-01").unwrap();
        let mut conn = DbConnection::from_env().get_connection().unwrap();
        conn.exec_drop(
            "DELETE FROM Vehicle WHERE registration_number = 'KA01-EX-0003'",
            (),
        )
        .unwrap();
        assert!(VehicleExpense::get_by_id(e.id).unwrap().is_none());
    }
}
