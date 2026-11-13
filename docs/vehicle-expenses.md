# Vehicle expenses and cost per trip

Tracked by the `todo.org` item *"Fuel and trip expense logging per vehicle
(fuel fills, tolls, driver advance) with cost per trip in reports."*

## Model

`VehicleExpense { id, org_id, vehicle_registration, trip_id, expense_type,
amount, incurred_on, litres, notes }` (`src/logistics/vehicle/expense.rs`).

- `expense_type`: `FUEL`, `TOLL`, `DRIVER_ADVANCE` or `OTHER`.
- `amount` is whole currency units (like invoice amounts), `> 0`.
- `incurred_on` is ISO `YYYY-MM-DD`. `litres` is optional, for fuel fills.
- `trip_id` is optional: a `Trips` id, or the dispatch id for a standalone
  dispatch (its one-stop trip) — the same ids `GET /api/orgs/{id}/trips`
  returns. It is checked to belong to the caller's org but is not a foreign
  key. Untagged expenses still count towards the fleet total.
- Cascade-deleted with the vehicle and the org. Expenses are append/delete
  only (no edit): delete and re-log a wrong entry.

## API

| Method | Path | Description |
|---|---|---|
| GET | `/api/vehicles/{reg}/expenses` | A vehicle's expenses, newest first |
| POST | `/api/vehicles/{reg}/expenses` | Log one — `{ expense_type, amount, incurred_on, trip_id?, litres?, notes? }`. `400` bad amount/litres/date, `404` unknown vehicle or trip |
| DELETE | `/api/vehicle-expenses/{id}` | Delete one |
| GET | `/api/orgs/{id}/vehicle-expenses` | Every expense across the org |

No role restriction, matching vehicle documents and maintenance.

## Cost per trip in reports

`GET /api/orgs/{id}/reports` gains `trip_costs`:

| Field | Definition |
|---|---|
| `expense_total` | Σ all expenses, tagged or not |
| `untagged_total` | Σ expenses with no `trip_id` |
| `trips_with_expenses` | Trips with at least one tagged expense |
| `avg_cost_per_trip` | Σ tagged cost / `trips_with_expenses`, one decimal; `null` if none |
| `trips[]` | Per trip, most expensive first: `fuel`, `toll`, `driver_advance`, `other`, `total_cost`, `invoiced` (Σ invoices of its non-cancelled dispatches) and `margin` = `invoiced - total_cost` |

`margin` understates until every dispatch on the trip is invoiced.

## Known limitations

- No per-vehicle cost-per-km (no odometer telemetry; see
  `docs/vehicle-maintenance.md`).
- Hire freight is not an expense here; see `hired_transport` in
  `docs/reports.md`.
- No frontend yet (lives in logistics-frontend).
