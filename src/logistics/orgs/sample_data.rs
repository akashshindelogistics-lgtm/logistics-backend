//! Optional demo data for a new organisation (`docs/onboarding.md`, design 6).
//!
//! [`load`] creates one godown with stock, one vehicle, one driver (assigned
//! to the vehicle) and one customer with a location, so a new user can run a
//! dispatch straight away. Every created entity is recorded in the
//! `SampleData` table (a real tag, not a name convention), and [`remove`]
//! deletes exactly those rows, plus anything hanging off them (the sample
//! godown's stock, dispatches to the sample customer, their invoices and
//! notifications, and trips of the sample vehicle).

use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::driver::driver::Driver;
use crate::logistics::godown::godown::Godown;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::stock::stock::Stock;
use crate::logistics::vehicle::vehicle::{Unit, Vehicle, VehicleType};
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use uuid::Uuid;

const KIND_GODOWN: &str = "godown";
const KIND_VEHICLE: &str = "vehicle";
const KIND_DRIVER: &str = "driver";
const KIND_CUSTOMER: &str = "customer";

/// What [`load`] created or [`remove`] deleted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SampleDataSummary {
    pub godowns: usize,
    pub stock_items: usize,
    pub vehicles: usize,
    pub drivers: usize,
    pub customers: usize,
    /// Dispatches to the sample customer that were deleted (always 0 on load).
    pub dispatches: usize,
}

/// Why a load or remove did not happen.
#[derive(Debug)]
pub enum SampleDataError {
    /// Sample data is already loaded for this org.
    AlreadyLoaded,
    /// Real data still depends on sample data (a dispatch to a real customer
    /// uses the sample vehicle), so removing it would orphan that data.
    InUse(String),
    Db(Box<dyn Error>),
}

impl std::fmt::Display for SampleDataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyLoaded => write!(f, "Sample data is already loaded"),
            Self::InUse(m) => write!(f, "{m}"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl<E: Into<Box<dyn Error>>> From<E> for SampleDataError {
    fn from(e: E) -> Self {
        Self::Db(e.into())
    }
}

/// Create the `SampleData` table if it does not exist. Kept in sync with
/// `test_support::migrate`.
pub fn ensure_table(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS SampleData (
            org_id VARCHAR(36) NOT NULL,
            kind VARCHAR(16) NOT NULL,
            ref_id VARCHAR(255) NOT NULL,
            PRIMARY KEY (org_id, kind, ref_id),
            CONSTRAINT fk_sample_data_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
        )",
    )?;
    Ok(())
}

fn tag(
    conn: &mut mysql::PooledConn,
    org_id: Uuid,
    kind: &str,
    ref_id: &str,
) -> Result<(), Box<dyn Error>> {
    conn.exec_drop(
        "INSERT INTO SampleData (org_id, kind, ref_id) VALUES (:org_id, :kind, :ref_id)",
        params! { "org_id" => org_id.to_string(), "kind" => kind, "ref_id" => ref_id },
    )?;
    Ok(())
}

fn tagged(
    conn: &mut mysql::PooledConn,
    org_id: Uuid,
    kind: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    Ok(conn.exec(
        "SELECT ref_id FROM SampleData WHERE org_id = :org_id AND kind = :kind",
        params! { "org_id" => org_id.to_string(), "kind" => kind },
    )?)
}

/// Whether sample data is currently loaded for the org.
pub fn is_loaded(org_id: Uuid) -> Result<bool, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    Organization::ensure_tables(&mut conn)?;
    ensure_table(&mut conn)?;
    let n: Option<i64> = conn.exec_first(
        "SELECT COUNT(*) FROM SampleData WHERE org_id = :org_id",
        params! { "org_id" => org_id.to_string() },
    )?;
    Ok(n.unwrap_or(0) > 0)
}

/// Load the sample godown, stock, vehicle, driver and customer. Fails with
/// [`SampleDataError::AlreadyLoaded`] if it is already there. If a step fails
/// part-way, what was created is removed again.
pub fn load(org_id: Uuid) -> Result<SampleDataSummary, SampleDataError> {
    if is_loaded(org_id)? {
        return Err(SampleDataError::AlreadyLoaded);
    }
    let result = create_all(org_id);
    if result.is_err() {
        let _ = remove(org_id);
    }
    result
}

