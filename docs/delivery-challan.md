# Delivery challan / lorry receipt

Tracked by GitHub issue #300 (from the 2026-11-11 competitive analysis,
#299): *"Printable delivery challan / LR (bilty) per dispatch as PDF, with
line items, vehicle, driver and a signature box."* Every plant hands the
driver a challan today; this lets the system print it instead of the
challan book.

## API

| Method | Path | Description |
|---|---|---|
| GET | `/api/dispatches/{id}/challan.pdf` | The dispatch's challan as an A4 PDF (`Content-Disposition: inline; filename="challan-DC-XXXXXXXX.pdf"`). Any role in the dispatch's org; `403` for another org's dispatch, `404` if it doesn't exist. |

It is generated on every request from the current records, so nothing new is
stored.

## What's on it

- **Header:** the org's name and address; the challan number `DC-` plus the
  first 8 characters of the dispatch id (the same short id customers see in
  SMS and email); the dispatch date in IST; the current status. A
  `CANCELLED` dispatch is marked "Not valid for transport".
- **From / To:** the org (consignor) and the customer (consignee) with
  address and, for the customer, phone.
- **Transport:** vehicle number; own fleet or "Hired from <vendor>"; driver
  name, phone and licence number; the stop number on a multi-stop trip.
- **Goods:** one row per line item (description, category, size per unit,
  quantity) and the total quantity. Long lists continue on further pages
  with the table header repeated.
- **Signature boxes:** driver, receiver (signature & stamp, "received in
  good condition") and the org's authorised signatory, each with Name and
  Date lines.

## Known limits

- **Driver of an own-fleet truck is the one assigned now.** A dispatch does
  not record who drove it, so a challan reprinted after the truck's driver
  changes shows the new driver. A hired truck's driver comes from the
  vehicle hire, which is fixed when the truck is assigned.
- **Latin-1 text only.** The PDF uses the built-in Helvetica fonts so no
  font file ships with the server; `₹` prints as `Rs.` and Devanagari as
  `?`. Hindi/Marathi challans need an embedded font (see #309).
- **No GST fields yet.** GSTIN, HSN codes and the e-way bill number belong
  on the challan once #301 adds them.

## Implementation

- `dispatch::pdf` — a small PDF 1.4 writer (text, lines, boxes, word wrap)
  with no new crate dependency.
- `dispatch::challan` — `Challan::for_dispatch` gathers the data,
  `Challan::render_pdf` lays it out.
- `server::routes::dispatches::dispatch_challan_pdf` — the route.
