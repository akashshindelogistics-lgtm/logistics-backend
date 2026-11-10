use actix_cors::Cors;
use actix_web::{App, HttpServer};
use logistics_system::logistics::agent::runtime as agents;
use logistics_system::logistics::auth::auth::ensure_jwt_secret_configured;
use logistics_system::logistics::events::worker;
use logistics_system::logistics::server::routes::config_routes;

/// How often agents run when `AGENTS_ENABLED=true`, unless
/// `AGENT_INTERVAL_MINUTES` says otherwise. See `docs/agentic-workflow.md`.
const DEFAULT_AGENT_INTERVAL_MINUTES: u64 = 60;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Fail fast on a release build that has no usable JWT_SECRET, instead of
    // 500-ing on the first login.
    ensure_jwt_secret_configured();

    // Background consumers and scans run on their own threads; see
    // docs/async-events.md.
    worker::start_from_env();

    // Off unless asked for, because each run costs money. Each org also has its
    // own mode, which starts as OFF.
    if std::env::var("AGENTS_ENABLED").is_ok_and(|v| v.trim() == "true") {
        let minutes = std::env::var("AGENT_INTERVAL_MINUTES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|m| *m > 0)
            .unwrap_or(DEFAULT_AGENT_INTERVAL_MINUTES);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)).await;
                if let Err(err) = agents::run_due_orgs().await {
                    eprintln!("agent scheduler failed: {err}");
                }
            }
        });
    }

    // Bind host/port come from the environment so the same binary runs behind a
    // reverse proxy in a container (`0.0.0.0`, `PORT`) and directly on a
    // developer's machine (the `127.0.0.1:8080` defaults).
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    println!("Starting Logistics System REST API server at http://{host}:{port}...");

    // Permissive CORS: lets the published Swagger UI (or any other origin) call
    // this server directly. Auth uses bearer tokens, not cookies, so a wide-open
    // origin policy carries no credential risk.
    HttpServer::new(|| App::new().wrap(Cors::permissive()).configure(config_routes))
        .bind((host.as_str(), port))?
        .run()
        .await
}
