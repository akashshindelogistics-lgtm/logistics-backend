//! The delivery challan / lorry receipt (LR, "bilty") for one dispatch: the
//! paper the driver carries with the load and the customer signs on receipt.
//!
//! [`Challan::for_dispatch`] gathers what goes on it (consignor, consignee,
//! vehicle, driver, line items) and [`Challan::render_pdf`] lays it out as a
//! printable A4 PDF. Served by `GET /api/dispatches/{id}/challan.pdf`.
//! See docs/delivery-challan.md.

use super::dispatch::{DispatchLineItem, DispatchOrder, DispatchStatus, VehicleSource};
use super::pdf::{self, Font, PAGE_HEIGHT, PAGE_WIDTH, Page};
use crate::logistics::customer::customer::Customer;
use crate::logistics::driver::driver::Driver;
use crate::logistics::orgs::orgs::Organization;
use crate::logistics::vehicle::vehicle::Vehicle;
use crate::logistics::vendor::hire::VehicleHire;
use std::error::Error;
use uuid::Uuid;

/// The sender or receiver block on the challan.
#[derive(Debug, Clone, PartialEq)]
pub struct ChallanParty {
    pub name: String,
    pub address: String,
    pub phone: Option<String>,
}

/// Everything printed on one dispatch's challan.
#[derive(Debug, Clone, PartialEq)]
pub struct Challan {
    pub dispatch_id: Uuid,
    /// When the dispatch was created (Unix seconds); the challan's date.
    pub issued_at: i64,
    pub status: DispatchStatus,
    /// The organisation sending the goods.
    pub consignor: ChallanParty,
    /// The customer receiving them.
    pub consignee: ChallanParty,
    /// Truck number, or a placeholder while a hired truck is awaited.
    pub vehicle: String,
    pub vehicle_source: VehicleSource,
    /// The vendor a hired truck came from. `None` for the org's own fleet.
    pub transporter: Option<String>,
    pub driver_name: Option<String>,
    pub driver_phone: Option<String>,
    pub driver_license: Option<String>,
    /// This dispatch's stop number when it is one stop on a multi-stop trip.
    pub stop_sequence: Option<i64>,
    pub line_items: Vec<DispatchLineItem>,
}

/// Seconds east of UTC for the times printed on a challan (IST). Every
/// customer so far is in India; make this per-org if that changes.
const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;

impl Challan {
    /// The challan's printed number: `DC-` and the first 8 hex digits of the
    /// dispatch id, the same short id customers see in SMS and email.
    pub fn number(&self) -> String {
        format!(
            "DC-{}",
            self.dispatch_id.to_string()[..8].to_ascii_uppercase()
        )
    }

    /// Gather a challan for `dispatch` from the org, customer, vehicle,
    /// driver and (for a hired truck) vehicle-hire records.
    ///
    /// For an own-fleet truck the driver is the one assigned to it **now**:
    /// the dispatch does not record who drove it, so a challan reprinted
    /// after the truck's driver changes shows the new driver.
    pub fn for_dispatch(dispatch: &DispatchOrder) -> Result<Self, Box<dyn Error>> {
        let org = Organization::get_by_id(dispatch.org_id)?.ok_or("organisation not found")?;
        let customer = Customer::get_by_id(dispatch.customer_id)?.ok_or("customer not found")?;

        let (mut transporter, mut driver_name, mut driver_phone, mut driver_license) =
            (None, None, None, None);
        if let Some(hire_id) = dispatch.hire_id {
            if let Some(hire) = VehicleHire::get_by_id(hire_id)? {
                transporter = Some(hire.vendor_name).filter(|s| !s.is_empty());
                driver_name = hire.driver_name;
                driver_phone = hire.driver_phone;
                driver_license = hire.driver_license;
            }
        } else if let Some(reg) = &dispatch.vehicle_registration_number {
            let driver_id = Vehicle::list_by_org(dispatch.org_id)?
                .into_iter()
                .find(|v| &v.registration_number == reg)
                .and_then(|v| v.assigned_driver_id);
            if let Some(driver) = driver_id.map(Driver::get_by_id).transpose()?.flatten() {
                driver_name = Some(driver.name);
                driver_phone = Some(driver.phone);
                driver_license = Some(driver.license_number);
            }
        }

        Ok(Challan {
            dispatch_id: dispatch.id,
            issued_at: dispatch.dispatched_at,
            status: dispatch.status,
            consignor: ChallanParty {
                name: org.name,
                address: org.address,
                phone: None,
            },
            consignee: ChallanParty {
                name: customer.name,
                address: customer.address,
                phone: customer.phone,
            },
            vehicle: dispatch.vehicle_label(),
            vehicle_source: dispatch.vehicle_source,
            transporter,
            driver_name,
            driver_phone,
            driver_license,
            stop_sequence: dispatch.stop_sequence,
            line_items: dispatch.line_items.clone(),
        })
    }

