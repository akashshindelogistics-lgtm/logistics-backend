# CSV data export

`GET /api/orgs/{id}/export/{dataset}` (bearer auth, must be your own org —
`403` otherwise) downloads one of the org's datasets as a CSV file, so the data
can always be taken out of the system (a data-handover promise for pilots).
Every role may export, like every other read. An unknown dataset is `404`.

| `dataset` | Columns |
| --- | --- |
| `dispatches` | `id, customer_id, status, vehicle_registration_number, vehicle_source, dispatched_date, total_quantity, line_items, delivered_date, trip_id, stop_sequence` |
| `stock` | one row per stock item per godown: `godown_id, godown_name, description, category, quantity, volume_in_size, reorder_threshold, below_threshold` |
| `invoices` | `id, dispatch_id, customer_id, amount, issued_on, due_on, paid_on, status` |
| `customers` | `id, name, address, phone, email` |
| `tally-invoices` | Tally XML: one *Sales* voucher per invoice — debit the customer's ledger, credit a `Freight Income` ledger; dated `issued_on`, number = invoice id. Invoices of cancelled dispatches are skipped |
| `tally-stock` | Tally XML: one *Delivery Note* voucher per dispatch whose stock has left (`LOADED`, `IN_TRANSIT`, `DELIVERED`) with an inventory entry per line item (stock item name = description, qty in `Nos`) |

The two `tally-*` datasets are `application/xml` (`<dataset>.xml`) in Tally's
import envelope (Gateway of Tally → Import Data → Vouchers), so the accountant
can keep GST books in Tally. Customer and stock-item ledgers/masters, and the
`Freight Income` ledger, must exist in Tally before importing (or enable
auto-creation of masters there). Vouchers whose customer no longer exists, or dispatches with no line items, are skipped (logged on the server). GST splits are not exported; `amount` is
posted as-is.

CSV responses are `text/csv` with a header row, CRLF line endings and a
`Content-Disposition: attachment; filename="<dataset>.csv"`. Dates are UTC
`YYYY-MM-DD`. Fields containing commas, quotes or newlines are quoted; text
starting with `=`, `+`, `-` or `@` gets a leading `'` so spreadsheets don't
evaluate it as a formula. This also applies to phone numbers written
with a leading `+` (for example `+91...`), which appear as `'+91...` in the
`customers` export.

Implementation: `reports/export.rs`, built from the existing `list_by_org`
methods (nothing is stored). The route is excluded from chat tools
(`chat/catalog.rs`). The dashboard download buttons live in the frontend repo.
