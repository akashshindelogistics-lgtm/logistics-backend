//! Freight billing — one invoice per dispatch, with a payment status.
//!
//! A dispatch's freight cost is recorded by raising an [`Invoice`] against it
//! (`POST /api/dispatches/{id}/invoice`). The invoice carries an `amount`, an
//! `issued_on` date (server-stamped) and a caller-supplied `due_on` date.
//! `status` is computed on every read: `Paid` once `paid_on` is set, otherwise
//! `Overdue` when `due_on` is in the past, otherwise `Pending`. A customer's
//! payment standing (total outstanding, overdue count) is an aggregate over
//! their invoices — see [`Invoice::customer_summary`].
//!
//! Dates are stored as ISO `YYYY-MM-DD` strings and compared
//! lexicographically (ISO dates sort chronologically). Amounts are whole
//! currency units (the deployment's currency; there is only one).

use crate::logistics::db::connection::DbConnection;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Where an invoice sits relative to payment, computed on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentStatus {
    /// Not yet paid, and not yet past its due date.
    Pending,
    /// Paid — `paid_on` is set.
    Paid,
    /// Unpaid and past its `due_on` date.
    Overdue,
    /// Some money received, a balance remains, and the invoice is not yet
    /// past due.
    PartiallyPaid,
}

/// How a payment was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentMode {
    Cash,
    Upi,
    Cheque,
    Neft,
}

impl PaymentMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PaymentMode::Cash => "CASH",
            PaymentMode::Upi => "UPI",
            PaymentMode::Cheque => "CHEQUE",
            PaymentMode::Neft => "NEFT",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "UPI" => PaymentMode::Upi,
            "CHEQUE" => PaymentMode::Cheque,
            "NEFT" => PaymentMode::Neft,
            _ => PaymentMode::Cash,
        }
    }
}

/// One payment received against an invoice.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct InvoicePayment {
    pub id: Uuid,
    pub invoice_id: Uuid,
    /// Whole currency units, greater than zero.
    pub amount: i64,
    pub mode: PaymentMode,
    /// ISO `YYYY-MM-DD`; defaults to today.
    pub paid_on: String,
    /// Optional UPI transaction id, cheque number or NEFT UTR.
    pub reference: Option<String>,
}

impl PaymentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PaymentStatus::Pending => "PENDING",
            PaymentStatus::Paid => "PAID",
            PaymentStatus::Overdue => "OVERDUE",
            PaymentStatus::PartiallyPaid => "PARTIALLY_PAID",
        }
    }
}

/// A freight invoice raised against exactly one dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Invoice {
    pub id: Uuid,
    pub org_id: Uuid,
    pub dispatch_id: Uuid,
    pub customer_id: Uuid,
    /// Freight charge, in whole currency units.
    pub amount: i64,
    /// ISO `YYYY-MM-DD`, stamped server-side when the invoice is raised.
    pub issued_on: String,
    /// ISO `YYYY-MM-DD`, supplied by the caller.
    pub due_on: String,
    /// ISO `YYYY-MM-DD`, set when the balance reaches zero; `null` until then.
    pub paid_on: Option<String>,
    /// Sum of the payments recorded so far.
    pub amount_paid: i64,
    /// `amount - amount_paid`.
    pub balance: i64,
    /// Server-computed from the balance / `due_on` / today. Never stored.
    pub status: PaymentStatus,
}

/// A customer's overall payment standing, aggregated over their invoices.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CustomerBillingSummary {
    pub customer_id: Uuid,
    pub invoice_count: i64,
    /// Sum of the `balance` across every invoice that is not fully paid.
    pub total_outstanding: i64,
    pub overdue_count: i64,
    pub invoices: Vec<Invoice>,
}

/// Failure modes distinct enough for the route layer to map to status codes.
#[derive(Debug)]
pub enum InvoiceError {
    /// A supplied date was not a valid ISO `YYYY-MM-DD` calendar date.
    InvalidDate(String),
    /// `amount` was not greater than zero.
    NonPositiveAmount,
    /// The dispatch already has an invoice.
    AlreadyInvoiced,
    /// The invoice is already paid and can't be edited.
    AlreadyPaid,
    /// A payment was larger than the invoice's remaining balance.
    Overpayment { balance: i64 },
    /// The new invoice amount would not exceed what is already paid.
    AmountNotAbovePaid { paid: i64 },
    /// A lower-level database error.
    Db(Box<dyn Error>),
}

