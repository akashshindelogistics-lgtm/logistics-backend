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
/// One row `(org_id, "loaded", "")` per org, inserted first by [`load`]. Its
/// primary key makes two concurrent loads for one org mutually exclusive.
const KIND_MARKER: &str = "loaded";

/// The sample stock lines: (volume per unit, quantity, description, category).
const SAMPLE_STOCK: [(i64, i64, &str, &str); 2] = [
    (10, 500, "Sample Rice Bags", "Grains"),
    (5, 200, "Sample Cooking Oil", "Groceries"),
];

/// Ids of sample entities: the tagged ones on remove, the ones this request
/// created on a failed load.
#[derive(Debug, Default)]
struct SampleIds {
    godowns: Vec<String>,
    vehicles: Vec<String>,
    drivers: Vec<String>,
    customers: Vec<String>,
}

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
/// [`SampleDataError::AlreadyLoaded`] if it is already there, or is being
/// loaded by a concurrent request. If a step fails part-way, only what this
/// request created is removed again.
pub fn load(org_id: Uuid) -> Result<SampleDataSummary, SampleDataError> {
    let mut conn = DbConnection::from_env().get_connection()?;
    Organization::ensure_tables(&mut conn)?;
    ensure_table(&mut conn)?;
    match conn.exec_drop(
        "INSERT INTO SampleData (org_id, kind, ref_id) VALUES (:org_id, :kind, '')",
        params! { "org_id" => org_id.to_string(), "kind" => KIND_MARKER },
    ) {
        Ok(()) => {}
        Err(mysql::Error::MySqlError(ref e)) if e.code == 1062 => {
            return Err(SampleDataError::AlreadyLoaded);
        }
        Err(e) => return Err(e.into()),
    }
    let mut created = SampleIds::default();
    let result = create_all(org_id, &mut created);
    if result.is_err()
        && let Err(e) = rollback(org_id, &created)
    {
        eprintln!("sample data: failed to roll back a failed load for {org_id}: {e}");
    }
    result
}

/// Undo a failed [`load`]: delete exactly the entities it created, their
/// tags and the marker, leaving any other sample rows alone.
fn rollback(org_id: Uuid, created: &SampleIds) -> Result<(), Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    let org = org_id.to_string();
    let mut tx = conn.start_transaction(TxOpts::default())?;
    delete_entities(&mut tx, &org, created)?;
    tx.exec_drop(
        "DELETE FROM SampleData WHERE org_id = :org AND kind = :kind",
        params! { "org" => &org, "kind" => KIND_MARKER },
    )?;
    tx.commit()?;
    Ok(())
}

/// Delete the given sample vehicles, drivers, godowns and customers and their
/// tags. Vehicle documents and maintenance cascade from the vehicle, stock
/// from the godown. Like `Driver::delete`, a vehicle still assigned a sample
/// driver is unassigned first, as there is no foreign key to do it.
fn delete_entities(
    tx: &mut Transaction<'_>,
    org: &str,
    ids: &SampleIds,
) -> Result<(), Box<dyn Error>> {
    if !ids.drivers.is_empty() {
        let mut args: Vec<Value> = vec![org.to_string().into()];
        args.extend(ids.drivers.iter().cloned().map(Value::from));
        tx.exec_drop(
            format!(
                "UPDATE Vehicle SET assigned_driver_id = NULL
                 WHERE org_id = ? AND assigned_driver_id IN ({})",
                placeholders(ids.drivers.len())
            ),
            args,
        )?;
    }
    for (kind, table, col, list) in [
        (
            KIND_VEHICLE,
            "Vehicle",
            "registration_number",
            &ids.vehicles,
        ),
        (KIND_DRIVER, "Drivers", "id", &ids.drivers),
        (KIND_GODOWN, "Godowns", "id", &ids.godowns),
        (KIND_CUSTOMER, "Customers", "id", &ids.customers),
    ] {
        for id in list {
            tx.exec_drop(
                format!("DELETE FROM {table} WHERE {col} = :id AND org_id = :org"),
                params! { "id" => id, "org" => org },
            )?;
            tx.exec_drop(
                "DELETE FROM SampleData WHERE org_id = :org AND kind = :kind AND ref_id = :id",
                params! { "org" => org, "kind" => kind, "id" => id },
            )?;
        }
    }
    Ok(())
}

