//! What agents can see and ask for. `org_id` always comes from the run, never
//! from the model, so no tool can reach another org's data. A tool that isn't
//! defined here can't be called, whatever the model is told.

use crate::logistics::agent::store::{Risk, now_unix};
use crate::logistics::dispatch::dispatch::{DispatchOrder, DispatchStatus};
use crate::logistics::godown::godown::Godown;
use crate::logistics::vehicle::document::{ComplianceStatus, VehicleDocument};
use crate::logistics::vehicle::vehicle::Vehicle;
use serde_json::{Value, json};
use uuid::Uuid;

const MAX_LISTED: usize = 50;
const MAX_TEXT_CHARS: usize = 500;

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

fn advance_schema() -> Value {
    object_schema(
        json!({
            "dispatch_id": {"type": "string", "description": "The dispatch's id"},
            "status": {"type": "string", "enum": ["LOADED", "IN_TRANSIT"], "description": "The status to move to"},
            "reason": {"type": "string", "description": "The signal that shows this, for the person reviewing it"},
        }),
        &["dispatch_id", "status", "reason"],
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

pub fn catalog() -> Vec<ToolDef> {
    vec![
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
            name: "list_vehicle_locations",
            description: "Where the vehicles on open dispatches last reported from: registration, latitude, longitude, minutes since the fix and its source. Vehicles with no fix are listed without a position.",
            risk: Risk::Read,
            schema: no_arguments,
        },
        ToolDef {
            name: "list_available_vehicles",
            description: "The org's vehicles: registration, capacity, unit, type, whether a driver is assigned, whether it is on an open dispatch, and whether a document has expired.",
            risk: Risk::Read,
            schema: no_arguments,
        },
        ToolDef {
            name: "advance_dispatch",
            description: "Move a dispatch forward to LOADED or IN_TRANSIT, from a signal such as the driver's report or the vehicle's position. Never DELIVERED or RETURNED: those need proof or a person.",
            risk: Risk::Operational,
            schema: advance_schema,
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
pub fn api_definitions(allowed: &[&str]) -> Vec<Value> {
    catalog()
        .iter()
        .filter(|t| allowed.contains(&t.name))
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

fn expired_documents(org_id: Uuid, registration: &str) -> Result<Vec<VehicleDocument>, String> {
    Ok(VehicleDocument::list_by_org(org_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|d| d.vehicle_registration == registration && d.status == ComplianceStatus::Expired)
        .collect())
}

/// Agents may only report that goods are loaded or on the road. Delivery needs
/// proof and a return needs a person, so neither is offered.
fn advance_target(input: &Value) -> Result<DispatchStatus, String> {
    match string_arg(input, "status")? {
        "LOADED" => Ok(DispatchStatus::Loaded),
        "IN_TRANSIT" => Ok(DispatchStatus::InTransit),
        _ => Err("status must be LOADED or IN_TRANSIT".to_string()),
    }
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
        "advance_dispatch" => {
            let dispatch = owned_dispatch(org_id, input)?;
            let next = advance_target(input)?;
            if !dispatch.status.can_transition_to(next) {
                return Err(format!(
                    "Dispatch is {}, can't move it to {next}",
                    dispatch.status
                ));
            }
            Ok(format!("advance:{}:{next}", dispatch.id))
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
        "advance_dispatch" => {
            let mut dispatch = owned_dispatch(org_id, input)?;
            let next = advance_target(input)?;
            dispatch
                .transition_to(next, None, None)
                .map_err(|e| e.to_string())?;
            Ok(format!("Dispatch {} is now {next}", dispatch.id))
        }
        // The alert is the recorded action itself.
        "raise_alert" => Ok("Alert raised".to_string()),
        other => Err(format!("{other} isn't an action")),
    }
}

fn open_dispatch_vehicles(org_id: Uuid) -> Result<Vec<String>, String> {
    Ok(DispatchOrder::list_by_org(org_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|d| !d.status.is_terminal())
        .filter_map(|d| d.vehicle_registration_number)
        .collect())
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
        "list_vehicle_locations" => {
            let now = now_unix();
            let on_road = open_dispatch_vehicles(org_id)?;
            let vehicles: Vec<Value> = Vehicle::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|v| on_road.contains(&v.registration_number))
                .take(MAX_LISTED)
                .map(|v| match &v.location {
                    Some(l) => json!({
                        "vehicle": v.registration_number,
                        "latitude": l.latitude,
                        "longitude": l.longitude,
                        "minutes_since_fix": (now - l.timestamp) / 60,
                        "source": v.last_location_source,
                    }),
                    None => json!({"vehicle": v.registration_number, "latitude": null}),
                })
                .collect();
            Ok(json!({"vehicles": vehicles}))
        }
        "list_available_vehicles" => {
            let busy = open_dispatch_vehicles(org_id)?;
            let documents = VehicleDocument::list_by_org(org_id).map_err(|e| e.to_string())?;
            let vehicles: Vec<Value> = Vehicle::list_by_org(org_id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .take(MAX_LISTED)
                .map(|v| {
                    let expired = documents.iter().any(|d| {
                        d.vehicle_registration == v.registration_number
                            && d.status == ComplianceStatus::Expired
                    });
                    json!({
                        "vehicle": v.registration_number,
                        "capacity": v.capacity,
                        "unit": v.unit,
                        "type": v.vehicle_type,
                        "has_driver": v.assigned_driver_id.is_some(),
                        "on_open_dispatch": busy.contains(&v.registration_number),
                        "document_expired": expired,
                    })
                })
                .collect();
            Ok(json!({"vehicles": vehicles}))
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