fn create_all(org_id: Uuid) -> Result<SampleDataSummary, SampleDataError> {
    let mut conn = DbConnection::from_env().get_connection()?;
    // Registration numbers and licences are globally unique, so derive them
    // from the org id.
    let short = org_id.simple().to_string()[..8].to_uppercase();

    let mut godown = Godown::create(
        org_id,
        "Sample Godown",
        "1 Sample Warehouse Road, Pune",
        Some(1_000_000),
    )?;
    tag(&mut conn, org_id, KIND_GODOWN, &godown.id.to_string())?;
    godown.update_location(18.5204, 73.8567, Some("Pune"))?;
    let items = [
        Stock::new(10, 500, "Sample Rice Bags").with_category("Grains"),
        Stock::new(5, 200, "Sample Cooking Oil").with_category("Groceries"),
    ];
    for item in &items {
        item.add_to_godown(godown.id)?;
    }

    let driver = Driver::create(
        org_id,
        "Sample Driver",
        format!("SAMPLE-LIC-{short}"),
        "+91 90000 00000",
    )?;
    tag(&mut conn, org_id, KIND_DRIVER, &driver.id.to_string())?;

    let reg = format!("SAMPLE-{short}");
    let org = Organization::get_by_id(org_id)?.ok_or("Organization not found")?;
    let mut vehicle = Vehicle::new(&reg, 100_000, Unit::MetricTon).with_type(VehicleType::Truck);
    vehicle.add_new_vehicle_to_org(&org)?;
    tag(&mut conn, org_id, KIND_VEHICLE, &reg)?;
    vehicle.assign_driver(Some(driver.id))?;

    let mut customer =
        Customer::create_customer(org_id, "Sample Customer", "10 Sample Market Road, Mumbai")?;
    tag(&mut conn, org_id, KIND_CUSTOMER, &customer.id.to_string())?;
    customer.update_location(19.0760, 72.8777, Some("Mumbai"))?;

    Ok(SampleDataSummary {
        godowns: 1,
        stock_items: items.len(),
        vehicles: 1,
        drivers: 1,
        customers: 1,
        dispatches: 0,
    })
}