    /// A suggested download name, e.g. `challan-DC-1A2B3C4D.pdf`.
    pub fn file_name(&self) -> String {
        format!("challan-{}.pdf", self.number())
    }

    /// Lay the challan out as an A4 PDF. Line items run onto further pages
    /// when they don't fit, repeating the table header; the signature boxes
    /// always close the last page.
    pub fn render_pdf(&self) -> Vec<u8> {
        let mut pages = Vec::new();
        let mut page = Page::default();
        let mut y = self.draw_header(&mut page);
        y = self.draw_parties(&mut page, y + 14.0);
        y = self.draw_transport(&mut page, y + 12.0);
        y = draw_table_header(&mut page, y + 16.0);

        for (i, item) in self.line_items.iter().enumerate() {
            let desc = pdf::wrap(&item.stock_description, BODY, Font::Regular, COL_DESC_W);
            let cat = pdf::wrap(&item.category, BODY, Font::Regular, COL_CAT_W);
            let row_h = (desc.len().max(cat.len()).max(1) as f32) * LINE + 6.0;
            if y + row_h > CONTENT_BOTTOM {
                pages.push(std::mem::take(&mut page));
                y = draw_table_header(&mut page, MARGIN + 10.0);
            }
            let base = y + LINE;
            page.text(COL_NO_X, base, BODY, Font::Regular, &(i + 1).to_string());
            for (j, l) in desc.iter().enumerate() {
                page.text(COL_DESC_X, base + j as f32 * LINE, BODY, Font::Regular, l);
            }
            for (j, l) in cat.iter().enumerate() {
                page.text(COL_CAT_X, base + j as f32 * LINE, BODY, Font::Regular, l);
            }
            page.text_right(
                COL_SIZE_R,
                base,
                BODY,
                Font::Regular,
                &item.volume_in_size.to_string(),
            );
            page.text_right(
                COL_QTY_R,
                base,
                BODY,
                Font::Regular,
                &item.quantity.to_string(),
            );
            y += row_h;
            page.line(MARGIN, y, RIGHT, y, 0.3);
        }

        let total: i64 = self.line_items.iter().map(|li| li.quantity).sum();
        if y + 20.0 > CONTENT_BOTTOM {
            pages.push(std::mem::take(&mut page));
            y = MARGIN + 10.0;
        }
        page.text(COL_CAT_X, y + LINE, BODY, Font::Bold, "Total quantity");
        page.text_right(COL_QTY_R, y + LINE, BODY, Font::Bold, &total.to_string());
        y += 20.0;

        if y + SIGNATURE_BLOCK_H > CONTENT_BOTTOM {
            pages.push(std::mem::take(&mut page));
        }
        self.draw_signatures(&mut page);
        pages.push(page);

        let count = pages.len();
        for (i, p) in pages.iter_mut().enumerate() {
            self.draw_footer(p, i + 1, count);
        }
        pdf::render(&pages, &format!("Delivery challan {}", self.number()))
    }

