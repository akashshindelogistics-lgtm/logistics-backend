//! Industry onboarding templates: a starter manifest for a new plant, so it
//! does not have to be written by hand. See docs/onboarding/templates.md.

use serde_json::{Value, json};

/// Templates compiled into the binary, so they cannot drift from the code
/// that validates them.
pub const TEMPLATES: &[(&str, &str)] = &[(
    "building-materials",
    include_str!("../../../docs/onboarding/templates/building-materials.json"),
)];

/// The compliance papers every vehicle is expected to carry.
pub const COMPLIANCE_DOC_TYPES: &[&str] = &[
    "RegistrationCertificate",
    "Insurance",
    "PollutionCertificate",
    "FitnessCertificate",
    "Permit",
];

pub fn template_names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|(n, _)| *n).collect()
}

/// Volume of one piece in litres from its size in mm (1 L = 1,000,000 mm³),
/// the litre convention used throughout.
pub fn litres_from_size_mm(size: [u64; 3]) -> f64 {
    (size[0] * size[1] * size[2]) as f64 / 1_000_000.0
}

/// A starter manifest for `template`, with the org and plant names filled in.
pub fn render(template: &str, org_name: &str, plant_name: Option<&str>) -> Result<Value, String> {
    let (_, raw) = TEMPLATES
        .iter()
        .find(|(n, _)| *n == template)
        .ok_or_else(|| {
            format!(
                "unknown template '{template}' (available: {})",
                template_names().join(", ")
            )
        })?;
    if org_name.trim().is_empty() {
        return Err("org name must not be empty".to_string());
    }
    let mut manifest: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    manifest["org"]["name"] = json!(org_name.trim());
    if let Some(plant) = plant_name.map(str::trim).filter(|p| !p.is_empty()) {
        let old = manifest["godowns"][0]["name"]
            .as_str()
            .unwrap_or("")
            .to_string();
        manifest["godowns"][0]["name"] = json!(plant);
        if let Some(stock) = manifest["stock"].as_array_mut() {
            for s in stock {
                if s["godown"] == json!(old) {
                    s["godown"] = json!(plant);
                }
            }
        }
    }
    Ok(manifest)
}

/// Structural checks on a manifest: every stock row names a declared godown
/// and has exactly one way of stating its volume, sizes are positive, vehicle
/// documents use known types. Returns every problem found.
pub fn validate(manifest: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let names = |key: &str, field: &str| -> Vec<String> {
        manifest[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x[field].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    if manifest["org"]["name"]
        .as_str()
        .is_none_or(|n| n.trim().is_empty())
    {
        errors.push("org.name is required".to_string());
    }
    let godowns = names("godowns", "name");
    for s in manifest["stock"].as_array().into_iter().flatten() {
        let desc = s["description"].as_str().unwrap_or("?");
        match s["godown"].as_str() {
            Some(g) if godowns.iter().any(|x| x == g) => {}
            other => errors.push(format!("stock '{desc}': unknown godown {other:?}")),
        }
        let has_size = !s["size_mm"].is_null();
        let has_volume = !s["volume_in_size"].is_null();
        if has_size == has_volume {
            errors.push(format!(
                "stock '{desc}': give exactly one of size_mm or volume_in_size"
            ));
        }
        if has_size {
            let ok = s["size_mm"].as_array().is_some_and(|a| {
                a.len() == 3 && a.iter().all(|d| d.as_u64().is_some_and(|d| d > 0))
            });
            if !ok {
                errors.push(format!(
                    "stock '{desc}': size_mm must be [l, w, h] in mm, all > 0"
                ));
            }
        }
    }
    for v in manifest["vehicles"].as_array().into_iter().flatten() {
        let reg = v["registration_number"].as_str().unwrap_or("?");
        for d in v["documents"].as_array().into_iter().flatten() {
            let t = d["doc_type"].as_str().unwrap_or("");
            if !COMPLIANCE_DOC_TYPES.contains(&t) {
                errors.push(format!("vehicle {reg}: unknown doc_type '{t}'"));
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_renders_and_validates() {
        for name in template_names() {
            let m = render(name, "Acme Blocks", None).unwrap();
            assert_eq!(validate(&m), Vec::<String>::new(), "{name}");
        }
    }

    #[test]
    fn plant_name_flows_into_stock_rows() {
        let m = render("building-materials", "Acme", Some("Plant 2")).unwrap();
        assert_eq!(m["godowns"][0]["name"], "Plant 2");
        assert!(
            m["stock"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["godown"] == "Plant 2")
        );
        assert!(validate(&m).is_empty());
    }

    #[test]
    fn unknown_template_and_blank_org_are_rejected() {
        assert!(
            render("nope", "Acme", None)
                .unwrap_err()
                .contains("building-materials")
        );
        assert!(render("building-materials", "  ", None).is_err());
    }

    #[test]
    fn validate_reports_bad_rows() {
        let mut m = render("building-materials", "Acme", None).unwrap();
        m["stock"][0]["godown"] = json!("Nowhere");
        m["stock"][1]["volume_in_size"] = json!(2);
        m["stock"][2]["size_mm"] = json!([600, 0, 100]);
        m["vehicles"] = json!([{"registration_number":"MH1","documents":[{"doc_type":"Bogus"}]}]);
        assert_eq!(validate(&m).len(), 4);
    }

    #[test]
    fn litre_convention_matches_manifest_example() {
        assert_eq!(litres_from_size_mm([600, 200, 150]), 18.0);
    }
}
