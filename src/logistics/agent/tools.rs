//! What agents can see and ask for. `org_id` always comes from the run, never
//! from the model, so no tool can reach another org's data. A tool that isn't
//! defined here can't be called, whatever the model is told.

use crate::logistics::agent::runtime::{BILLING, DISPATCH_DESK};
use crate::logistics::agent::store::{Risk, now_unix};
use crate::logistics::billing::invoice::{Invoice, PaymentStatus, is_valid_iso_date, today_iso};
use crate::logistics::customer::customer::Customer;
use crate::logistics::db::connection::DbConnection;
use crate::logistics::dispatch::dispatch::{DispatchOrder, DispatchStatus};
use crate::logistics::events;
use crate::logistics::godown::godown::Godown;
use crate::logistics::notification::notification::Notification;
use crate::logistics::vehicle::document::{ComplianceStatus, VehicleDocument};
use serde_json::{Value, json};
use uuid::Uuid;

const MAX_LISTED: usize = 50;
const MAX_TEXT_CHARS: usize = 500;
const WEEK: i64 = 7 * 86_400;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub risk: Risk,
    pub schema: fn() -> Value,
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn no_arguments() -> Value {
    object_schema(json!({}), &[])
}

fn dispatch_action_schema() -> Value {
    object_schema(
        json!({
            "dispatch_id": {"type": "string", "description": "The dispatch's id"},
            "reason": {"type": "string", "description": "Why, in one or two sentences, for the person reviewing this"},
        }),
        &["dispatch_id", "reason"],
    )
}

fn alert_schema() -> Value {
    object_schema(
        json!({
            "severity": {"type": "string", "enum": ["LOW", "MEDIUM", "HIGH"]},
            "subject": {"type": "string", "description": "What this is about, such as a dispatch id or vehicle number. Used to avoid repeat alerts"},
            "reason": {"type": "string", "description": "What's wrong and what a person should do"},
        }),
        &["severity", "subject", "reason"],
    )
}

fn invoice_schema() -> Value {
    object_schema(
        json!({
            "dispatch_id": {"type": "string", "description": "The delivered dispatch's id"},
            "amount": {"type": "integer", "description": "The freight charge in whole rupees"},
            "due_on": {"type": "string", "description": "Payment due date, YYYY-MM-DD"},
            "reason": {"type": "string", "description": "Where the amount comes from, for the person reviewing this"},
        }),
        &["dispatch_id", "amount", "due_on", "reason"],
    )
}

fn reminder_schema() -> Value {
    object_schema(
        json!({
            "invoice_id": {"type": "string", "description": "The overdue invoice's id"},
            "reason": {"type": "string", "description": "Why a reminder is due now, in one sentence"},
        }),
        &["invoice_id", "reason"],
    )
}

/// Names of the tools each agent gets. Anything not listed for an agent can't
/// be called by it, whatever the model is told.
const DESK_TOOLS: &[&str] = &[
    "list_open_dispatches",
    "list_compliance_issues",
    "list_low_stock",
    "confirm_dispatch",
    "cancel_dispatch",
    "raise_alert",
];
const BILLING_TOOLS: &[&str] = &[
    "list_billing_work",
    "raise_invoice",
    "send_reminder",
    "raise_alert",
];

/// The tools one agent may use. Unknown agents get none.
pub fn catalog_for(agent: &str) -> Vec<ToolDef> {
    let names = match agent {
        DISPATCH_DESK => DESK_TOOLS,
        BILLING => BILLING_TOOLS,
        _ => &[],
    };
    catalog()
        .into_iter()
        .filter(|t| names.contains(&t.name))
        .collect()
}