    fn draw_header(&self, page: &mut Page) -> f32 {
        let mut y = MARGIN + 16.0;
        // The full name is in the FROM box; here it only has to clear the title.
        let name = pdf::wrap(&self.consignor.name, 16.0, Font::Bold, 300.0);
        page.text(MARGIN, y, 16.0, Font::Bold, name.first().map_or("", |s| s));
        for l in pdf::wrap(&self.consignor.address, BODY, Font::Regular, 300.0) {
            y += LINE;
            page.text(MARGIN, y, BODY, Font::Regular, &l);
        }

        let mut ry = MARGIN + 12.0;
        page.text_right(RIGHT, ry, 13.0, Font::Bold, "DELIVERY CHALLAN");
        ry += LINE + 2.0;
        page.text_right(
            RIGHT,
            ry,
            SMALL,
            Font::Regular,
            "Lorry receipt (LR / bilty)",
        );
        ry += LINE + 4.0;
        page.text_right(
            RIGHT,
            ry,
            BODY,
            Font::Bold,
            &format!("No. {}", self.number()),
        );
        ry += LINE;
        page.text_right(
            RIGHT,
            ry,
            BODY,
            Font::Regular,
            &format!("Date: {}", format_ist(self.issued_at)),
        );
        ry += LINE;
        page.text_right(
            RIGHT,
            ry,
            BODY,
            Font::Regular,
            &format!("Status: {}", self.status.as_str().replace('_', " ")),
        );

        y = y.max(ry) + 10.0;
        if self.status == DispatchStatus::Cancelled {
            page.text(
                MARGIN,
                y + 4.0,
                12.0,
                Font::Bold,
                "CANCELLED - this dispatch was called off. Not valid for transport.",
            );
            y += 14.0;
        }
        page.line(MARGIN, y, RIGHT, y, 1.2);
        y
    }

    fn draw_parties(&self, page: &mut Page, top: f32) -> f32 {
        let half = (RIGHT - MARGIN - 12.0) / 2.0;
        let names = |p: &ChallanParty| pdf::wrap(&p.name, BODY, Font::Bold, half - 16.0);
        let (left_name, right_name) = (names(&self.consignor), names(&self.consignee));
        let left = self.party_lines(&self.consignor, half - 16.0);
        let right = self.party_lines(&self.consignee, half - 16.0);
        let rows = |n: &Vec<String>, l: &Vec<String>| n.len().max(1) - 1 + l.len();
        let h = 30.0 + rows(&left_name, &left).max(rows(&right_name, &right)) as f32 * LINE;
        for (x, label, name, lines) in [
            (MARGIN, "FROM (CONSIGNOR)", &left_name, &left),
            (MARGIN + half + 12.0, "TO (CONSIGNEE)", &right_name, &right),
        ] {
            page.rect(x, top, half, h, 0.6);
            page.text(x + 8.0, top + 13.0, SMALL, Font::Bold, label);
            for (i, l) in name.iter().enumerate() {
                page.text(
                    x + 8.0,
                    top + 13.0 + LINE * (i as f32 + 1.0),
                    BODY,
                    Font::Bold,
                    l,
                );
            }
            let skip = name.len().max(1) - 1;
            for (i, l) in lines.iter().enumerate() {
                page.text(
                    x + 8.0,
                    top + 13.0 + LINE * ((skip + i) as f32 + 2.0),
                    BODY,
                    Font::Regular,
                    l,
                );
            }
        }
        top + h
    }

    /// Address and phone lines under a party's name.
    fn party_lines(&self, party: &ChallanParty, width: f32) -> Vec<String> {
        let mut lines = pdf::wrap(&party.address, BODY, Font::Regular, width);
        if let Some(phone) = party.phone.as_deref().filter(|p| !p.trim().is_empty()) {
            lines.push(format!("Phone: {phone}"));
        }
        lines
    }

