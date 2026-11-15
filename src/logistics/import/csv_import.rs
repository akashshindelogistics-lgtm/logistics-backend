//! Bulk CSV import of vehicles, drivers, customers and stock (see
//! `docs/data-import.md`). One uploaded file is one kind of record.
//!
//! The flow is always the same: parse the CSV, validate **every** row (shape,
//! duplicates, references to existing godowns/drivers) and collect per-line
//! errors. A dry run stops there. A real import only writes when there are no
//! errors, and writes the whole file in one transaction, so a failure never
//! leaves half a fleet behind.

use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::driver::driver::Driver;
use crate::logistics::godown::godown::Godown;
use crate::logistics::gst::normalize_gstin;
use crate::logistics::stock::stock::Stock;
use crate::logistics::vehicle::vehicle::{Unit, Vehicle, VehicleType, ensure_tracker_key_column};
use mysql::prelude::*;
use mysql::{TxOpts, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use uuid::Uuid;

/// Most data rows one file may hold.
pub const MAX_IMPORT_ROWS: usize = 1000;
/// Largest CSV accepted, in bytes (1 MiB).
pub const MAX_IMPORT_BYTES: usize = 1024 * 1024;

const UNITS: [&str; 6] = ["MetricTon", "Kg", "Litre", "Box", "Pallet", "Piece"];
const VEHICLE_TYPES: [&str; 6] = ["Truck", "Tipper", "Trailer", "Tempo", "Pickup", "Tanker"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    Vehicles,
    Drivers,
    Customers,
    Stock,
}

impl ImportKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vehicles" => Some(Self::Vehicles),
            "drivers" => Some(Self::Drivers),
            "customers" => Some(Self::Customers),
            "stock" => Some(Self::Stock),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vehicles => "vehicles",
            Self::Drivers => "drivers",
            Self::Customers => "customers",
            Self::Stock => "stock",
        }
    }

    /// Column names, in template order.
    pub fn columns(&self) -> &'static [&'static str] {
        match self {
            Self::Vehicles => &[
                "registration_number",
                "capacity",
                "unit",
                "vehicle_type",
                "driver_license_number",
            ],
            Self::Drivers => &["name", "license_number", "phone"],
            Self::Customers => &["name", "address", "phone", "email", "gstin"],
            Self::Stock => &[
                "godown_name",
                "description",
                "category",
                "quantity",
                "volume_in_size",
                "reorder_threshold",
            ],
        }
    }

    /// The downloadable template: the header row only, so it imports cleanly
    /// (zero rows) and never carries sample data into an org.
    pub fn template(&self) -> String {
        format!("{}\r\n", self.columns().join(","))
    }
}

/// One problem found in the file. `line` is the 1-based line of the file the
/// row starts on (the header is line 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RowError {
    pub line: usize,
    pub column: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ImportReport {
    pub kind: String,
    pub dry_run: bool,
    /// Data rows in the file (blank lines excluded).
    pub total_rows: usize,
    /// Rows with no errors.
    pub valid_rows: usize,
    /// Rows written. `0` on a dry run or when any row had errors.
    pub imported: usize,
    pub errors: Vec<RowError>,
}

/// A file-level problem (not tied to a row): unreadable, wrong header, too
/// big. Reported as `400`.
#[derive(Debug, PartialEq, Eq)]
pub struct FileError(pub String);

// ---------------------------------------------------------------------------
// CSV parsing
// ---------------------------------------------------------------------------

struct Record {
    line: usize,
    fields: Vec<String>,
}

/// Minimal RFC 4180 reader: quoted fields, doubled quotes, CRLF or LF, an
/// optional UTF-8 BOM. Blank lines are skipped. Each record remembers the file
/// line it starts on.
fn parse_csv(text: &str) -> Result<Vec<Record>, FileError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut was_quoted = false;
    let mut line = 1;
    let mut record_line = 1;
    let mut chars = text.chars().peekable();

    let finish_record = |fields: &mut Vec<String>,
                         field: &mut String,
                         was_quoted: &mut bool,
                         line: usize,
                         records: &mut Vec<Record>| {
        fields.push(std::mem::take(field));
        let blank = fields.len() == 1 && fields[0].is_empty() && !*was_quoted;
        if !blank {
            records.push(Record {
                line,
                fields: std::mem::take(fields),
            });
        } else {
            fields.clear();
        }
        *was_quoted = false;
    };

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_quotes = false,
                '\n' => {
                    line += 1;
                    field.push('\n');
                }
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => {
                in_quotes = true;
                was_quoted = true;
            }
            ',' => fields.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                finish_record(
                    &mut fields,
                    &mut field,
                    &mut was_quoted,
                    record_line,
                    &mut records,
                );
                line += 1;
                record_line = line;
            }
            _ => field.push(c),
        }
    }
    if in_quotes {
        return Err(FileError(format!(
            "Unterminated quoted field starting near line {record_line}"
        )));
    }
    if !field.is_empty() || !fields.is_empty() {
        finish_record(
            &mut fields,
            &mut field,
            &mut was_quoted,
            record_line,
            &mut records,
        );
    }
    Ok(records)
}