pub fn catalog() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_billing_work",
            description: "Today's date, delivered dispatches that have no invoice yet (with their line items and how long ago they were delivered), and unpaid invoices that are past due (amount, balance, due date). At most 50 of each.",
            risk: Risk::Read,
            schema: no_arguments,
        },
        ToolDef {
            name: "raise_invoice",
            description: "Raise the invoice for a delivered dispatch that doesn't have one, and email the customer. Money and a customer are involved, so a person always reviews it. Only when you know the amount; otherwise raise an alert.",
            risk: Risk::Sensitive,
            schema: invoice_schema,
        },
        ToolDef {
            name: "send_reminder",
            description: "Email the customer a reminder that an invoice is overdue. Only for an invoice listed as overdue. At most one per week per invoice.",
            risk: Risk::Operational,
            schema: reminder_schema,
        },
        ToolDef {
            name: "list_open_dispatches",
            description: "Every dispatch not yet finished: id, status, vehicle, line item count, hours since its last status change, and whether it's running late. Oldest first, at most 50.",
            risk: Risk::Read,
            schema: no_arguments,
        },
        ToolDef {
            name: "list_compliance_issues",
            description: "Vehicle documents that have expired or expire soon: vehicle, document type, expiry date and status.",
            risk: Risk::Read,
            schema: no_arguments,
        },
        ToolDef {
            name: "list_low_stock",
            description: "Godown stock below its reorder threshold: godown, item and quantity.",
            risk: Risk::Read,
            schema: no_arguments,
        },
        ToolDef {
            name: "confirm_dispatch",
            description: "Move a PENDING dispatch to CONFIRMED, meaning it will be fulfilled as booked. Refused when its vehicle has an expired document.",
            risk: Risk::Operational,
            schema: dispatch_action_schema,
        },
        ToolDef {
            name: "cancel_dispatch",
            description: "Cancel a dispatch that hasn't left yet. Hard to undo, so a person always reviews it. Only for a dispatch that can't go ahead.",
            risk: Risk::Sensitive,
            schema: dispatch_action_schema,
        },
        ToolDef {
            name: "raise_alert",
            description: "Tell the org's staff about something that needs a person, for anything no other tool can resolve.",
            risk: Risk::Internal,
            schema: alert_schema,
        },
    ]
}

/// The tool definitions as the Messages API wants them.
pub fn api_definitions(agent: &str) -> Vec<Value> {
    catalog_for(agent)
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": (t.schema)(),
                "strict": true,
            })
        })
        .collect()
}

fn string_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} is required"))
}

pub fn reason(input: &Value) -> Result<String, String> {
    Ok(string_arg(input, "reason")?
        .chars()
        .take(MAX_TEXT_CHARS)
        .collect())
}

fn owned_dispatch(org_id: Uuid, input: &Value) -> Result<DispatchOrder, String> {
    let id = Uuid::parse_str(string_arg(input, "dispatch_id")?)
        .map_err(|_| "dispatch_id isn't a valid id".to_string())?;
    match DispatchOrder::get_by_id(id).map_err(|e| e.to_string())? {
        Some(d) if d.org_id == org_id => Ok(d),
        _ => Err("No such dispatch in this organization".to_string()),
    }
}

fn owned_invoice(org_id: Uuid, input: &Value) -> Result<Invoice, String> {
    let id = Uuid::parse_str(string_arg(input, "invoice_id")?)
        .map_err(|_| "invoice_id isn't a valid id".to_string())?;
    match Invoice::get_by_id(id).map_err(|e| e.to_string())? {
        Some(i) if i.org_id == org_id => Ok(i),
        _ => Err("No such invoice in this organization".to_string()),
    }
}

fn expired_documents(org_id: Uuid, registration: &str) -> Result<Vec<VehicleDocument>, String> {
    Ok(VehicleDocument::list_by_org(org_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|d| d.vehicle_registration == registration && d.status == ComplianceStatus::Expired)
        .collect())
}