    fn draw_transport(&self, page: &mut Page, top: f32) -> f32 {
        let source = match (self.vehicle_source, &self.transporter) {
            (VehicleSource::Hired, Some(vendor)) => format!("Hired from {vendor}"),
            (VehicleSource::Hired, None) => "Hired".to_string(),
            (VehicleSource::Own, _) => "Own fleet".to_string(),
        };
        let dash = |v: &Option<String>| {
            v.as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("-")
                .to_string()
        };
        let mut cells = vec![
            ("Vehicle no.", self.vehicle.clone()),
            ("Vehicle", source),
            ("Driver", dash(&self.driver_name)),
            ("Driver phone", dash(&self.driver_phone)),
            ("Licence no.", dash(&self.driver_license)),
        ];
        if let Some(stop) = self.stop_sequence {
            cells.push(("Trip stop", stop.to_string()));
        }
        let col_w = (RIGHT - MARGIN) / 3.0;
        let rows = cells.len().div_ceil(3);
        let h = rows as f32 * 30.0 + 6.0;
        page.rect(MARGIN, top, RIGHT - MARGIN, h, 0.6);
        for (i, (label, value)) in cells.iter().enumerate() {
            let x = MARGIN + 8.0 + (i % 3) as f32 * col_w;
            let y = top + 14.0 + (i / 3) as f32 * 30.0;
            page.text(x, y, SMALL, Font::Regular, label);
            let value = pdf::wrap(value, BODY, Font::Bold, col_w - 14.0)
                .into_iter()
                .next()
                .unwrap_or_default();
            page.text(x, y + LINE, BODY, Font::Bold, &value);
        }
        top + h
    }

    fn draw_signatures(&self, page: &mut Page) {
        let top = CONTENT_BOTTOM - SIGNATURE_BLOCK_H + 10.0;
        let w = (RIGHT - MARGIN - 24.0) / 3.0;
        let boxes = [
            (
                "Driver's signature",
                "Received the goods above for transport.",
            ),
            (
                "Receiver's signature & stamp",
                "Received the goods above in good condition.",
            ),
            ("Authorised signatory", ""),
        ];
        for (i, (label, note)) in boxes.iter().enumerate() {
            let x = MARGIN + i as f32 * (w + 12.0);
            page.rect(x, top, w, SIGNATURE_BLOCK_H - 10.0, 0.6);
            page.text(x + 6.0, top + 12.0, SMALL, Font::Bold, label);
            let note = if i == 2 {
                format!("For {}", self.consignor.name)
            } else {
                note.to_string()
            };
            for (j, l) in pdf::wrap(&note, SMALL, Font::Regular, w - 12.0)
                .iter()
                .take(2)
                .enumerate()
            {
                page.text(
                    x + 6.0,
                    top + 24.0 + j as f32 * 10.0,
                    SMALL,
                    Font::Regular,
                    l,
                );
            }
            let bottom = top + SIGNATURE_BLOCK_H - 10.0;
            page.text(x + 6.0, bottom - 22.0, SMALL, Font::Regular, "Name:");
            page.text(x + 6.0, bottom - 8.0, SMALL, Font::Regular, "Date:");
        }
    }

    fn draw_footer(&self, page: &mut Page, n: usize, of: usize) {
        let y = PAGE_HEIGHT - MARGIN + 8.0;
        page.line(MARGIN, y - 12.0, RIGHT, y - 12.0, 0.3);
        page.text(
            MARGIN,
            y,
            SMALL,
            Font::Regular,
            &format!("Dispatch {}", self.dispatch_id),
        );
        page.text_right(
            RIGHT,
            y,
            SMALL,
            Font::Regular,
            &format!("{} - page {n} of {of}", self.number()),
        );
    }
}

const MARGIN: f32 = 40.0;
const RIGHT: f32 = PAGE_WIDTH - MARGIN;
/// Lowest y the body may reach; below it sits the footer.
const CONTENT_BOTTOM: f32 = PAGE_HEIGHT - MARGIN - 20.0;
const SIGNATURE_BLOCK_H: f32 = 110.0;
const BODY: f32 = 10.0;
const SMALL: f32 = 8.0;
const LINE: f32 = 13.0;

const COL_NO_X: f32 = MARGIN + 6.0;
const COL_DESC_X: f32 = MARGIN + 30.0;
const COL_DESC_W: f32 = 230.0;
const COL_CAT_X: f32 = MARGIN + 270.0;
const COL_SIZE_R: f32 = MARGIN + 440.0;
/// Leaves room for the right-aligned size figure before `COL_SIZE_R`.
const COL_CAT_W: f32 = COL_SIZE_R - COL_CAT_X - 40.0;
const COL_QTY_R: f32 = RIGHT - 6.0;

