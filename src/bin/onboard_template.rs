//! Write a starter onboarding manifest for a new plant from an industry
//! template. See docs/onboarding/templates.md.
//!
//!     cargo run --bin onboard_template -- --list
//!     cargo run --bin onboard_template -- --template building-materials \
//!         --org "Acme Blocks" [--plant "Plant 1"] [--out manifest.json]

use logistics_system::logistics::onboarding::{render, template_names, validate};
use std::process::ExitCode;

fn value_of(args: &[String], flag: &str) -> Option<String> {
    let position = args.iter().position(|a| a == flag)?;
    args.get(position + 1).cloned()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        println!("{}", template_names().join("\n"));
        return Ok(());
    }
    let template =
        value_of(&args, "--template").ok_or("--template NAME is required (see --list)")?;
    let org = value_of(&args, "--org").ok_or("--org \"Org Name\" is required")?;
    let manifest = render(&template, &org, value_of(&args, "--plant").as_deref())?;
    let problems = validate(&manifest);
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())? + "\n";
    match value_of(&args, "--out") {
        Some(path) => std::fs::write(&path, text).map_err(|e| format!("writing {path}: {e}")),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