/// Check a write is allowed right now, and return what identifies it for
/// spotting repeats. Runs when an agent asks and again just before carrying it
/// out, because an approval can come much later.
pub fn validate(org_id: Uuid, name: &str, input: &Value) -> Result<String, String> {
    match name {
        "confirm_dispatch" => {
            let dispatch = owned_dispatch(org_id, input)?;
            if !dispatch.status.can_transition_to(DispatchStatus::Confirmed) {
                return Err(format!("Dispatch is {}, can't confirm it", dispatch.status));
            }
            if let Some(reg) = &dispatch.vehicle_registration_number {
                let expired = expired_documents(org_id, reg)?;
                if let Some(doc) = expired.first() {
                    return Err(format!(
                        "Vehicle {reg} has an expired {:?} (expired {}), so this dispatch can't be confirmed",
                        doc.doc_type, doc.expires_on
                    ));
                }
            }
            Ok(format!("confirm:{}", dispatch.id))
        }
        "cancel_dispatch" => {
            let dispatch = owned_dispatch(org_id, input)?;
            if !dispatch.status.can_transition_to(DispatchStatus::Cancelled) {
                return Err(format!("Dispatch is {}, can't cancel it", dispatch.status));
            }
            Ok(format!("cancel:{}", dispatch.id))
        }
        "raise_invoice" => {
            let dispatch = owned_dispatch(org_id, input)?;
            if dispatch.status != DispatchStatus::Delivered {
                return Err(format!(
                    "Dispatch is {}: only a delivered dispatch is invoiced",
                    dispatch.status
                ));
            }
            if Invoice::get_by_dispatch(dispatch.id)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Err("This dispatch already has an invoice".to_string());
            }
            if !input["amount"].as_i64().is_some_and(|a| a > 0) {
                return Err("amount must be a whole number above zero".to_string());
            }
            if !is_valid_iso_date(string_arg(input, "due_on")?) {
                return Err("due_on must be a date written YYYY-MM-DD".to_string());
            }
            Ok(format!("invoice:{}", dispatch.id))
        }
        "send_reminder" => {
            let invoice = owned_invoice(org_id, input)?;
            if invoice.status != PaymentStatus::Overdue {
                return Err("Only an overdue invoice gets a reminder".to_string());
            }
            // One reminder a week per invoice.
            Ok(format!("reminder:{}:{}", invoice.id, now_unix() / WEEK))
        }
        "raise_alert" => {
            let severity = string_arg(input, "severity")?;
            if !["LOW", "MEDIUM", "HIGH"].contains(&severity) {
                return Err("severity must be LOW, MEDIUM or HIGH".to_string());
            }
            let subject: String = string_arg(input, "subject")?
                .to_lowercase()
                .chars()
                .take(120)
                .collect();
            Ok(format!("alert:{subject}"))
        }
        other => Err(format!("{other} isn't an action")),
    }
}

/// Carry out a write. Goes through the same model code as the dashboard's
/// routes, so every domain rule still applies.
pub fn execute(org_id: Uuid, name: &str, input: &Value) -> Result<String, String> {
    validate(org_id, name, input)?;
    match name {
        "confirm_dispatch" | "cancel_dispatch" => {
            let mut dispatch = owned_dispatch(org_id, input)?;
            let next = if name == "confirm_dispatch" {
                DispatchStatus::Confirmed
            } else {
                DispatchStatus::Cancelled
            };
            dispatch
                .transition_to(next, None, None)
                .map_err(|e| e.to_string())?;
            Ok(format!("Dispatch {} is now {next}", dispatch.id))
        }
        "raise_invoice" => {
            let dispatch = owned_dispatch(org_id, input)?;
            let amount = input["amount"].as_i64().unwrap_or(0);
            let due_on = string_arg(input, "due_on")?;
            let invoice =
                Invoice::create(org_id, dispatch.id, dispatch.customer_id, amount, due_on)
                    .map_err(|e| e.to_string())?;
            // Best effort, as on the dashboard: the invoice is already saved.
            if let Err(e) = crate::logistics::server::routes::notify_invoice_raised(&invoice) {
                eprintln!(
                    "invoice {}: couldn't queue the customer email: {e}",
                    invoice.id
                );
            }
            Ok(format!("Invoice {} raised for Rs {amount}", invoice.id))
        }
        "send_reminder" => {
            let invoice = owned_invoice(org_id, input)?;
            queue_reminder(&invoice).map_err(|e| e.to_string())?;
            Ok(format!("Reminder queued for invoice {}", invoice.id))
        }
        // The alert is the recorded action itself.
        "raise_alert" => Ok("Alert raised".to_string()),
        other => Err(format!("{other} isn't an action")),
    }
}