impl fmt::Display for InvoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvoiceError::InvalidDate(s) => {
                write!(f, "'{s}' is not a valid ISO date (expected YYYY-MM-DD)")
            }
            InvoiceError::NonPositiveAmount => write!(f, "amount must be greater than zero"),
            InvoiceError::AlreadyInvoiced => write!(f, "this dispatch already has an invoice"),
            InvoiceError::AlreadyPaid => write!(f, "a paid invoice cannot be changed"),
            InvoiceError::Overpayment { balance } => {
                write!(f, "payment exceeds the remaining balance of {balance}")
            }
            InvoiceError::AmountNotAbovePaid { paid } => {
                write!(f, "amount must be greater than the {paid} already paid")
            }
            InvoiceError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl Error for InvoiceError {}

impl From<mysql::Error> for InvoiceError {
    fn from(e: mysql::Error) -> Self {
        InvoiceError::Db(Box::new(e))
    }
}

impl From<Box<dyn Error>> for InvoiceError {
    fn from(e: Box<dyn Error>) -> Self {
        InvoiceError::Db(e)
    }
}

/// Days from the Unix epoch to `y-m-d` (Howard Hinnant's `days_from_civil`),
/// and its inverse — mirrors the helpers in `vehicle::document`; kept local to
/// avoid a cross-module dependency for ~15 lines.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Today (UTC) as an ISO `YYYY-MM-DD` string.
pub(crate) fn today_iso() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        .div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Whether `s` is a real calendar date written as ISO `YYYY-MM-DD`.
pub(crate) fn is_valid_iso_date(s: &str) -> bool {
    validate_iso_date(s).is_ok()
}

/// Validate an ISO `YYYY-MM-DD` string, rejecting calendar-invalid dates.
fn validate_iso_date(s: &str) -> Result<(), InvoiceError> {
    let invalid = || InvoiceError::InvalidDate(s.to_string());
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return Err(invalid());
    }
    let y: i64 = parts[0].parse().map_err(|_| invalid())?;
    let m: i64 = parts[1].parse().map_err(|_| invalid())?;
    let d: i64 = parts[2].parse().map_err(|_| invalid())?;
    if !(1..=12).contains(&m) || d < 1 {
        return Err(invalid());
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
    ][(m - 1) as usize];
    if d > dim {
        return Err(invalid());
    }
    // Round-trips through the civil-days helpers, so `today_iso` comparisons
    // are sound.
    let _ = days_from_civil(y, m, d);
    Ok(())
}

fn payment_status(amount: i64, amount_paid: i64, due_on: &str) -> PaymentStatus {
    if amount_paid >= amount {
        PaymentStatus::Paid
    } else if due_on < today_iso().as_str() {
        PaymentStatus::Overdue
    } else if amount_paid > 0 {
        PaymentStatus::PartiallyPaid
    } else {
        PaymentStatus::Pending
    }
}

// `amount_paid` is summed from `InvoicePayments`. Invoices marked paid before
// payments were recorded have no rows there, so `paid_on` counts as fully paid.
const SELECT_COLS: &str =
    "id, org_id, dispatch_id, customer_id, amount, issued_on, due_on, paid_on,
     CASE WHEN paid_on IS NOT NULL THEN amount ELSE
       CAST((SELECT COALESCE(SUM(p.amount), 0) FROM InvoicePayments p
             WHERE p.invoice_id = Invoices.id) AS SIGNED) END";

type InvoiceRow = (
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    Option<String>,
    i64,
);

