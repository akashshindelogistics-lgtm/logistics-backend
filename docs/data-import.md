# CSV data import

`POST /api/orgs/{id}/import/{kind}` loads vehicles, drivers, customers or stock
from a CSV so an org can bring its existing data in at once. **Admin only**
(`403` for other roles or another org; unknown `kind` is `404`). The body is
`multipart/form-data` with one `file` field, like [file-uploads.md](file-uploads.md).

`GET /api/orgs/{id}/import/{kind}/template` downloads the CSV template (the
header row only, `<kind>-template.csv`) for any member. A template imports
cleanly as a file with zero rows; it carries no sample data.

| `kind` | Columns (`*` = required) |
| --- | --- |
| `vehicles` | `registration_number*, capacity*, unit, vehicle_type, driver_license_number` |
| `drivers` | `name*, license_number*, phone*` |
| `customers` | `name*, address*, phone, email, gstin` |
| `stock` | `godown_name*, description*, category, quantity*, volume_in_size, reorder_threshold` |

- Header names are case-insensitive; column order is free; unknown columns are
  ignored. Optional columns may be left out of the header entirely.
- `unit` is one of `MetricTon, Kg, Litre, Box, Pallet, Piece` (default
  `MetricTon`); `vehicle_type` one of `Truck, Tipper, Trailer, Tempo, Pickup,
  Tanker` (default `Truck`). Unlike the JSON API, an unknown value is a row
  error rather than a silent fallback.
- `capacity` ≥ 1; `quantity` ≥ 0; `volume_in_size` defaults to `1`;
  `reorder_threshold` and `category` (default `General`) are optional.
- Vehicles may reference an existing driver of the org by `driver_license_number`;
  stock rows reference a godown of the org by `godown_name` (case-insensitive).
  Import drivers/godowns first. Drivers already on a vehicle cannot be assigned
  again.
- Duplicates are rejected, never overwritten: an existing or repeated
  registration number (registration numbers are unique system-wide), licence
  number, customer name+address, or stock description within a godown. Stock
  rows also respect the godown's `max_capacity` cumulatively across the file.
- A leading `'` before `= + - @` (added by the [export](data-export.md) to
  defuse spreadsheet formulas) is stripped, so exported files import back.

## Dry run and all-or-nothing

Every row is validated before anything is written. With `?dry_run=true` nothing
is ever written; the response is `200` with an `ImportReport`:
`{kind, dry_run, total_rows, valid_rows, imported, errors: [{line, column, message}]}`.
`line` is the line of the file the row starts on (the header is line 1).

Without `dry_run`, a file with any row error writes **nothing** and returns
`422` with the same report; a clean file is inserted in a single transaction
(`imported` = rows written). Limits: 1,000 data rows and 1 MiB per file; a
missing required column, non-UTF-8 text or exceeding a limit is `400`.

Implementation: `import/csv_import.rs` (parser, validation, transactional write)
and `server/routes/imports.rs`. Imports use direct SQL inserts, so the
assistant's search index is not refreshed per row; run the assistant reindex
afterwards. Both routes are excluded from chat tools (`chat/catalog.rs`). The
dashboard import UI is in the frontend repo.
