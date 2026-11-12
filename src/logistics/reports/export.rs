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
    /// Freight invoices as Tally Sales vouchers (XML).
    TallyInvoices,
    /// Dispatched stock as Tally Delivery Note vouchers (XML).
    TallyStock,
}

impl ExportDataset {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dispatches" => Some(Self::Dispatches),
            "stock" => Some(Self::Stock),
            "invoices" => Some(Self::Invoices),
            "customers" => Some(Self::Customers),
            "tally-invoices" => Some(Self::TallyInvoices),
            "tally-stock" => Some(Self::TallyStock),
            _ => None,
        }
    }

    /// File extension of the download.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::TallyInvoices | Self::TallyStock => "xml",
            _ => "csv",
        }
    }

    pub fn content_type(&self) -> &'static str {
        match self {
            Self::TallyInvoices | Self::TallyStock => "application/xml; charset=utf-8",
            _ => "text/csv; charset=utf-8",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dispatches => "dispatches",
            Self::Stock => "stock",
            Self::Invoices => "invoices",
            Self::Customers => "customers",
            Self::TallyInvoices => "tally-invoices",
            Self::TallyStock => "tally-stock",
        }
    }

    /// Build the CSV (header row included) for `org_id`.
    pub fn to_csv(&self, org_id: Uuid) -> Result<String, Box<dyn Error>> {
        match self {
            Self::Dispatches => dispatches_csv(org_id),
            Self::Stock => stock_csv(org_id),
            Self::Invoices => invoices_csv(org_id),
            Self::Customers => customers_csv(org_id),
            Self::TallyInvoices => tally_invoices_xml(org_id),
            Self::TallyStock => tally_stock_xml(org_id),
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

/// Escape text for XML element content.
fn xml(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// ISO `YYYY-MM-DD` to Tally's `YYYYMMDD`.
fn tally_date(iso: &str) -> String {
    iso.replace('-', "")
}

/// Wrap voucher XML in a Tally import envelope.
fn tally_envelope(vouchers: &str) -> String {
    format!(
        "<ENVELOPE>\n<HEADER><TALLYREQUEST>Import Data</TALLYREQUEST></HEADER>\n\
         <BODY><IMPORTDATA>\n<REQUESTDESC><REPORTNAME>Vouchers</REPORTNAME></REQUESTDESC>\n\
         <REQUESTDATA>\n{vouchers}</REQUESTDATA>\n</IMPORTDATA></BODY>\n</ENVELOPE>\n"
    )
}

/// Sales vouchers: debit the customer's ledger, credit "Freight Income".
fn tally_invoices_xml(org_id: Uuid) -> Result<String, Box<dyn Error>> {
    let customers: std::collections::HashMap<Uuid, String> = Customer::list_by_org(org_id)?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect();
    let mut vouchers = String::new();
    for i in Invoice::list_by_org(org_id)? {
        let party = xml(customers.get(&i.customer_id).map_or("", |n| n.as_str()));
        vouchers.push_str(&format!(
            "<TALLYMESSAGE><VOUCHER VCHTYPE=\"Sales\" ACTION=\"Create\">\n\
             <DATE>{date}</DATE>\n<VOUCHERTYPENAME>Sales</VOUCHERTYPENAME>\n\
             <VOUCHERNUMBER>{id}</VOUCHERNUMBER>\n<REFERENCE>{dispatch}</REFERENCE>\n\
             <PARTYLEDGERNAME>{party}</PARTYLEDGERNAME>\n\
             <NARRATION>Freight for dispatch {dispatch}; due {due}</NARRATION>\n\
             <ALLLEDGERENTRIES.LIST><LEDGERNAME>{party}</LEDGERNAME>\
             <ISDEEMEDPOSITIVE>Yes</ISDEEMEDPOSITIVE><AMOUNT>-{amount}</AMOUNT></ALLLEDGERENTRIES.LIST>\n\
             <ALLLEDGERENTRIES.LIST><LEDGERNAME>Freight Income</LEDGERNAME>\
             <ISDEEMEDPOSITIVE>No</ISDEEMEDPOSITIVE><AMOUNT>{amount}</AMOUNT></ALLLEDGERENTRIES.LIST>\n\
             </VOUCHER></TALLYMESSAGE>\n",
            date = tally_date(&i.issued_on),
            id = i.id,
            dispatch = i.dispatch_id,
            due = xml(&i.due_on),
            amount = i.amount,
        ));
    }
    Ok(tally_envelope(&vouchers))
}

/// Delivery Note vouchers: one per dispatch, with an inventory entry per line item.
fn tally_stock_xml(org_id: Uuid) -> Result<String, Box<dyn Error>> {
    let customers: std::collections::HashMap<Uuid, String> = Customer::list_by_org(org_id)?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect();
    let mut vouchers = String::new();
    for d in DispatchOrder::list_by_org(org_id)? {
        let party = xml(customers.get(&d.customer_id).map_or("", |n| n.as_str()));
        let mut entries = String::new();
        for l in &d.line_items {
            entries.push_str(&format!(
                "<ALLINVENTORYENTRIES.LIST><STOCKITEMNAME>{}</STOCKITEMNAME>\
                 <ISDEEMEDPOSITIVE>No</ISDEEMEDPOSITIVE>\
                 <ACTUALQTY>{} Nos</ACTUALQTY><BILLEDQTY>{} Nos</BILLEDQTY></ALLINVENTORYENTRIES.LIST>\n",
                xml(&l.stock_description),
                l.quantity,
                l.quantity
            ));
        }
        vouchers.push_str(&format!(
            "<TALLYMESSAGE><VOUCHER VCHTYPE=\"Delivery Note\" ACTION=\"Create\">\n\
             <DATE>{date}</DATE>\n<VOUCHERTYPENAME>Delivery Note</VOUCHERTYPENAME>\n\
             <VOUCHERNUMBER>{id}</VOUCHERNUMBER>\n<PARTYLEDGERNAME>{party}</PARTYLEDGERNAME>\n\
             <NARRATION>Dispatch {id} ({status}), vehicle {vehicle}</NARRATION>\n\
             {entries}</VOUCHER></TALLYMESSAGE>\n",
            date = tally_date(&date_of(d.dispatched_at)),
            id = d.id,
            status = d.status.as_str(),
            vehicle = xml(&opt(&d.vehicle_registration_number)),
        ));
    }
    Ok(tally_envelope(&vouchers))
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
    fn xml_escapes_markup() {
        assert_eq!(xml("A & B <\"c\">"), "A &amp; B &lt;&quot;c&quot;&gt;");
        assert_eq!(tally_date("2026-11-12"), "20261112");
    }

    #[test]
    fn parse_round_trips() {
        for d in [
            "dispatches",
            "stock",
            "invoices",
            "customers",
            "tally-invoices",
            "tally-stock",
        ] {
            assert_eq!(ExportDataset::parse(d).unwrap().as_str(), d);
        }
        assert!(ExportDataset::parse("users").is_none());
    }
}