/// Trim a cell and undo the export's spreadsheet-formula guard (a leading `'`
/// before `= + - @`), so an exported file can be imported back.
fn clean(cell: &str) -> String {
    let t = cell.trim();
    if let Some(rest) = t.strip_prefix('\'')
        && rest.starts_with(['=', '+', '-', '@'])
    {
        return rest.to_string();
    }
    t.to_string()
}

/// A row keyed by column name.
#[derive(Debug)]
struct Row {
    line: usize,
    cells: HashMap<&'static str, String>,
}

impl Row {
    fn get(&self, col: &str) -> &str {
        self.cells.get(col).map(String::as_str).unwrap_or("")
    }
}

fn read_rows(kind: ImportKind, bytes: &[u8]) -> Result<Vec<Row>, FileError> {
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(FileError(format!(
            "File is {} bytes, which exceeds the {MAX_IMPORT_BYTES}-byte limit",
            bytes.len()
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| FileError("File is not valid UTF-8 text".to_string()))?;
    let mut records = parse_csv(text)?;
    if records.is_empty() {
        return Err(FileError(
            "File is empty: expected a header row".to_string(),
        ));
    }
    let header: Vec<String> = records
        .remove(0)
        .fields
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    let cols = kind.columns();
    let missing: Vec<&str> = cols
        .iter()
        .copied()
        .filter(|c| !header.iter().any(|h| h == c))
        .collect();
    // The last column(s) that are optional per kind may be omitted entirely.
    let missing: Vec<&str> = missing
        .into_iter()
        .filter(|c| !optional_columns(kind).contains(c))
        .collect();
    if !missing.is_empty() {
        return Err(FileError(format!(
            "Missing column(s): {}. Expected header: {}",
            missing.join(", "),
            cols.join(",")
        )));
    }
    if records.len() > MAX_IMPORT_ROWS {
        return Err(FileError(format!(
            "File has {} data rows; the limit is {MAX_IMPORT_ROWS} per file",
            records.len()
        )));
    }
    Ok(records
        .into_iter()
        .map(|rec| {
            let mut cells = HashMap::new();
            for (i, name) in header.iter().enumerate() {
                if let Some(col) = cols.iter().find(|c| *c == name) {
                    cells.insert(
                        *col,
                        clean(rec.fields.get(i).map(String::as_str).unwrap_or("")),
                    );
                }
            }
            Row {
                line: rec.line,
                cells,
            }
        })
        .collect())
}

fn optional_columns(kind: ImportKind) -> &'static [&'static str] {
    match kind {
        ImportKind::Vehicles => &["unit", "vehicle_type", "driver_license_number"],
        ImportKind::Drivers => &[],
        ImportKind::Customers => &["phone", "email", "gstin"],
        ImportKind::Stock => &["category", "volume_in_size", "reorder_threshold"],
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

struct Errors(Vec<RowError>);

impl Errors {
    fn add(&mut self, line: usize, column: &str, message: impl Into<String>) {
        self.0.push(RowError {
            line,
            column: Some(column.to_string()),
            message: message.into(),
        });
    }
}

/// Widest value most text columns hold (`VARCHAR(255)`).
const MAX_TEXT: usize = 255;
/// `Drivers.phone` is `VARCHAR(64)`.
const MAX_DRIVER_PHONE: usize = 64;

/// `false` (and a row error) when `v` has more than `max` characters, the
/// way MySQL counts a `VARCHAR(max)`.
fn fits(row: &Row, col: &str, v: &str, max: usize, errs: &mut Errors) -> bool {
    if v.chars().count() > max {
        errs.add(
            row.line,
            col,
            format!("{col} is longer than {max} characters"),
        );
        false
    } else {
        true
    }
}

fn required(row: &Row, col: &str, max: usize, errs: &mut Errors) -> Option<String> {
    let v = row.get(col);
    if v.is_empty() {
        errs.add(row.line, col, format!("{col} is required"));
        None
    } else if !fits(row, col, v, max, errs) {
        None
    } else {
        Some(v.to_string())
    }
}

/// An optional text cell: `None` when blank. A value over `max` characters
/// is a row error (and also `None`).
fn optional(row: &Row, col: &str, max: usize, errs: &mut Errors) -> Option<String> {
    let v = row.get(col);
    if v.is_empty() || !fits(row, col, v, max, errs) {
        None
    } else {
        Some(v.to_string())
    }
}

fn int(row: &Row, col: &str, min: i64, errs: &mut Errors) -> Option<i64> {
    match row.get(col).parse::<i64>() {
        Ok(n) if n >= min => Some(n),
        Ok(_) => {
            errs.add(row.line, col, format!("{col} must be at least {min}"));
            None
        }
        Err(_) => {
            errs.add(row.line, col, format!("{col} must be a whole number"));
            None
        }
    }
}

fn optional_int(row: &Row, col: &str, min: i64, errs: &mut Errors) -> Option<Option<i64>> {
    if row.get(col).is_empty() {
        Some(None)
    } else {
        int(row, col, min, errs).map(Some)
    }
}

fn one_of(
    row: &Row,
    col: &str,
    allowed: &[&'static str],
    errs: &mut Errors,
) -> Option<&'static str> {
    let v = row.get(col);
    match allowed.iter().find(|a| a.eq_ignore_ascii_case(v)) {
        Some(a) => Some(*a),
        None => {
            errs.add(
                row.line,
                col,
                format!("{col} '{v}' is not one of {}", allowed.join(", ")),
            );
            None
        }
    }
}

struct VehicleRow {
    registration_number: String,
    capacity: i64,
    unit: Unit,
    vehicle_type: VehicleType,
    driver_id: Option<Uuid>,
}

struct DriverRow {
    name: String,
    license_number: String,
    phone: String,
}

struct CustomerRow {
    name: String,
    address: String,
    phone: Option<String>,
    email: Option<String>,
    gstin: Option<String>,
}

struct StockRow {
    godown_id: Uuid,
    description: String,
    category: String,
    quantity: i64,
    volume_in_size: i64,
    reorder_threshold: Option<i64>,
}

enum Parsed {
    Vehicles(Vec<VehicleRow>),
    Drivers(Vec<DriverRow>),
    Customers(Vec<CustomerRow>),
    Stock(Vec<StockRow>),
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

fn validate_vehicles(
    org_id: Uuid,
    rows: &[Row],
    errs: &mut Errors,
) -> Result<Vec<VehicleRow>, Box<dyn Error>> {
    // Licences are not unique in `Drivers`, so keep every match and refuse
    // to guess when there is more than one.
    let mut drivers: HashMap<String, Vec<Uuid>> = HashMap::new();
    for d in Driver::list_by_org(org_id)? {
        drivers
            .entry(norm(&d.license_number))
            .or_default()
            .push(d.id);
    }
    // Drivers already on a vehicle; one driver drives one vehicle.
    let taken: HashSet<Uuid> = Vehicle::list_by_org(org_id)?
        .into_iter()
        .filter_map(|v| v.assigned_driver_id)
        .collect();
    let existing: HashSet<String> = {
        // Registration numbers are unique across the whole system.
        let mut conn = DbConnection::from_env().get_connection()?;
        let regs: Vec<String> = conn.query("SELECT registration_number FROM Vehicle")?;
        regs.iter().map(|r| norm(r)).collect()
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut used_drivers: HashSet<Uuid> = HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let before = errs.0.len();
        let reg = required(row, "registration_number", MAX_TEXT, errs);
        let capacity = int(row, "capacity", 1, errs);
        let unit = if row.get("unit").is_empty() {
            Some("MetricTon")
        } else {
            one_of(row, "unit", &UNITS, errs)
        };
        let vtype = if row.get("vehicle_type").is_empty() {
            Some("Truck")
        } else {
            one_of(row, "vehicle_type", &VEHICLE_TYPES, errs)
        };
        let mut driver_id = None;
        let lic = row.get("driver_license_number");
        if !lic.is_empty() {
            match drivers.get(&norm(lic)).map(Vec::as_slice) {
                None | Some([]) => errs.add(
                    row.line,
                    "driver_license_number",
                    format!("no driver with licence number '{lic}' in this organization"),
                ),
                Some([id]) if taken.contains(id) || !used_drivers.insert(*id) => errs.add(
                    row.line,
                    "driver_license_number",
                    format!("driver '{lic}' is already assigned to a vehicle"),
                ),
                Some([id]) => driver_id = Some(*id),
                Some(_) => errs.add(
                    row.line,
                    "driver_license_number",
                    format!("more than one driver has licence number '{lic}'"),
                ),
            }
        }
        if let Some(reg) = &reg {
            if existing.contains(&norm(reg)) {
                errs.add(
                    row.line,
                    "registration_number",
                    format!("vehicle '{reg}' already exists"),
                );
            } else if !seen.insert(norm(reg)) {
                errs.add(
                    row.line,
                    "registration_number",
                    format!("'{reg}' appears more than once in the file"),
                );
            }
        }
        if errs.0.len() == before
            && let (Some(reg), Some(capacity), Some(unit), Some(vtype)) =
                (reg, capacity, unit, vtype)
        {
            out.push(VehicleRow {
                registration_number: reg,
                capacity,
                unit: Unit::from_str(unit),
                vehicle_type: VehicleType::from_str(vtype),
                driver_id,
            });
        }
    }
    Ok(out)
}

fn validate_drivers(
    org_id: Uuid,
    rows: &[Row],
    errs: &mut Errors,
) -> Result<Vec<DriverRow>, Box<dyn Error>> {
    let mut seen: HashSet<String> = Driver::list_by_org(org_id)?
        .into_iter()
        .map(|d| norm(&d.license_number))
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let before = errs.0.len();
        let name = required(row, "name", MAX_TEXT, errs);
        let lic = required(row, "license_number", MAX_TEXT, errs);
        let phone = required(row, "phone", MAX_DRIVER_PHONE, errs);
        if let Some(l) = &lic
            && !seen.insert(norm(l))
        {
            errs.add(
                row.line,
                "license_number",
                format!("licence number '{l}' already exists or appears more than once"),
            );
        }
        if errs.0.len() == before
            && let (Some(name), Some(license_number), Some(phone)) = (name, lic, phone)
        {
            out.push(DriverRow {
                name,
                license_number,
                phone,
            });
        }
    }
    Ok(out)
}

fn validate_customers(
    org_id: Uuid,
    rows: &[Row],
    errs: &mut Errors,
) -> Result<Vec<CustomerRow>, Box<dyn Error>> {
    let mut seen: HashSet<(String, String)> = Customer::list_by_org(org_id)?
        .into_iter()
        .map(|c| (norm(&c.name), norm(&c.address)))
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let before = errs.0.len();
        let name = required(row, "name", MAX_TEXT, errs);
        let address = required(row, "address", MAX_TEXT, errs);
        let phone = optional(row, "phone", MAX_TEXT, errs);
        let email = optional(row, "email", MAX_TEXT, errs);
        if let Some(e) = &email
            && (!e.contains('@') || e.contains(char::is_whitespace))
        {
            errs.add(
                row.line,
                "email",
                format!("'{e}' is not a valid email address"),
            );
        }
        let gstin_cell = Some(row.get("gstin").to_string()).filter(|v| !v.is_empty());
        let gstin = match normalize_gstin(gstin_cell) {
            Ok(g) => g,
            Err(msg) => {
                errs.add(row.line, "gstin", msg);
                None
            }
        };
        if let (Some(n), Some(a)) = (&name, &address)
            && !seen.insert((norm(n), norm(a)))
        {
            errs.add(
                row.line,
                "name",
                format!("customer '{n}' at this address already exists or appears more than once"),
            );
        }
        if errs.0.len() == before
            && let (Some(name), Some(address)) = (name, address)
        {
            out.push(CustomerRow {
                name,
                address,
                phone,
                email,
                gstin,
            });
        }
    }
    Ok(out)
}

fn validate_stock(
    org_id: Uuid,
    rows: &[Row],
    errs: &mut Errors,
) -> Result<Vec<StockRow>, Box<dyn Error>> {
    let mut godowns: HashMap<String, Vec<Godown>> = HashMap::new();
    for g in Godown::list_by_org(org_id)? {
        godowns.entry(norm(&g.name)).or_default().push(g);
    }
    // Volume held per godown, starting from what is stored today.
    let mut used: HashMap<Uuid, i64> = HashMap::new();
    let mut descriptions: HashSet<(Uuid, String)> = HashSet::new();
    for gs in godowns.values() {
        for g in gs {
            used.insert(g.id, g.used_capacity());
            for s in &g.stock {
                descriptions.insert((g.id, norm(&s.description)));
            }
        }
    }
    let mut out = Vec::new();
    for row in rows {
        let before = errs.0.len();
        let godown_name = required(row, "godown_name", MAX_TEXT, errs);
        let description = required(row, "description", MAX_TEXT, errs);
        let quantity = int(row, "quantity", 0, errs);
        let volume = if row.get("volume_in_size").is_empty() {
            Some(1)
        } else {
            int(row, "volume_in_size", 0, errs)
        };
        let threshold = optional_int(row, "reorder_threshold", 0, errs);
        let category = optional(row, "category", MAX_TEXT, errs)
            .unwrap_or_else(crate::logistics::stock::stock::default_category);

        let mut godown: Option<&Godown> = None;
        if let Some(name) = &godown_name {
            match godowns.get(&norm(name)).map(Vec::as_slice) {
                None | Some([]) => errs.add(
                    row.line,
                    "godown_name",
                    format!("no godown named '{name}' in this organization"),
                ),
                Some([g]) => godown = Some(g),
                Some(_) => errs.add(
                    row.line,
                    "godown_name",
                    format!("more than one godown is named '{name}'"),
                ),
            }
        }
        if let (Some(g), Some(d)) = (godown, &description)
            && !descriptions.insert((g.id, norm(d)))
        {
            errs.add(
                row.line,
                "description",
                format!(
                    "'{d}' is already in godown '{}' or appears more than once",
                    g.name
                ),
            );
        }
        if let (Some(g), Some(q), Some(v)) = (godown, quantity, volume)
            && errs.0.len() == before
        {
            let total = used.entry(g.id).or_insert(0);
            let projected = total.saturating_add(v.saturating_mul(q));
            match g.max_capacity {
                Some(limit) if projected > limit => errs.add(
                    row.line,
                    "quantity",
                    format!(
                        "godown '{}' capacity exceeded: max_capacity is {limit}, this row would bring it to {projected}",
                        g.name
                    ),
                ),
                _ => *total = projected,
            }
        }
        if errs.0.len() == before
            && let (
                Some(g),
                Some(description),
                Some(quantity),
                Some(volume_in_size),
                Some(reorder_threshold),
            ) = (godown, description, quantity, volume, threshold)
        {
            out.push(StockRow {
                godown_id: g.id,
                description,
                category,
                quantity,
                volume_in_size,
                reorder_threshold,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn write_all(org_id: Uuid, parsed: &Parsed) -> Result<usize, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    // DDL auto-commits in MySQL, so make sure the tables exist first.
    match parsed {
        Parsed::Vehicles(_) => {
            ensure_vehicle_table(&mut conn)?;
        }
        Parsed::Drivers(_) => Driver::ensure_table(&mut conn)?,
        Parsed::Customers(_) => Customer::ensure_table(&mut conn)?,
        Parsed::Stock(_) => Stock::ensure_table(&mut conn)?,
    }
    let mut tx = conn.start_transaction(TxOpts::default())?;
    let count = match parsed {
        Parsed::Vehicles(rows) => {
            for r in rows {
                tx.exec_drop(
                    "INSERT INTO Vehicle (registration_number, capacity, unit, org_id, assigned_driver_id, tracker_key, vehicle_type)
                     VALUES (:reg, :capacity, :unit, :org_id, :driver, :tracker_key, :vehicle_type)",
                    params! {
                        "reg" => &r.registration_number,
                        "capacity" => r.capacity,
                        "unit" => r.unit.as_str(),
                        "org_id" => org_id.to_string(),
                        "driver" => r.driver_id.map(|d| d.to_string()),
                        "tracker_key" => Uuid::new_v4().to_string(),
                        "vehicle_type" => r.vehicle_type.as_str(),
                    },
                )?;
            }
            rows.len()
        }
        Parsed::Drivers(rows) => {
            for r in rows {
                tx.exec_drop(
                    "INSERT INTO Drivers (id, org_id, name, license_number, phone, is_active)
                     VALUES (:id, :org_id, :name, :license_number, :phone, TRUE)",
                    params! {
                        "id" => Uuid::new_v4().to_string(),
                        "org_id" => org_id.to_string(),
                        "name" => &r.name,
                        "license_number" => &r.license_number,
                        "phone" => &r.phone,
                    },
                )?;
            }
            rows.len()
        }
        Parsed::Customers(rows) => {
            for r in rows {
                tx.exec_drop(
                    "INSERT INTO Customers (id, org_id, name, address, phone, email, gstin)
                     VALUES (:id, :org_id, :name, :address, :phone, :email, :gstin)",
                    params! {
                        "id" => Uuid::new_v4().to_string(),
                        "org_id" => org_id.to_string(),
                        "name" => &r.name,
                        "address" => &r.address,
                        "phone" => &r.phone,
                        "email" => &r.email,
                        "gstin" => &r.gstin,
                    },
                )?;
            }
            rows.len()
        }
        Parsed::Stock(rows) => {
            for r in rows {
                tx.exec_drop(
                    "INSERT INTO Stock (volume_in_size, quantity, description, category, reorder_threshold, godown_id)
                     VALUES (:volume_in_size, :quantity, :description, :category, :reorder_threshold, :godown_id)",
                    params! {
                        "volume_in_size" => r.volume_in_size,
                        "quantity" => r.quantity,
                        "description" => &r.description,
                        "category" => &r.category,
                        "reorder_threshold" => r.reorder_threshold,
                        "godown_id" => r.godown_id.to_string(),
                    },
                )?;
            }
            rows.len()
        }
    };
    tx.commit()?;
    Ok(count)
}

/// Bring the `Vehicle` table (and its later-added columns) into existence
/// without inserting anything: registering and removing nothing, just DDL.
fn ensure_vehicle_table(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS Vehicle (
            registration_number VARCHAR(255) PRIMARY KEY,
            capacity BIGINT NOT NULL,
            unit VARCHAR(50) NOT NULL,
            org_id VARCHAR(36) NOT NULL,
            assigned_driver_id VARCHAR(36) DEFAULT NULL,
            latitude DOUBLE DEFAULT NULL,
            longitude DOUBLE DEFAULT NULL,
            last_updated_at BIGINT DEFAULT NULL,
            location_address VARCHAR(255) DEFAULT NULL,
            tracker_key VARCHAR(36) DEFAULT NULL,
            vehicle_type VARCHAR(20) NOT NULL DEFAULT 'Truck',
            location_source VARCHAR(20) DEFAULT NULL,
            CONSTRAINT fk_vehicle_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
        )",
    )?;
    ensure_tracker_key_column(conn)
}

/// How long a real import waits for another import into the same org.
const IMPORT_LOCK_WAIT_SECS: u64 = 10;

/// A MySQL named lock that serializes real imports per org, held from
/// validation through commit. Without it two concurrent imports of the same
/// file (a double click, two Admins) both pass the duplicate checks and both
/// write. Named locks belong to a session, so the guard keeps its own
/// connection and releases the lock on drop, before that connection goes
/// back to the pool.
struct OrgImportLock {
    conn: mysql::PooledConn,
    name: String,
}

impl OrgImportLock {
    fn lock_name(org_id: Uuid) -> String {
        format!("logistics-import:{org_id}")
    }

    /// `Ok(None)` when another import still holds the lock after the wait.
    fn acquire(org_id: Uuid) -> Result<Option<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        let name = Self::lock_name(org_id);
        let got: Option<Option<i64>> = conn.exec_first(
            "SELECT GET_LOCK(:name, :wait)",
            params! { "name" => &name, "wait" => IMPORT_LOCK_WAIT_SECS },
        )?;
        Ok((got.flatten() == Some(1)).then_some(Self { conn, name }))
    }
}

impl Drop for OrgImportLock {
    fn drop(&mut self) {
        if let Err(err) = self
            .conn
            .exec_drop("DO RELEASE_LOCK(:name)", params! { "name" => &self.name })
        {
            eprintln!("couldn't release import lock {}: {err}", self.name);
        }
    }
}

/// Validate `bytes` as a CSV of `kind` for `org_id` and, unless `dry_run` or
/// any row has errors, write every row in one transaction. A real import
/// holds a per-org lock across validate and write, and refreshes the
/// assistant's search index (best-effort) after it commits.
pub fn import_csv(
    org_id: Uuid,
    kind: ImportKind,
    bytes: &[u8],
    dry_run: bool,
) -> Result<Result<ImportReport, FileError>, Box<dyn Error>> {
    let rows = match read_rows(kind, bytes) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };
    let lock = if dry_run {
        None
    } else {
        match OrgImportLock::acquire(org_id)? {
            Some(lock) => Some(lock),
            None => {
                return Ok(Err(FileError(
                    "Another import into this organization is still running; try again shortly"
                        .to_string(),
                )));
            }
        }
    };
    let mut errs = Errors(Vec::new());
    let parsed = match kind {
        ImportKind::Vehicles => Parsed::Vehicles(validate_vehicles(org_id, &rows, &mut errs)?),
        ImportKind::Drivers => Parsed::Drivers(validate_drivers(org_id, &rows, &mut errs)?),
        ImportKind::Customers => Parsed::Customers(validate_customers(org_id, &rows, &mut errs)?),
        ImportKind::Stock => Parsed::Stock(validate_stock(org_id, &rows, &mut errs)?),
    };
    let valid_rows = match &parsed {
        Parsed::Vehicles(v) => v.len(),
        Parsed::Drivers(v) => v.len(),
        Parsed::Customers(v) => v.len(),
        Parsed::Stock(v) => v.len(),
    };
    let mut errors = errs.0;
    errors.sort_by_key(|e| e.line);
    let imported = if dry_run || !errors.is_empty() || valid_rows == 0 {
        0
    } else {
        write_all(org_id, &parsed)?
    };
    drop(lock);
    if imported > 0 {
        // The inserts above bypass the models' write-through indexing, so
        // rebuild the org's assistant index. Best-effort: the rows are
        // committed either way.
        if let Err(err) = crate::logistics::ai::chunk::reindex_org(org_id) {
            eprintln!(
                "assistant reindex after {} import failed: {err}",
                kind.as_str()
            );
        }
    }
    Ok(Ok(ImportReport {
        kind: kind.as_str().to_string(),
        dry_run,
        total_rows: rows.len(),
        valid_rows,
        imported,
        errors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_quotes_crlf_bom_and_blank_lines() {
        let recs =
            parse_csv("\u{feff}a,b\r\n\r\n\"x, y\",\"he said \"\"hi\"\"\"\r\n\"multi\nline\",z\n")
                .unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[1].line, 3);
        assert_eq!(recs[1].fields, vec!["x, y", "he said \"hi\""]);
        assert_eq!(recs[2].line, 4);
        assert_eq!(recs[2].fields, vec!["multi\nline", "z"]);
    }

    #[test]
    fn parser_rejects_unterminated_quote() {
        assert!(parse_csv("a,b\n\"oops,1\n").is_err());
    }

    #[test]
    fn clean_undoes_the_export_formula_guard() {
        assert_eq!(clean(" '+91 98 "), "+91 98");
        assert_eq!(clean("'quoted"), "'quoted");
    }

    #[test]
    fn templates_are_the_header_row() {
        assert_eq!(
            ImportKind::Drivers.template(),
            "name,license_number,phone\r\n"
        );
        assert!(
            read_rows(ImportKind::Stock, ImportKind::Stock.template().as_bytes())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_required_column_is_a_file_error() {
        let err = read_rows(ImportKind::Drivers, b"name,phone\nA,1\n").unwrap_err();
        assert!(err.0.contains("license_number"));
    }

    use crate::logistics::db::connection::set_test_db_override;
    use crate::logistics::orgs::orgs::Organization;
    use crate::logistics::test_support::TestDb;

    fn errors_of(report: &ImportReport) -> Vec<(usize, Option<&str>)> {
        report
            .errors
            .iter()
            .map(|e| (e.line, e.column.as_deref()))
            .collect()
    }

    #[test]
    fn cells_longer_than_their_column_are_row_errors() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Len Co", "1 Rd").unwrap();
        let long = "9".repeat(65);
        let csv = format!(
            "name,license_number,phone\nA,L1,{long}\nB,L2,{}\n",
            &long[1..]
        );
        let report = import_csv(org.id, ImportKind::Drivers, csv.as_bytes(), true)
            .unwrap()
            .unwrap();
        assert_eq!(errors_of(&report), vec![(2, Some("phone"))]);
        assert_eq!(report.valid_rows, 1);

        let long = "x".repeat(256);
        let csv = format!(
            "name,address,phone,email,gstin\nA,1 St,{long},a@b.com,\nB,2 St,,{long}@b.com,\n"
        );
        let report = import_csv(org.id, ImportKind::Customers, csv.as_bytes(), true)
            .unwrap()
            .unwrap();
        assert_eq!(
            errors_of(&report),
            vec![(2, Some("phone")), (3, Some("email"))]
        );

        // Characters, not bytes: 255 multi-byte characters still fit.
        let wide = "\u{0905}".repeat(255);
        let csv =
            format!("godown_name,description,category,quantity\nG,Rice,{long},1\nG,Dal,{wide},1\n");
        let report = import_csv(org.id, ImportKind::Stock, csv.as_bytes(), true)
            .unwrap()
            .unwrap();
        assert!(report.errors.contains(&RowError {
            line: 2,
            column: Some("category".to_string()),
            message: "category is longer than 255 characters".to_string(),
        }));
        assert!(
            !report
                .errors
                .iter()
                .any(|e| e.line == 3 && e.column.as_deref() == Some("category"))
        );
    }

    #[test]
    fn a_licence_shared_by_two_drivers_is_a_row_error() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Twin Licence Co", "1 Rd").unwrap();
        Driver::create(org.id, "First", "MH12 X", "1").unwrap();
        Driver::create(org.id, "Second", "mh12 x", "2").unwrap();
        let csv = "registration_number,capacity,driver_license_number\nTWIN-1,5,MH12 X\n";
        let report = import_csv(org.id, ImportKind::Vehicles, csv.as_bytes(), false)
            .unwrap()
            .unwrap();
        assert_eq!(errors_of(&report), vec![(2, Some("driver_license_number"))]);
        assert_eq!(report.imported, 0);
        assert!(Vehicle::list_by_org(org.id).unwrap().is_empty());
    }

    #[test]
    fn a_real_import_waits_for_another_import_of_the_same_org() {
        let db = TestDb::create();
        let org = Organization::create_organization("Lock Co", "1 Rd").unwrap();
        // Stand in for an import already in flight by holding its lock.
        let held = OrgImportLock::acquire(org.id)
            .unwrap()
            .expect("lock is free");
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            drop(held);
        });
        let started = std::time::Instant::now();
        let csv = b"name,license_number,phone\nA,L1,1\n";
        let report = import_csv(org.id, ImportKind::Drivers, csv, false)
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(1000),
            "import did not wait for the held lock"
        );
        assert_eq!(report.imported, 1);
        release.join().unwrap();

        // Two imports of the same file at the same moment: one writes, the
        // other sees the first one's rows as duplicates.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let csv = b"name,license_number,phone\nB,L2,2\nC,L3,3\n";
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let name = db.name().to_string();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    set_test_db_override(Some(name));
                    barrier.wait();
                    let report = import_csv(org.id, ImportKind::Drivers, csv, false)
                        .unwrap()
                        .unwrap();
                    set_test_db_override(None);
                    report.imported
                })
            })
            .collect();
        let mut imported: Vec<usize> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        imported.sort();
        assert_eq!(imported, vec![0, 2]);
        assert_eq!(Driver::list_by_org(org.id).unwrap().len(), 3);
    }

    #[test]
    fn a_real_import_refreshes_the_assistant_index() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Index Co", "1 Rd").unwrap();
        let csv = b"name,address\nZephyrine Traders,9 Quay Rd\n";
        let report = import_csv(org.id, ImportKind::Customers, csv, false)
            .unwrap()
            .unwrap();
        assert_eq!(report.imported, 1);
        let mut conn = DbConnection::from_env().get_connection().unwrap();
        let texts: Vec<String> = conn
            .exec(
                "SELECT text FROM AiChunks WHERE org_id = :org_id",
                params! { "org_id" => org.id.to_string() },
            )
            .unwrap();
        assert!(
            texts.iter().any(|t| t.contains("Zephyrine Traders")),
            "imported customer is not in the assistant index: {texts:?}"
        );
    }
}
