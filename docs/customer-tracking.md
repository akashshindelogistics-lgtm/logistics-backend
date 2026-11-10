# Customer tracking link

Customers used to get "your order is on its way" with no way to see where it
was. Now each dispatch has a tracking link that opens without a login.

## How it works

- Every dispatch created from now on gets a random `tracking_token`
  (`Uuid::new_v4`, 122 random bits) in `Dispatches.tracking_token`. It is not
  the dispatch id and is never returned by the dashboard's dispatch routes.
- Dispatches created before this change have no token and get no link. They
  are not backfilled, because MySQL's `UUID()` is time-based and partly
  guessable.
- When `PUBLIC_APP_URL` is set (the dashboard's public address, e.g.
  `https://<name>.ngrok-free.dev`), the customer's **created** and
  **running late** messages end with:

  ```
  Track your order: <PUBLIC_APP_URL>/track/<token>
  ```

  The link is added in code (`notification::with_tracking_link`), both to the
  template and to the AI-written copy, so the model can't drop or change it.
  The URL travels to the consumer in the event payload (`tracking_url`). The
  driver's SMS never carries it, and neither does the delivered message.
- Unset `PUBLIC_APP_URL` means no link, exactly as before.

## The public route

`GET /api/public/track/{token}` takes no bearer token: the tracking token is
the credential. An unknown or malformed token returns `404` with the same
message either way.

It returns only what the customer needs:

| field | |
| --- | --- |
| `reference` | first 8 characters of the dispatch id, as quoted in the message |
| `shipper` | the organization's name |
| `status`, `status_history` | lifecycle so far |
| `dispatched_at`, `expected_by` | `expected_by` = dispatched + `PROMISED_DELIVERY_HOURS` (72h) |
| `delivered_at` | from the proof of delivery, if delivered |
| `vehicle_position` | `{latitude, longitude, updated_at}` of the vehicle's last fix, only while `LOADED` or `IN_TRANSIT` |

Customer name, address and contact details, line items, the vehicle number,
the driver and the proof-of-delivery receiver and photo are left out. A hired
truck that isn't in the org's fleet has no position.

## Frontend

logistics-frontend serves the public `/track/:token` page outside the login. It
shows the status, a timeline, the expected delivery time and a map of the
vehicle's last position.

## Deployment

Add `PUBLIC_APP_URL` to the API's environment (`~/.config/logistics-ngrok/api.env`
on the ngrok host, `deploy/.env` for docker compose) and restart the API.

## Not done

- No way to revoke or rotate a token. Every token stops showing the vehicle
  once the order is delivered.
- No rate limiting on the public route. Guessing a 122-bit token isn't
  practical, so this only matters for load.
- No "copy tracking link" button on the Dispatches page. The link is visible in
  the dispatch's notifications panel.