fn create_all(org_id: Uuid, created: &mut SampleIds) -> Result<SampleDataSummary, SampleDataError> {
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
    created.godowns.push(godown.id.to_string());
    tag(&mut conn, org_id, KIND_GODOWN, &godown.id.to_string())?;
    godown.update_location(18.5204, 73.8567, Some("Pune"))?;
    for (volume, quantity, description, category) in SAMPLE_STOCK {
        Stock::new(volume, quantity, description)
            .with_category(category)
            .add_to_godown(godown.id)?;
    }

    let driver = Driver::create(
        org_id,
        "Sample Driver",
        format!("SAMPLE-LIC-{short}"),
        "+91 90000 00000",
    )?;
    created.drivers.push(driver.id.to_string());
    tag(&mut conn, org_id, KIND_DRIVER, &driver.id.to_string())?;

    let reg = format!("SAMPLE-{short}");
    let org = Organization::get_by_id(org_id)?.ok_or("Organization not found")?;
    // Tag before creating: `add_new_vehicle_to_org` is an upsert, so only a
    // tag this request inserted shows that the vehicle row is its own.
    tag(&mut conn, org_id, KIND_VEHICLE, &reg)?;
    created.vehicles.push(reg.clone());
    let mut vehicle = Vehicle::new(&reg, 100_000, Unit::MetricTon).with_type(VehicleType::Truck);
    vehicle.add_new_vehicle_to_org(&org)?;
    vehicle.assign_driver(Some(driver.id))?;

    let mut customer =
        Customer::create_customer(org_id, "Sample Customer", "10 Sample Market Road, Mumbai")?;
    created.customers.push(customer.id.to_string());
    tag(&mut conn, org_id, KIND_CUSTOMER, &customer.id.to_string())?;
    customer.update_location(19.0760, 72.8777, Some("Mumbai"))?;

    Ok(SampleDataSummary {
        godowns: 1,
        stock_items: SAMPLE_STOCK.len(),
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
/// customer uses the sample vehicle, stock was transferred to or from the
/// sample godown, or a dispatch to the sample customer carries non-sample
/// stock, a hired vehicle or an invoice. A real vehicle the sample driver was
/// assigned to is unassigned.
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
    let has_pod_checks = table_exists(&mut conn, "PodPhotoChecks")?;
    let has_chunks = table_exists(&mut conn, "AiChunks")?;
    let has_line_items = table_exists(&mut conn, "DispatchLineItems")?;
    let has_invoices = table_exists(&mut conn, "Invoices")?;

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
    // Only delete them if they are sample data through and through: no stock
    // from a real godown (deleting it would leave that stock drawn down for
    // good), no vehicle hire or vendor payments, no invoice.
    if !dispatch_ids.is_empty() {
        let mut args: Vec<Value> = dispatch_ids.iter().cloned().map(Value::from).collect();
        let mut sql = format!(
            "SELECT COUNT(*) FROM Dispatches d WHERE d.id IN ({}) AND (d.hire_id IS NOT NULL",
            placeholders(dispatch_ids.len())
        );
        if has_line_items {
            sql.push_str(&format!(
                " OR EXISTS (SELECT 1 FROM DispatchLineItems li WHERE li.dispatch_id = d.id
                   AND li.stock_description NOT IN ({}))",
                placeholders(SAMPLE_STOCK.len())
            ));
            args.extend(SAMPLE_STOCK.iter().map(|s| Value::from(s.2.to_string())));
        }
        if has_invoices {
            sql.push_str(" OR EXISTS (SELECT 1 FROM Invoices i WHERE i.dispatch_id = d.id)");
        }
        sql.push(')');
        let n: Option<i64> = conn.exec_first(sql, args)?;
        if n.unwrap_or(0) > 0 {
            return Err(SampleDataError::InUse(
                "A dispatch to the sample customer carries your own stock, a hired vehicle \
                 or an invoice; cancel or delete it first"
                    .into(),
            ));
        }
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
        // Photo-check outcomes have no foreign key either; left behind they
        // keep the deleted dispatches on the review list.
        if has_pod_checks {
            tx.exec_drop(
                format!("DELETE FROM PodPhotoChecks WHERE dispatch_id IN ({ph})"),
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
    let ids = SampleIds {
        godowns,
        vehicles,
        drivers,
        customers,
    };
    delete_entities(&mut tx, &org, &ids)?;
    tx.exec_drop(
        "DELETE FROM SampleData WHERE org_id = :org",
        params! { "org" => &org },
    )?;
    tx.commit()?;

    if has_chunks {
        // Search-index rows are best effort, as everywhere else.
        for id in ids
            .godowns
            .iter()
            .chain(&ids.vehicles)
            .chain(&ids.customers)
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
        godowns: ids.godowns.len(),
        stock_items,
        vehicles: ids.vehicles.len(),
        drivers: ids.drivers.len(),
        customers: ids.customers.len(),
        dispatches: dispatch_ids.len(),
    })
}
