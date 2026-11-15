# Freight billing

Tracked by the `todo.org` item *"Add freight billing: a cost per dispatch,
invoice generation, and a payment status (paid/pending/overdue) per
customer."*

## Model

Exactly **one invoice per dispatch**. Raising an invoice records the
dispatch's freight cost and starts the clock on payment.

```
Orgs ──1:N──▶ Dispatches ──1:1──▶ Invoices
                                  (amount, issued_on, due_on, paid_on?)
```

- `Invoice { id, org_id, dispatch_id, customer_id, amount, issued_on,
  due_on, paid_on?, status }` — `src/logistics/billing/invoice.rs`.
  `org_id` and `customer_id` are copied from the dispatch. `dispatch_id` is
  `NOT NULL UNIQUE`, and the table has `ON DELETE CASCADE` foreign keys to
  both `Orgs(id)` and `Dispatches(id)`.
- `amount` is a whole number in the deployment's currency (there is only
  one). It must be greater than zero.
- Dates are ISO `YYYY-MM-DD` strings, validated as real calendar dates on
  write and compared lexicographically (ISO dates sort chronologically).
  `issued_on` is stamped server-side when the invoice is raised; `due_on` is
  supplied by the caller; `paid_on` is stamped when the invoice is paid.
  The backend has no opinion on payment terms — it accepts whatever `due_on`
  the caller sends, validated only as a real calendar date. The dashboard's
  invoice form pre-fills `due_on` 30 days from today (a standard "net 30"),
  the common default commercial payment term, but the dispatcher can change
  it to match a customer-specific agreement before raising the invoice.
- **Payments**: an invoice can be paid in several instalments. Each is an
  `InvoicePayment { id, invoice_id, amount, mode, paid_on, reference? }` row
  (`InvoicePayments`, `ON DELETE CASCADE` from `Invoices`). `mode` is `CASH`,
  `UPI`, `CHEQUE` or `NEFT`; `reference` is a UPI txn id, cheque number or
  UTR. A payment must be > 0 and no more than the balance (`400` otherwise),
  and `paid_on` defaults to today. The invoice carries `amount_paid`
  (sum of payments) and `balance` (`amount - amount_paid`); `paid_on` on the
  invoice is stamped when the balance reaches zero. Invoices marked paid
  before payments existed have no rows and count as fully paid.
- `status` is **computed on every read**, never stored:
  - `PAID` — balance is zero.
  - `OVERDUE` — balance remains and `due_on` is before today.
  - `PARTIALLY_PAID` — some paid, balance remains, not yet due.
  - `PENDING` — nothing paid and not yet due.
- `Invoice::customer_summary(customer_id)` aggregates a customer's invoices
  into `{ invoice_count, total_outstanding, overdue_count, invoices }` where
  `total_outstanding` is the summed `balance` of every invoice.

The date maths uses `days_from_civil` / `civil_from_days` helpers local to
the module (mirroring `vehicle::document`), so the crate takes no date
dependency.

## API

| Method | Path | Description |
|---|---|---|
| POST | `/api/dispatches/{id}/invoice` | Raise an invoice for a dispatch — `{ amount, due_on }`. `400` on a bad amount/date, `409` if the dispatch is already invoiced |
| GET | `/api/dispatches/{id}/invoice` | The dispatch's invoice, or `404` if not yet invoiced |
| PUT | `/api/invoices/{id}` | Amend `amount` / `due_on`. `409` once the invoice is paid; `400` if the new amount isn't above what is already paid |
| POST | `/api/invoices/{id}/pay` | Pay the whole remaining balance in cash, dated today (idempotent — a no-op if already paid) |
| POST | `/api/invoices/{id}/payments` | Record a payment — `{ amount, mode, paid_on?, reference? }`. `201` with the updated invoice; `400` if it exceeds the balance |
| GET | `/api/invoices/{id}/payments` | The invoice's payments, oldest first |
| GET | `/api/orgs/{id}/invoices` | Every invoice for the org, most recently issued first |
| GET | `/api/customers/{id}/billing` | The customer's payment standing (outstanding total, overdue count, invoices) |

Ownership is checked the same way as the rest of the system: the
per-dispatch routes go through `load_owned_dispatch`, the by-id routes
through `load_owned_invoice` (checks the invoice's stored `org_id`), and the
customer route through `load_owned_customer`.

## Dashboard

- **Dispatches page** gains a **Billing** column. An uninvoiced dispatch
  shows an "Invoice" button that opens an inline amount + due-date form; an
  invoiced one shows the amount, a colour-coded status badge, and a "Mark
  paid" button while unpaid.
- **Customers page** gains a **Billing** column showing each customer's
  outstanding balance and an "N overdue" badge, computed client-side from a
  single `GET /api/orgs/{id}/invoices` call.

## Not yet built

- **UPI payment link in the invoice SMS** — needs a DLT template variable
  (#316) and a UPI payee id; the invoice email is unchanged for now.
