//! CSV export of an organisation's own data — dispatches, stock, invoices and
//! customers — so it can always be taken out of the system (see
//! `docs/data-export.md`). Like the reports, everything is derived from the
//! existing list methods; nothing is stored.

use super::{SECONDS_PER_DAY, iso_date_from_epoch_day};
use crate::logistics::billing::invoice::Invoice;
use crate::logistics::customer::customer::Customer;
use crate::logistics::dispatch::dispatch::DispatchOrder;
use crate::logistics::godown::godown::Godown;
use std::error::Error;
use uuid::Uuid;

/// Which slice of the org's data to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportDataset {
    Dispatches,
    Stock,
    Invoices,
    Customers,
}

impl ExportDataset {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dispatches" => Some(Self::Dispatches),
            "stock" => Some(Self::Stock),
            "invoices" => Some(Self::Invoices),
            "customers" => Some(Self::Customers),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dispatches => "dispatches",
            Self::Stock => "stock",
            Self::Invoices => "invoices",
            Self::Customers => "customers",
        }
    }

    /// Build the CSV (header row included) for `org_id`.
    pub fn to_csv(&self, org_id: Uuid) -> Result<String, Box<dyn Error>> {
        match self {
            Self::Dispatches => dispatches_csv(org_id),
            Self::Stock => stock_csv(org_id),
            Self::Invoices => invoices_csv(org_id),
            Self::Customers => customers_csv(org_id),
        }
    }
}

/// One CSV field. Quotes when it contains a comma, quote or newline, and
/// prefixes a `'` to text starting with `= + - @` (or tab/CR) so a spreadsheet
/// can't read user-entered text as a formula.
fn field(value: &str) -> String {
    let mut v = value.to_string();
    if matches!(v.chars().next(), Some('=' | '+' | '-' | '@' | '\t' | '\r')) {
        v.insert(0, '\'');
    }
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

fn row(fields: &[String]) -> String {
    let mut line = fields
        .iter()
        .map(|f| field(f))
        .collect::<Vec<_>>()
        .join(",");
    line.push_str("\r\n");
    line
}

fn header(cols: &[&str]) -> String {
    row(&cols.iter().map(|c| c.to_string()).collect::<Vec<_>>())
}

fn date_of(epoch_secs: i64) -> String {
    iso_date_from_epoch_day(epoch_secs.div_euclid(SECONDS_PER_DAY))
}

fn opt(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn dispatches_csv(org_id: Uuid) -> Result<String, Box<dyn Error>> {
    let mut out = header(&[
        "id",
        "customer_id",
        "status",
        "vehicle_registration_number",
        "vehicle_source",
        "dispatched_date",
        "total_quantity",
        "line_items",
        "delivered_date",
        "trip_id",
        "stop_sequence",
    ]);
    for d in DispatchOrder::list_by_org(org_id)? {
        let items = d
            .line_items
            .iter()
            .map(|l| format!("{} x{}", l.stock_description, l.quantity))
            .collect::<Vec<_>>()
            .join("; ");
        let delivered = d
            .status_history
            .iter()
            .find(|e| e.status == crate::logistics::dispatch::dispatch::DispatchStatus::Delivered)
            .map(|e| date_of(e.changed_at))
            .unwrap_or_default();
        out.push_str(&row(&[
            d.id.to_string(),
            d.customer_id.to_string(),
            d.status.as_str().to_string(),
            opt(&d.vehicle_registration_number),
            format!("{:?}", d.vehicle_source).to_uppercase(),
            date_of(d.dispatched_at),
            d.total_quantity().to_string(),
            items,
            delivered,
            d.trip_id.map(|t| t.to_string()).unwrap_or_default(),
            d.stop_sequence.map(|s| s.to_string()).unwrap_or_default(),
        ]));
    }
    Ok(out)
}

fn stock_csv(org_id: Uuid) -> Result<String, Box<dyn Error>> {
    let mut out = header(&[
        "godown_id",
        "godown_name",
        "description",
        "category",
        "quantity",
        "volume_in_size",
        "reorder_threshold",
        "below_threshold",
    ]);
    for g in Godown::list_by_org(org_id)? {
        for s in &g.stock {
            out.push_str(&row(&[
                g.id.to_string(),
                g.name.clone(),
                s.description.clone(),
                s.category.clone(),
                s.quantity.to_string(),
                s.volume_in_size.to_string(),
                s.reorder_threshold
                    .map(|t| t.to_string())
                    .unwrap_or_default(),
                s.below_threshold.to_string(),
            ]));
        }
    }
    Ok(out)
}

fn invoices_csv(org_id: Uuid) -> Result<String, Box<dyn Error>> {
    let mut out = header(&[
        "id",
        "dispatch_id",
        "customer_id",
        "amount",
        "issued_on",
        "due_on",
        "paid_on",
        "status",
    ]);
    for i in Invoice::list_by_org(org_id)? {
        out.push_str(&row(&[
            i.id.to_string(),
            i.dispatch_id.to_string(),
            i.customer_id.to_string(),
            i.amount.to_string(),
            i.issued_on.clone(),
            i.due_on.clone(),
            opt(&i.paid_on),
            i.status.as_str().to_string(),
        ]));
    }
    Ok(out)
}

fn customers_csv(org_id: Uuid) -> Result<String, Box<dyn Error>> {
    let mut out = header(&["id", "name", "address", "phone", "email"]);
    for c in Customer::list_by_org(org_id)? {
        out.push_str(&row(&[
            c.id.to_string(),
            c.name.clone(),
            c.address.clone(),
            opt(&c.phone),
            opt(&c.email),
        ]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_quotes_and_escapes() {
        assert_eq!(field("plain"), "plain");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn field_neutralises_spreadsheet_formulas() {
        assert_eq!(field("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(field("@cmd"), "'@cmd");
        assert_eq!(field("+1"), "'+1");
    }

    #[test]
    fn parse_round_trips() {
        for d in ["dispatches", "stock", "invoices", "customers"] {
            assert_eq!(ExportDataset::parse(d).unwrap().as_str(), d);
        }
        assert!(ExportDataset::parse("users").is_none());
    }
}