fn table_exists(conn: &mut mysql::PooledConn, name: &str) -> Result<bool, Box<dyn Error>> {
    let n: Option<i64> = conn.exec_first(
        "SELECT 1 FROM information_schema.tables
         WHERE table_schema = DATABASE() AND table_name = :name",
        params! { "name" => name },
    )?;
    Ok(n.is_some())
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// Remove exactly the rows [`load`] tagged, and what depends on them. Does
/// nothing (all-zero summary) when no sample data is loaded. Fails with
/// [`SampleDataError::InUse`], deleting nothing, if a dispatch to a real
/// customer uses the sample vehicle, or stock was transferred out of the
/// sample godown.
pub fn remove(org_id: Uuid) -> Result<SampleDataSummary, SampleDataError> {
    let mut conn = DbConnection::from_env().get_connection()?;
    Organization::ensure_tables(&mut conn)?;
    ensure_table(&mut conn)?;
    let godowns = tagged(&mut conn, org_id, KIND_GODOWN)?;
    let vehicles = tagged(&mut conn, org_id, KIND_VEHICLE)?;
    let drivers = tagged(&mut conn, org_id, KIND_DRIVER)?;
    let customers = tagged(&mut conn, org_id, KIND_CUSTOMER)?;
    let org = org_id.to_string();

    let has_dispatches = table_exists(&mut conn, "Dispatches")?;
    let has_transfers = table_exists(&mut conn, "StockTransfers")?;
    let has_trips = table_exists(&mut conn, "Trips")?;
    let has_notifications = table_exists(&mut conn, "Notifications")?;
    let has_chunks = table_exists(&mut conn, "AiChunks")?;

    // Dispatches to the sample customer are sample data too.
    let mut dispatch_ids: Vec<String> = Vec::new();
    if has_dispatches && !customers.is_empty() {
        let mut args: Vec<Value> = vec![org.clone().into()];
        args.extend(customers.iter().cloned().map(Value::from));
        dispatch_ids = conn.exec(
            format!(
                "SELECT id FROM Dispatches WHERE org_id = ? AND customer_id IN ({})",
                placeholders(customers.len())
            ),
            args,
        )?;
    }

    if has_dispatches && !vehicles.is_empty() {
        let mut args: Vec<Value> = vec![org.clone().into()];
        args.extend(vehicles.iter().cloned().map(Value::from));
        let mut sql = format!(
            "SELECT COUNT(*) FROM Dispatches WHERE org_id = ?
             AND vehicle_registration_number IN ({})",
            placeholders(vehicles.len())
        );
        if !customers.is_empty() {
            sql.push_str(&format!(
                " AND customer_id NOT IN ({})",
                placeholders(customers.len())
            ));
            args.extend(customers.iter().cloned().map(Value::from));
        }
        let n: Option<i64> = conn.exec_first(sql, args)?;
        if n.unwrap_or(0) > 0 {
            return Err(SampleDataError::InUse(
                "A dispatch to one of your own customers uses the sample vehicle; \
                 reassign or delete it first"
                    .into(),
            ));
        }
    }
    if has_transfers && !godowns.is_empty() {
        let mut args: Vec<Value> = vec![org.clone().into()];
        args.extend(godowns.iter().cloned().map(Value::from));
        args.extend(godowns.iter().cloned().map(Value::from));
        let n: Option<i64> = conn.exec_first(
            format!(
                "SELECT COUNT(*) FROM StockTransfers WHERE org_id = ?
                 AND (from_godown_id IN ({0}) OR to_godown_id IN ({0}))",
                placeholders(godowns.len())
            ),
            args,
        )?;
        if n.unwrap_or(0) > 0 {
            return Err(SampleDataError::InUse(
                "Stock was transferred to or from the sample godown; it cannot be removed".into(),
            ));
        }
    }

    let mut stock_items = 0usize;
    for g in &godowns {
        let n: Option<i64> = conn.exec_first(
            "SELECT COUNT(*) FROM Stock WHERE godown_id = :g",
            params! { "g" => g },
        )?;
        stock_items += n.unwrap_or(0) as usize;
    }

    let mut tx = conn.start_transaction(TxOpts::default())?;
    if !dispatch_ids.is_empty() {
        let ph = placeholders(dispatch_ids.len());
        let args = || {
            dispatch_ids
                .iter()
                .cloned()
                .map(Value::from)
                .collect::<Vec<_>>()
        };
        if has_notifications {
            tx.exec_drop(
                format!("DELETE FROM Notifications WHERE dispatch_id IN ({ph})"),
                args(),
            )?;
        }
        // Line items, status history, proof of delivery and invoices cascade.
        tx.exec_drop(format!("DELETE FROM Dispatches WHERE id IN ({ph})"), args())?;
    }
    if has_trips && !vehicles.is_empty() {
        let mut args: Vec<Value> = vec![org.clone().into()];
        args.extend(vehicles.iter().cloned().map(Value::from));
        tx.exec_drop(
            format!(
                "DELETE FROM Trips WHERE org_id = ? AND vehicle_registration_number IN ({})",
                placeholders(vehicles.len())
            ),
            args,
        )?;
    }
    // Vehicle documents and maintenance cascade from the vehicle, stock from
    // the godown.
    for (table, col, ids) in [
        ("Vehicle", "registration_number", &vehicles),
        ("Drivers", "id", &drivers),
        ("Godowns", "id", &godowns),
        ("Customers", "id", &customers),
    ] {
        for id in ids {
            tx.exec_drop(
                format!("DELETE FROM {table} WHERE {col} = :id AND org_id = :org"),
                params! { "id" => id, "org" => &org },
            )?;
        }
    }
    tx.exec_drop(
        "DELETE FROM SampleData WHERE org_id = :org",
        params! { "org" => &org },
    )?;
    tx.commit()?;

    if has_chunks {
        // Search-index rows are best effort, as everywhere else.
        for id in godowns
            .iter()
            .chain(&vehicles)
            .chain(&customers)
            .chain(&dispatch_ids)
        {
            if let Err(e) = conn.exec_drop(
                "DELETE FROM AiChunks WHERE org_id = :org AND source_id LIKE :pat",
                params! { "org" => &org, "pat" => format!("%{id}%") },
            ) {
                eprintln!("ai chunk index: failed to remove sample chunks for {id}: {e}");
            }
        }
    }

    Ok(SampleDataSummary {
        godowns: godowns.len(),
        stock_items,
        vehicles: vehicles.len(),
        drivers: drivers.len(),
        customers: customers.len(),
        dispatches: dispatch_ids.len(),
    })
}
