# GST fields and e-way bills (phase 1: manual entry)

Trucks carrying goods worth over Rs 50,000 need an e-way bill. Phase 1 only
**stores and returns** the numbers; the dashboard does the printing (the
backend has no PDF/HTML output). Generating the bill through the NIC e-way
bill API is phase 2 and not built.

## Fields

| Where | Field | Rule |
|---|---|---|
| `Orgs.gstin` / `Organization.gstin` | GSTIN | 15 letters/digits, upper-cased; optional |
| `Customers.gstin` / `Customer.gstin` | GSTIN | same |
| `StockCategories(org_id, name, hsn_code)` | HSN code per category | 2 to 8 digits |
| `Dispatches.eway_bill_no` / `DispatchOrder.eway_bill_no` | e-way bill number | exactly 12 digits; optional |

The GSTIN rule is the vehicle vendor one (`gst::normalize_gstin`); the GSTIN
checksum is **not** verified. Blank input is treated as "not set".

## HSN per category

Categories stay free text on `Stock.category` (see
[stock-categories.md](stock-categories.md)). The HSN lives in its own
`StockCategories` table, keyed by org and category name, so it is set once per
category and looked up by name. A category with no row simply has no HSN. It
is **not** snapshotted onto dispatch line items: the dashboard joins
`GET /api/stock-categories` with each line item's `category`.

## Endpoints

- `PUT /api/orgs/{id}` takes an optional `gstin`: omitted keeps the current
  one, an empty string clears it, a malformed one is a `400`.
- `POST /api/orgs/{id}/customers` takes an optional `gstin`;
  `PUT /api/customers/{id}/gstin` `{gstin}` sets or clears it (`null` or empty
  clears).
- `GET /api/stock-categories`, `PUT|DELETE /api/stock-categories/{name}/hsn`
  `{hsn_code}` (warehouse roles for writes).
- `PUT /api/dispatches/{id}/eway-bill` `{eway_bill_no}` (dispatch roles). The
  number is editable until the dispatch is `DELIVERED`, after which it is
  locked (`400`). It is returned on every dispatch read.

Existing databases get the new columns and table through the usual
`ensure_*` pattern on first use.