/// The line-items table's shaded header row; returns the y below it.
fn draw_table_header(page: &mut Page, top: f32) -> f32 {
    page.shade(MARGIN, top, RIGHT - MARGIN, 18.0);
    let base = top + 12.5;
    page.text(COL_NO_X, base, SMALL, Font::Bold, "#");
    page.text(COL_DESC_X, base, SMALL, Font::Bold, "DESCRIPTION OF GOODS");
    page.text(COL_CAT_X, base, SMALL, Font::Bold, "CATEGORY");
    page.text_right(COL_SIZE_R, base, SMALL, Font::Bold, "SIZE / UNIT");
    page.text_right(COL_QTY_R, base, SMALL, Font::Bold, "QUANTITY");
    page.line(MARGIN, top + 18.0, RIGHT, top + 18.0, 0.6);
    top + 18.0
}

/// `12 Nov 2026, 10:30 IST` for a Unix timestamp.
fn format_ist(unix: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let local = unix + IST_OFFSET_SECS;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    format!(
        "{d} {} {y}, {:02}:{:02} IST",
        MONTHS[(m - 1) as usize],
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Howard Hinnant's `civil_from_days`; mirrors `billing::invoice`, kept
/// local like the copies there and in `vehicle::document`.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(desc: &str, qty: i64) -> DispatchLineItem {
        DispatchLineItem {
            stock_description: desc.to_string(),
            quantity: qty,
            volume_in_size: 4,
            category: "AAC blocks".to_string(),
        }
    }

    fn sample(items: Vec<DispatchLineItem>) -> Challan {
        Challan {
            dispatch_id: Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000000").unwrap(),
            // 2026-11-12 05:00 UTC = 10:30 IST
            issued_at: 1_794_459_600,
            status: DispatchStatus::InTransit,
            consignor: ChallanParty {
                name: "Shree Bricks (Chakan)".to_string(),
                address: "Plot 14, MIDC, Chakan, Pune 410501".to_string(),
                phone: None,
            },
            consignee: ChallanParty {
                name: "Patil Constructions".to_string(),
                address: "Survey 22, Hinjewadi Phase 2, Pune 411057".to_string(),
                phone: Some("+919800000000".to_string()),
            },
            vehicle: "MH12AB1234".to_string(),
            vehicle_source: VehicleSource::Own,
            transporter: None,
            driver_name: Some("Ramesh Pawar".to_string()),
            driver_phone: Some("+919811111111".to_string()),
            driver_license: Some("MH1220190001234".to_string()),
            stop_sequence: None,
            line_items: items,
        }
    }

    fn text_of(pdf: &[u8]) -> String {
        pdf.iter().map(|&b| b as char).collect()
    }

    #[test]
    fn test_number_and_file_name_use_the_short_dispatch_id() {
        let c = sample(vec![item("6 inch block", 10)]);
        assert_eq!(c.number(), "DC-1A2B3C4D");
        assert_eq!(c.file_name(), "challan-DC-1A2B3C4D.pdf");
    }

    #[test]
    fn test_format_ist() {
        assert_eq!(format_ist(1_794_459_600), "12 Nov 2026, 10:30 IST");
        // 23:00 UTC on 31 Dec is already the next day in IST.
        assert_eq!(format_ist(1_798_758_000), "1 Jan 2027, 04:30 IST");
    }

    #[test]
    fn test_pdf_shows_parties_vehicle_driver_items_and_signature_boxes() {
        let c = sample(vec![item("6 inch block", 10), item("4 inch block", 25)]);
        let text = text_of(&c.render_pdf());
        for expected in [
            "(DELIVERY CHALLAN)",
            "(No. DC-1A2B3C4D)",
            "(Date: 12 Nov 2026, 10:30 IST)",
            "(Status: IN TRANSIT)",
            "(Shree Bricks \\(Chakan\\))",
            "(Patil Constructions)",
            "(Phone: +919800000000)",
            "(MH12AB1234)",
            "(Own fleet)",
            "(Ramesh Pawar)",
            "(MH1220190001234)",
            "(6 inch block)",
            "(4 inch block)",
            "(35)",
            "(Driver's signature)",
            "(Receiver's signature & stamp)",
            "(For Shree Bricks \\(Chakan\\))",
            "(DC-1A2B3C4D - page 1 of 1)",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn test_hired_truck_names_the_transporter() {
        let mut c = sample(vec![item("6 inch block", 1)]);
        c.vehicle_source = VehicleSource::Hired;
        c.transporter = Some("Sai Roadlines".to_string());
        c.driver_license = None;
        let text = text_of(&c.render_pdf());
        assert!(text.contains("(Hired from Sai Roadlines)"));
        assert!(text.contains("(-)"), "a missing licence prints as a dash");
    }

    #[test]
    fn test_cancelled_dispatch_is_marked_not_valid() {
        let mut c = sample(vec![item("6 inch block", 1)]);
        c.status = DispatchStatus::Cancelled;
        let text = text_of(&c.render_pdf());
        assert!(text.contains("Not valid for transport"));
    }

    #[test]
    fn test_many_line_items_run_onto_more_pages() {
        let items = (1..=80).map(|i| item(&format!("Item {i}"), i)).collect();
        let c = sample(items);
        let text = text_of(&c.render_pdf());
        assert!(text.contains("/Count 3"), "80 rows need three pages");
        assert!(text.contains("(DC-1A2B3C4D - page 3 of 3)"));
        assert!(text.contains("(Item 80)"));
        // The header repeats on every page; the signatures appear once.
        assert_eq!(text.matches("(DESCRIPTION OF GOODS)").count(), 3);
        assert_eq!(text.matches("(Driver's signature)").count(), 1);
        assert!(text.contains(&format!("({})", (1..=80).sum::<i64>())));
    }

    /// Every `Tj` on the page as (left x, right x, baseline y, font size).
    fn drawn_spans(text: &str) -> Vec<(f32, f32, f32, f32)> {
        text.lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("BT /")?;
                let (head, s) = rest.split_once(" Td (")?;
                let s = s.strip_suffix(") Tj ET")?;
                let f: Vec<&str> = head.split(' ').collect();
                let font = if f[0] == "F2" {
                    Font::Bold
                } else {
                    Font::Regular
                };
                let (size, x, y): (f32, f32, f32) =
                    (f[1].parse().ok()?, f[3].parse().ok()?, f[4].parse().ok()?);
                let s = s
                    .replace("\\(", "(")
                    .replace("\\)", ")")
                    .replace("\\\\", "\\");
                Some((
                    x,
                    x + pdf::text_width(&s, size, font),
                    PAGE_HEIGHT - y,
                    size,
                ))
            })
            .collect()
    }

    #[test]
    fn test_long_category_org_and_party_names_stay_in_their_columns() {
        let mut it = item("6 inch block", 10);
        it.category = "AAC Blocks 600x200x150 mm (Siporex) ".repeat(3)[..100].to_string();
        let mut c = sample(vec![it]);
        c.consignor.name =
            "Shree Mauli Brickvision Industries Private Limited, Pune Ltd".to_string();
        assert_eq!(c.consignor.name.len(), 60);
        c.consignee.name =
            "Patil Constructions and Infrastructure Developers LLP Hinjewadi".to_string();
        let pdf = c.render_pdf();
        let spans = drawn_spans(&text_of(&pdf));
        assert!(spans.len() > 30, "parsed the content stream");

        for &(x0, x1, y, size) in &spans {
            assert!(x1 <= RIGHT + 0.5, "text at y={y} runs past the margin");
            if x0 == COL_CAT_X && size == BODY {
                assert!(
                    x1 <= COL_CAT_X + COL_CAT_W + 0.5,
                    "category too wide at y={y}"
                );
            }
        }
        // No two pieces of text on (about) the same line overlap.
        for (i, a) in spans.iter().enumerate() {
            for b in &spans[i + 1..] {
                if (a.2 - b.2).abs() < a.3.min(b.3) {
                    assert!(a.1 <= b.0 || b.1 <= a.0, "{a:?} overlaps {b:?}");
                }
            }
        }
    }
}