impl Invoice {
    /// Create the `Invoices` table if it does not exist. Kept in sync with
    /// `test_support::migrate`.
    pub fn ensure_table(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
        crate::logistics::dispatch::dispatch::ensure_tables(conn)?;
        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS Invoices (
                id VARCHAR(36) PRIMARY KEY,
                org_id VARCHAR(36) NOT NULL,
                dispatch_id VARCHAR(36) NOT NULL UNIQUE,
                customer_id VARCHAR(36) NOT NULL,
                amount BIGINT NOT NULL,
                issued_on VARCHAR(10) NOT NULL,
                due_on VARCHAR(10) NOT NULL,
                paid_on VARCHAR(10) DEFAULT NULL,
                CONSTRAINT fk_invoice_org
                    FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE,
                CONSTRAINT fk_invoice_dispatch
                    FOREIGN KEY (dispatch_id) REFERENCES Dispatches(id) ON DELETE CASCADE
            )",
        )?;
        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS InvoicePayments (
                id VARCHAR(36) PRIMARY KEY,
                invoice_id VARCHAR(36) NOT NULL,
                amount BIGINT NOT NULL,
                mode VARCHAR(10) NOT NULL,
                paid_on VARCHAR(10) NOT NULL,
                reference VARCHAR(100) DEFAULT NULL,
                INDEX idx_invoice_payments_invoice (invoice_id),
                CONSTRAINT fk_invoice_payment_invoice
                    FOREIGN KEY (invoice_id) REFERENCES Invoices(id) ON DELETE CASCADE
            )",
        )?;
        Ok(())
    }

    fn hydrate(row: InvoiceRow) -> Self {
        let (id, org_id, dispatch_id, customer_id, amount, issued_on, due_on, paid_on, amount_paid) =
            row;
        let status = payment_status(amount, amount_paid, &due_on);
        Invoice {
            id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
            org_id: Uuid::parse_str(&org_id).unwrap_or_else(|_| Uuid::new_v4()),
            dispatch_id: Uuid::parse_str(&dispatch_id).unwrap_or_else(|_| Uuid::new_v4()),
            customer_id: Uuid::parse_str(&customer_id).unwrap_or_else(|_| Uuid::new_v4()),
            amount,
            issued_on,
            due_on,
            paid_on,
            amount_paid,
            balance: amount - amount_paid,
            status,
        }
    }

    /// Raise an invoice against a dispatch. `org_id` / `customer_id` come from
    /// the dispatch (the route layer has already checked the caller owns it).
    /// Fails if the dispatch already has an invoice.
    pub fn create(
        org_id: Uuid,
        dispatch_id: Uuid,
        customer_id: Uuid,
        amount: i64,
        due_on: impl Into<String>,
    ) -> Result<Self, InvoiceError> {
        if amount <= 0 {
            return Err(InvoiceError::NonPositiveAmount);
        }
        let due_on = due_on.into();
        validate_iso_date(&due_on)?;

        let mut conn = DbConnection::from_env()
            .get_connection()
            .map_err(InvoiceError::Db)?;
        Self::ensure_table(&mut conn).map_err(InvoiceError::Db)?;

        if Self::get_by_dispatch(dispatch_id)
            .map_err(InvoiceError::Db)?
            .is_some()
        {
            return Err(InvoiceError::AlreadyInvoiced);
        }

        let id = Uuid::new_v4();
        let issued_on = today_iso();

        conn.exec_drop(
            "INSERT INTO Invoices (id, org_id, dispatch_id, customer_id, amount, issued_on, due_on, paid_on)
             VALUES (:id, :org_id, :dispatch_id, :customer_id, :amount, :issued_on, :due_on, NULL)",
            params! {
                "id" => id.to_string(),
                "org_id" => org_id.to_string(),
                "dispatch_id" => dispatch_id.to_string(),
                "customer_id" => customer_id.to_string(),
                "amount" => amount,
                "issued_on" => &issued_on,
                "due_on" => &due_on,
            },
        )?;

        let status = payment_status(amount, 0, &due_on);
        Ok(Invoice {
            id,
            org_id,
            dispatch_id,
            customer_id,
            amount,
            issued_on,
            due_on,
            paid_on: None,
            amount_paid: 0,
            balance: amount,
            status,
        })
    }

    pub fn get_by_id(id: Uuid) -> Result<Option<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let row: Option<InvoiceRow> = conn.exec_first(
            format!("SELECT {SELECT_COLS} FROM Invoices WHERE id = :id"),
            params! { "id" => id.to_string() },
        )?;
        Ok(row.map(Self::hydrate))
    }

    pub fn get_by_dispatch(dispatch_id: Uuid) -> Result<Option<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let row: Option<InvoiceRow> = conn.exec_first(
            format!("SELECT {SELECT_COLS} FROM Invoices WHERE dispatch_id = :dispatch_id"),
            params! { "dispatch_id" => dispatch_id.to_string() },
        )?;
        Ok(row.map(Self::hydrate))
    }

    /// Every invoice for the org, most recently issued first.
    pub fn list_by_org(org_id: Uuid) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let rows: Vec<InvoiceRow> = conn.exec(
            format!(
                "SELECT {SELECT_COLS} FROM Invoices WHERE org_id = :org_id
                 ORDER BY issued_on DESC, id DESC"
            ),
            params! { "org_id" => org_id.to_string() },
        )?;
        Ok(rows.into_iter().map(Self::hydrate).collect())
    }

    fn list_by_customer(customer_id: Uuid) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let rows: Vec<InvoiceRow> = conn.exec(
            format!(
                "SELECT {SELECT_COLS} FROM Invoices WHERE customer_id = :customer_id
                 ORDER BY issued_on DESC, id DESC"
            ),
            params! { "customer_id" => customer_id.to_string() },
        )?;
        Ok(rows.into_iter().map(Self::hydrate).collect())
    }

    /// A customer's payment standing: their invoices plus the outstanding
    /// total and overdue count.
    pub fn customer_summary(customer_id: Uuid) -> Result<CustomerBillingSummary, Box<dyn Error>> {
        let invoices = Self::list_by_customer(customer_id)?;
        let total_outstanding = invoices.iter().map(|i| i.balance).sum();
        let overdue_count = invoices
            .iter()
            .filter(|i| i.status == PaymentStatus::Overdue)
            .count() as i64;
        Ok(CustomerBillingSummary {
            customer_id,
            invoice_count: invoices.len() as i64,
            total_outstanding,
            overdue_count,
            invoices,
        })
    }

    /// Change the amount and/or due date. Rejected once the invoice is paid.
    pub fn update(&mut self, amount: i64, due_on: impl Into<String>) -> Result<(), InvoiceError> {
        if self.paid_on.is_some() {
            return Err(InvoiceError::AlreadyPaid);
        }
        if amount <= 0 {
            return Err(InvoiceError::NonPositiveAmount);
        }
        if amount <= self.amount_paid {
            return Err(InvoiceError::AmountNotAbovePaid {
                paid: self.amount_paid,
            });
        }
        let due_on = due_on.into();
        validate_iso_date(&due_on)?;

        let mut conn = DbConnection::from_env()
            .get_connection()
            .map_err(InvoiceError::Db)?;
        conn.exec_drop(
            "UPDATE Invoices SET amount = :amount, due_on = :due_on WHERE id = :id",
            params! {
                "id" => self.id.to_string(),
                "amount" => amount,
                "due_on" => &due_on,
            },
        )?;

        self.amount = amount;
        self.due_on = due_on;
        self.balance = amount - self.amount_paid;
        self.status = payment_status(self.amount, self.amount_paid, &self.due_on);
        Ok(())
    }

    /// Record a payment. `amount` must be positive and no more than the
    /// balance; `paid_on` defaults to today. Stamps the invoice's `paid_on`
    /// once the balance reaches zero. Serialised per invoice by a row lock so
    /// concurrent payments can't overpay.
    pub fn record_payment(
        &mut self,
        amount: i64,
        mode: PaymentMode,
        paid_on: Option<String>,
        reference: Option<String>,
    ) -> Result<InvoicePayment, InvoiceError> {
        if amount <= 0 {
            return Err(InvoiceError::NonPositiveAmount);
        }
        let paid_on = paid_on.unwrap_or_else(today_iso);
        validate_iso_date(&paid_on)?;
        let reference = reference
            .map(|r| r.trim().chars().take(100).collect::<String>())
            .filter(|r| !r.is_empty());

        let mut conn = DbConnection::from_env()
            .get_connection()
            .map_err(InvoiceError::Db)?;
        Self::ensure_table(&mut conn).map_err(InvoiceError::Db)?;
        let mut tx = conn.start_transaction(TxOpts::default())?;
        let locked: Option<i64> = tx.exec_first(
            "SELECT amount FROM Invoices WHERE id = :id FOR UPDATE",
            params! { "id" => self.id.to_string() },
        )?;
        let total = locked.unwrap_or(self.amount);
        let row: Option<InvoiceRow> = tx.exec_first(
            format!("SELECT {SELECT_COLS} FROM Invoices WHERE id = :id"),
            params! { "id" => self.id.to_string() },
        )?;
        let current = row.map(Self::hydrate).unwrap_or_else(|| self.clone());
        let balance = total - current.amount_paid;
        if amount > balance {
            return Err(InvoiceError::Overpayment { balance });
        }

        let payment = InvoicePayment {
            id: Uuid::new_v4(),
            invoice_id: self.id,
            amount,
            mode,
            paid_on: paid_on.clone(),
            reference,
        };
        tx.exec_drop(
            "INSERT INTO InvoicePayments (id, invoice_id, amount, mode, paid_on, reference)
             VALUES (:id, :invoice_id, :amount, :mode, :paid_on, :reference)",
            params! {
                "id" => payment.id.to_string(),
                "invoice_id" => self.id.to_string(),
                "amount" => amount,
                "mode" => mode.as_str(),
                "paid_on" => &payment.paid_on,
                "reference" => &payment.reference,
            },
        )?;
        let amount_paid = current.amount_paid + amount;
        let settled = amount_paid >= total;
        if settled && current.paid_on.is_none() {
            tx.exec_drop(
                "UPDATE Invoices SET paid_on = :paid_on WHERE id = :id",
                params! { "id" => self.id.to_string(), "paid_on" => &paid_on },
            )?;
        }
        tx.commit()?;

        self.amount = total;
        self.amount_paid = amount_paid;
        self.balance = total - amount_paid;
        if settled && self.paid_on.is_none() {
            self.paid_on = Some(paid_on);
        }
        self.status = payment_status(self.amount, self.amount_paid, &self.due_on);
        Ok(payment)
    }

    /// Payments recorded against this invoice, oldest first.
    pub fn payments(&self) -> Result<Vec<InvoicePayment>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let rows: Vec<(String, i64, String, String, Option<String>)> = conn.exec(
            "SELECT id, amount, mode, paid_on, reference FROM InvoicePayments
             WHERE invoice_id = :id ORDER BY paid_on, id",
            params! { "id" => self.id.to_string() },
        )?;
        Ok(rows
            .into_iter()
            .map(|(id, amount, mode, paid_on, reference)| InvoicePayment {
                id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
                invoice_id: self.id,
                amount,
                mode: PaymentMode::parse(&mode),
                paid_on,
                reference,
            })
            .collect())
    }

    /// Pay off the whole remaining balance in cash, dated today. A no-op if
    /// the invoice is already paid.
    pub fn mark_paid(&mut self) -> Result<(), InvoiceError> {
        if self.balance <= 0 {
            return Ok(());
        }
        self.record_payment(self.balance, PaymentMode::Cash, None, None)
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::customer::customer::Customer;
    use crate::logistics::dispatch::dispatch::{DispatchLineItem, DispatchOrder, DispatchStatus};
    use crate::logistics::orgs::orgs::Organization;
    use crate::logistics::test_support::TestDb;

    fn iso_offset(days: i64) -> String {
        let today = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            / 86_400;
        let (y, m, d) = civil_from_days(today + days);
        format!("{y:04}-{m:02}-{d:02}")
    }

    fn org_customer_dispatch() -> (Organization, Customer, DispatchOrder) {
        let org = Organization::create_organization("Billing Org", "1 Finance Rd").expect("org");
        let customer =
            Customer::create_customer(org.id, "Paying Customer", "9 Market Rd").expect("customer");
        let mut dispatch = DispatchOrder {
            id: Uuid::new_v4(),
            org_id: org.id,
            customer_id: customer.id,
            vehicle_registration_number: Some("KA01 BILL 1".to_string()),
            vehicle_source: Default::default(),
            hire_id: None,
            line_items: vec![DispatchLineItem {
                stock_description: "Cement".to_string(),
                quantity: 10,
                volume_in_size: 1,
                category: "General".to_string(),
            }],
            status: DispatchStatus::Pending,
            dispatched_at: 1_700_000_000,
            status_history: Vec::new(),
            proof_of_delivery: None,
            trip_id: None,
            stop_sequence: None,
            eway_bill_no: None,
        };
        dispatch.save().expect("save dispatch");
        (org, customer, dispatch)
    }

    #[test]
    fn test_validate_iso_date_rejects_nonsense() {
        assert!(validate_iso_date("2026-09-30").is_ok());
        assert!(validate_iso_date("2026-02-30").is_err());
        assert!(validate_iso_date("nope").is_err());
    }

    #[test]
    fn test_create_and_status_tracks_the_due_date() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();

        let pending = Invoice::create(org.id, dispatch.id, customer.id, 5000, iso_offset(15))
            .expect("create");
        assert_eq!(pending.status, PaymentStatus::Pending);
        assert_eq!(pending.amount, 5000);
        assert_eq!(pending.issued_on, today_iso());

        // A second invoice for the same dispatch is rejected.
        let dup = Invoice::create(org.id, dispatch.id, customer.id, 1, iso_offset(1))
            .expect_err("one invoice per dispatch");
        assert!(matches!(dup, InvoiceError::AlreadyInvoiced));

        let reloaded = Invoice::get_by_dispatch(dispatch.id).unwrap().unwrap();
        assert_eq!(reloaded.id, pending.id);
    }

    #[test]
    fn test_overdue_when_due_date_has_passed() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        let inv =
            Invoice::create(org.id, dispatch.id, customer.id, 200, iso_offset(-3)).expect("create");
        assert_eq!(inv.status, PaymentStatus::Overdue);
    }

    #[test]
    fn test_create_rejects_bad_input() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        assert!(matches!(
            Invoice::create(org.id, dispatch.id, customer.id, 0, iso_offset(5)),
            Err(InvoiceError::NonPositiveAmount)
        ));
        assert!(matches!(
            Invoice::create(org.id, dispatch.id, customer.id, 100, "2026-13-40"),
            Err(InvoiceError::InvalidDate(_))
        ));
    }

    #[test]
    fn test_mark_paid_is_terminal_and_idempotent() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        let mut inv =
            Invoice::create(org.id, dispatch.id, customer.id, 750, iso_offset(-1)).expect("create");
        assert_eq!(inv.status, PaymentStatus::Overdue);

        inv.mark_paid().expect("pay");
        assert_eq!(inv.status, PaymentStatus::Paid);
        assert_eq!(inv.paid_on.as_deref(), Some(today_iso().as_str()));
        inv.mark_paid().expect("paying again is a no-op");

        // A paid invoice can't be edited.
        assert!(matches!(
            inv.update(999, iso_offset(30)),
            Err(InvoiceError::AlreadyPaid)
        ));

        let reloaded = Invoice::get_by_id(inv.id).unwrap().unwrap();
        assert_eq!(reloaded.status, PaymentStatus::Paid);
    }

    #[test]
    fn test_update_changes_amount_and_recomputes_status() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        let mut inv =
            Invoice::create(org.id, dispatch.id, customer.id, 100, iso_offset(-2)).expect("create");
        assert_eq!(inv.status, PaymentStatus::Overdue);

        inv.update(250, iso_offset(20)).expect("update");
        assert_eq!(inv.amount, 250);
        assert_eq!(inv.status, PaymentStatus::Pending);
    }

    #[test]
    fn test_customer_summary_aggregates_outstanding_and_overdue() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Summary Org", "1 Rd").expect("org");
        let customer =
            Customer::create_customer(org.id, "Multi Invoice Cust", "2 Rd").expect("customer");

        // Three dispatches, three invoices: one paid, one pending, one overdue.
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut d = DispatchOrder {
                id: Uuid::new_v4(),
                org_id: org.id,
                customer_id: customer.id,
                vehicle_registration_number: Some(format!("KA01 S {i}")),
                vehicle_source: Default::default(),
                hire_id: None,
                line_items: vec![DispatchLineItem {
                    stock_description: "Goods".to_string(),
                    quantity: 1,
                    volume_in_size: 1,
                    category: "General".to_string(),
                }],
                status: DispatchStatus::Pending,
                dispatched_at: 1_700_000_000,
                status_history: Vec::new(),
                proof_of_delivery: None,
                trip_id: None,
                stop_sequence: None,
                eway_bill_no: None,
            };
            d.save().unwrap();
            ids.push(d.id);
        }

        let mut paid = Invoice::create(org.id, ids[0], customer.id, 1000, iso_offset(10)).unwrap();
        paid.mark_paid().unwrap();
        Invoice::create(org.id, ids[1], customer.id, 400, iso_offset(10)).unwrap();
        Invoice::create(org.id, ids[2], customer.id, 250, iso_offset(-5)).unwrap();

        let summary = Invoice::customer_summary(customer.id).unwrap();
        assert_eq!(summary.invoice_count, 3);
        assert_eq!(summary.total_outstanding, 650); // 400 pending + 250 overdue
        assert_eq!(summary.overdue_count, 1);
    }

    #[test]
    fn test_partial_payments_follow_the_balance() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        let mut inv =
            Invoice::create(org.id, dispatch.id, customer.id, 1000, iso_offset(10)).unwrap();

        inv.record_payment(400, PaymentMode::Upi, None, Some("UTR1".into()))
            .unwrap();
        assert_eq!((inv.amount_paid, inv.balance), (400, 600));
        assert_eq!(inv.status, PaymentStatus::PartiallyPaid);
        assert!(inv.paid_on.is_none());

        // Overpaying or a bad amount is rejected.
        assert!(matches!(
            inv.record_payment(601, PaymentMode::Cash, None, None),
            Err(InvoiceError::Overpayment { balance: 600 })
        ));
        assert!(matches!(
            inv.record_payment(0, PaymentMode::Cash, None, None),
            Err(InvoiceError::NonPositiveAmount)
        ));
        // The amount can't drop to what is already paid.
        assert!(matches!(
            inv.update(400, iso_offset(10)),
            Err(InvoiceError::AmountNotAbovePaid { paid: 400 })
        ));

        let reloaded = Invoice::get_by_id(inv.id).unwrap().unwrap();
        assert_eq!(reloaded.balance, 600);
        assert_eq!(
            Invoice::customer_summary(customer.id)
                .unwrap()
                .total_outstanding,
            600
        );

        inv.record_payment(600, PaymentMode::Cheque, Some(iso_offset(0)), None)
            .unwrap();
        assert_eq!(inv.status, PaymentStatus::Paid);
        assert_eq!(inv.balance, 0);
        assert!(inv.paid_on.is_some());

        let payments = inv.payments().unwrap();
        assert_eq!(payments.len(), 2);
        assert_eq!(payments.iter().map(|p| p.amount).sum::<i64>(), 1000);
        assert!(
            payments
                .iter()
                .any(|p| p.mode == PaymentMode::Upi && p.reference.as_deref() == Some("UTR1"))
        );
        assert_eq!(
            Invoice::get_by_id(inv.id).unwrap().unwrap().status,
            PaymentStatus::Paid
        );
    }

    #[test]
    fn test_overdue_wins_over_partially_paid() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        let mut inv =
            Invoice::create(org.id, dispatch.id, customer.id, 100, iso_offset(-1)).unwrap();
        inv.record_payment(10, PaymentMode::Neft, None, None)
            .unwrap();
        assert_eq!(inv.status, PaymentStatus::Overdue);
    }

    #[test]
    fn test_deleting_the_dispatch_cascades_to_its_invoice() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        let inv = Invoice::create(org.id, dispatch.id, customer.id, 500, iso_offset(5)).unwrap();

        let mut conn = DbConnection::from_env().get_connection().unwrap();
        conn.exec_drop(
            "DELETE FROM Dispatches WHERE id = :id",
            params! { "id" => dispatch.id.to_string() },
        )
        .unwrap();

        assert!(Invoice::get_by_id(inv.id).unwrap().is_none());
    }

    #[test]
    fn test_deleting_the_org_cascades_to_invoices() {
        let _db = TestDb::create();
        let (org, customer, dispatch) = org_customer_dispatch();
        Invoice::create(org.id, dispatch.id, customer.id, 500, iso_offset(5)).unwrap();
        org.remove_organization().expect("remove org");
        assert!(Invoice::list_by_org(org.id).unwrap().is_empty());
    }
}
