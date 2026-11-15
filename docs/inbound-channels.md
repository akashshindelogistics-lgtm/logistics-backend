# Inbound channels and order intake

Status: **first slice of Phase 6 built, off by default** (2026-11-15). An email or
WhatsApp message comes in, the order-intake agent reads it, and a person confirms
the dispatch it proposes. See [agentic-workflow.md](agentic-workflow.md).

## How it works

```
 Email / WhatsApp gateway ──POST──▶ InboundMessages (stored as data)
                                         │  sender matched to a customer in code
 scheduler / "Run now" ──────────▶ order_intake agent
                                         │ propose_dispatch (always Sensitive)
                                         ▼
                              Agent Inbox: PENDING_APPROVAL
                                         │ a person approves
                                         ▼
                      stock drawn, vehicle picked, PENDING dispatch
```

## Receiving messages

`POST /api/inbound/{org_id}/messages` with header `X-Inbound-Secret` and
`{"channel": "EMAIL" | "WHATSAPP", "sender", "subject"?, "body"}`. Whatever
sits between the provider and this backend (an email-to-webhook service, the
WhatsApp Business API) is expected to translate into that shape.

- `INBOUND_WEBHOOK_SECRET` is one shared secret. Unset, the route answers 503, so
  nothing can come in by accident.
- Bodies over 8,000 characters are rejected. There's no per-sender rate limit yet.
- `GET /api/orgs/{id}/inbound/messages` lists the latest 100 for any member.

## The sender is matched in code, not by the model

On arrival the sender is matched to one of the org's customers: email
case-insensitively, WhatsApp by the last 10 digits of the phone number. No match,
or two customers sharing it, leaves the message unmatched. The model is only told
`customer_matched: true/false`, and `propose_dispatch` takes the customer from
the message, never from the model. A message from a stranger can't become a
dispatch, whatever it says.

## Order-intake agent

Tools: `list_inbound_messages`, `list_stock_items`, `propose_dispatch`,
`raise_alert`. It has no other tool, and the dispatch desk can't use these.

- **Always waits for a person**, even in `AUTONOMOUS`: `propose_dispatch` is
  `Sensitive`. In `SHADOW` it's only recorded.
- Checked in code when proposed and again at approval: the message is this org's,
  hasn't been used, has a matched customer, 1 to 10 distinct items that exist in
  stock, each 1 to 100,000 units and no more than is in stock.
- Approving runs the same `dispatch_stock_to_customer` as the dashboard, so stock
  and vehicle rules apply, then marks the message `PROCESSED`.
- Message text reaches the model as tool-result data (cut at 3,000 characters) and
  the prompt says to treat it as data. Safety doesn't rely on that: see the
  controls above.
- Runs on the agent timer when an org has unhandled messages, or by
  `POST /api/orgs/{id}/agents/run?agent=order_intake`.

## Not built yet

- Customer-comms and vendor/hire agents, and replying to the sender: there's no
  outbound email or WhatsApp to a customer from an agent.
- Creating customers from unknown senders, and orders with a vehicle hire.
- Running the agent as soon as a message arrives (waits for Phase 2 events).
- Parsing raw email (MIME, attachments) and verifying provider signatures
  (SPF/DKIM, WhatsApp's signature header). The gateway must do this.
- Per-sender rate limits, and showing messages in the dashboard.