fn queue_reminder(invoice: &Invoice) -> Result<(), Box<dyn std::error::Error>> {
    let Some(customer) = Customer::get_by_id(invoice.customer_id)? else {
        return Err("The invoice's customer no longer exists".into());
    };
    let mut conn = DbConnection::from_env().get_connection()?;
    Notification::prepare_tables(&mut conn)?;
    let mut tx = conn.start_transaction(mysql::TxOpts::default())?;
    let notifs = Notification::enqueue_invoice_reminder(
        &mut tx,
        invoice.org_id,
        invoice.dispatch_id,
        &customer,
        invoice.balance,
        &invoice.due_on,
    )?;
    tx.commit()?;
    Notification::index_best_effort(&notifs);
    events::worker::wake();
    Ok(())
}

/// Answer a read tool. Holds no customer names or contact details: agents don't
/// need them for this, so they aren't sent to the model.
pub fn read(org_id: Uuid, name: &str) -> Result<Value, String> {
    match name {
        "list_open_dispatches" => {
            let now = now_unix();
            let mut rows: Vec<(i64, Value)> = DispatchOrder::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|d| !d.status.is_terminal())
                .map(|d| {
                    let changed = d
                        .status_history
                        .last()
                        .map_or(d.dispatched_at, |e| e.changed_at);
                    let row = json!({
                        "id": d.id,
                        "status": d.status.as_str(),
                        "vehicle": d.vehicle_registration_number,
                        "line_items": d.line_items.len(),
                        "hours_in_status": (now - changed) / 3600,
                        "running_late": d.is_running_late(),
                    });
                    (changed, row)
                })
                .collect();
            let total = rows.len();
            rows.sort_by_key(|(changed, _)| *changed);
            let dispatches: Vec<Value> =
                rows.into_iter().take(MAX_LISTED).map(|(_, r)| r).collect();
            Ok(json!({"total": total, "dispatches": dispatches}))
        }
        "list_billing_work" => {
            let invoiced: std::collections::HashSet<Uuid> = Invoice::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .iter()
                .map(|i| i.dispatch_id)
                .collect();
            let now = now_unix();
            let to_invoice: Vec<Value> = DispatchOrder::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|d| d.status == DispatchStatus::Delivered && !invoiced.contains(&d.id))
                .take(MAX_LISTED)
                .map(|d| {
                    let delivered_at = d
                        .status_history
                        .iter()
                        .rev()
                        .find(|e| e.status == DispatchStatus::Delivered)
                        .map_or(d.dispatched_at, |e| e.changed_at);
                    let items: Vec<Value> = d
                        .line_items
                        .iter()
                        .map(|i| json!({"item": i.stock_description, "quantity": i.quantity}))
                        .collect();
                    json!({
                        "dispatch_id": d.id,
                        "days_since_delivery": (now - delivered_at) / 86_400,
                        "line_items": items,
                    })
                })
                .collect();
            let overdue: Vec<Value> = Invoice::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|i| i.status == PaymentStatus::Overdue)
                .take(MAX_LISTED)
                .map(|i| {
                    json!({
                        "invoice_id": i.id,
                        "amount": i.amount,
                        "balance": i.balance,
                        "due_on": i.due_on,
                    })
                })
                .collect();
            Ok(json!({
                "today": today_iso(),
                "delivered_without_invoice": to_invoice,
                "overdue_invoices": overdue,
            }))
        }
        "list_compliance_issues" => {
            let documents: Vec<Value> = VehicleDocument::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|d| d.status != ComplianceStatus::Valid)
                .take(MAX_LISTED)
                .map(|d| {
                    json!({
                        "vehicle": d.vehicle_registration,
                        "document": format!("{:?}", d.doc_type),
                        "expires_on": d.expires_on,
                        "status": format!("{:?}", d.status),
                    })
                })
                .collect();
            Ok(json!({"documents": documents}))
        }
        "list_low_stock" => {
            let items: Vec<Value> = Godown::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .iter()
                .flat_map(|g| {
                    g.stock.iter().filter(|s| s.below_threshold).map(|s| {
                        json!({"godown": g.name, "item": s.description, "quantity": s.quantity})
                    })
                })
                .take(MAX_LISTED)
                .collect();
            Ok(json!({"items": items}))
        }
        other => Err(format!("{other} isn't a read tool")),
    }
}
