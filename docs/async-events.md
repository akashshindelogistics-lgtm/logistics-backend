# Async events: publish, subscribe, consume

Slow or failure-prone side effects run outside the HTTP request. A request
records what happened; consumers do the follow-up work in the background, with
retries.

## Why

Before this change, creating a dispatch waited on up to two Claude calls and two
SMS/email sends, one after another and with no timeout. A trip did that for
every stop. A failed send was never retried, and a crash just after the dispatch
was saved lost its notifications.

## Why MySQL and not a broker

The deployment is one API container and one MySQL on a free-tier VM. A broker is
another service to run, and it cannot commit atomically with a MySQL write, so
it would bring back the lost-event window this design closes. MySQL 8.0's
`FOR UPDATE SKIP LOCKED` is enough for competing consumers. `events::queue` is
the only code that knows the storage, so a broker can replace it later.

## How it works

```
HTTP request                                  worker thread
  transaction                                   claim due rows (SKIP LOCKED)
    domain writes (dispatch, stock, hire)       consumer.handle(event)
    Notifications rows (template text)            AI copy (first attempt only)
    Events row + one EventDeliveries row          send, mark SENT per row
    per subscribed consumer                     done / retry with backoff / DEAD
  commit, wake a worker
```

- **`Events`** is what happened. `dedupe_key` (unique, optional) makes publishing
  an event at most once, which is how delay alerts avoid double sending.
- **`EventDeliveries`** tracks one consumer's progress on one event, so consumers
  fail and retry independently.
- **Delivery is at-least-once.** Consumers must tolerate a repeat. The notification
  consumer only touches rows still `QUEUED`, so a retry skips what was sent. A
  crash just after a provider accepts a message can still send one duplicate.
- **Retries:** 30s, doubling, capped at 1 hour, 6 attempts, then `DEAD`.
  A worker that dies mid-event loses its 5 minute lease and the event is reclaimed.
- **Workers are dedicated OS threads**, because the MySQL driver is blocking.
  They cannot stall HTTP workers.

## What runs where

| Work | Where | Why |
|---|---|---|
| Dispatch, stock, hire and status writes | Request, one transaction | The caller needs the result, and partial writes corrupt stock |
| `Notifications` rows | Request, same transaction | The dashboard shows them immediately, even with workers off |
| AI notification copy, SMS and email | `notification` consumer | Slow external calls |
| Delay alerts | Scanner publishes; consumer sends | One retrying send path |
| AI search indexing | Request, best effort, after commit | A local DB write: a queue would add writes and allow stale overwrites |
| `assistant/reindex` | Request | Returns a count the dashboard shows; going async needs a frontend change |
| GPS pings | Request | One small update |

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `EVENT_WORKERS_ENABLED` | off | `true` starts the consumers and the delay-alert scan |
| `EVENT_WORKER_COUNT` | `2` | Consumer threads, 1 to 16 |

Workers are **off unless enabled** on purpose. A plain `cargo run` points at the
local `logistics` database by default, and a worker there would claim real events
and message real customers. With workers off, requests behave normally and
notifications stay `QUEUED` until a worker runs. `deploy/docker-compose.yml`
enables them; any other long-running deployment must set the variable too.

Events published while no worker runs are not lost: the next worker picks them up.

## Adding a consumer

1. Implement `events::worker::Consumer`. It must be safe to run twice.
2. Add it to `events::worker::registered_consumers`.
3. Add its name to `EventType::subscribers` for each event it wants.
4. Publish inside the transaction that makes the change, and call
   `events::worker::wake()` after commit.

Run `ensure_table` calls **before** opening a transaction. MySQL implicitly commits
on `CREATE TABLE`, even `IF NOT EXISTS`, which would silently end the transaction.

Tests call `drain_once` on the test thread, because `TestDb`'s database override is
thread-local.

## Rollout plan

1. HTTP timeouts on every outbound call.
2. Transactions around dispatch creation, trips, status changes and hire assignment.
3. Queue, worker and retention.
4. Move notifications and delay alerts onto it.

Later: `assistant/reindex` as a job (with a frontend PR), proof-of-delivery photo
checks and SMS tracking links as consumers, and an SSE consumer for live dashboards.
