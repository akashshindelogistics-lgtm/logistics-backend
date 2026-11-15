//! `VehicleLocationHistory`: every position a vehicle has reported, from any
//! source, so the trip map can draw a trail. Only the latest fix lives on
//! `Vehicle`. Rows older than the retention window are pruned. See
//! `docs/driver-phone-tracking.md`.

use crate::logistics::db::connection::DbConnection;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use utoipa::ToSchema;
use uuid::Uuid;

/// Days of history kept when `LOCATION_HISTORY_RETENTION_DAYS` is unset.
pub const DEFAULT_RETENTION_DAYS: i64 = 30;

/// Most rows one retention `DELETE` removes, so pruning a large backlog is
/// many short statements rather than one long transaction holding locks on
/// `idx_vlh_recorded_at` while fixes keep arriving.
pub const PRUNE_CHUNK_ROWS: u32 = 5000;

/// Most points one history request returns, so a wide range cannot build an
/// unbounded response. The oldest points in the range come first.
pub const MAX_HISTORY_POINTS: u32 = 5000;

/// One recorded position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LocationHistoryPoint {
    pub latitude: f64,
    pub longitude: f64,
    /// When the position was captured, unix seconds.
    pub recorded_at: i64,
    /// `driver_phone`, `gps_tracker` or `manual`.
    pub source: String,
    /// Horizontal accuracy in metres; only phones report it.
    pub accuracy_m: Option<f64>,
    /// Ground speed in metres per second; only phones report it.
    pub speed_mps: Option<f64>,
}

pub(crate) fn ensure_table(conn: &mut PooledConn) -> Result<(), Box<dyn Error>> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS VehicleLocationHistory (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            org_id VARCHAR(36) NOT NULL,
            registration_number VARCHAR(255) NOT NULL,
            latitude DOUBLE NOT NULL,
            longitude DOUBLE NOT NULL,
            recorded_at BIGINT NOT NULL,
            source VARCHAR(20) NOT NULL,
            accuracy_m DOUBLE DEFAULT NULL,
            speed_mps DOUBLE DEFAULT NULL,
            UNIQUE KEY uq_vlh_vehicle_time_source (registration_number, recorded_at, source),
            KEY idx_vlh_recorded_at (recorded_at)
        )",
    )?;
    Ok(())
}

/// Append a fix, stamped with the vehicle's current owner org. A repeat of the
/// same vehicle/time/source (a phone resending a batch) is ignored, and so is a
/// fix for a registration number with no `Vehicle` row.
pub fn record(
    registration_number: &str,
    source: &str,
    latitude: f64,
    longitude: f64,
    recorded_at: i64,
    accuracy_m: Option<f64>,
    speed_mps: Option<f64>,
) -> Result<(), Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    conn.exec_drop(
        "INSERT IGNORE INTO VehicleLocationHistory
            (org_id, registration_number, latitude, longitude, recorded_at, source, accuracy_m, speed_mps)
         SELECT org_id, :reg, :lat, :lng, :at, :source, :acc, :speed
         FROM Vehicle WHERE registration_number = :reg",
        params! {
            "reg" => registration_number,
            "lat" => latitude,
            "lng" => longitude,
            "at" => recorded_at,
            "source" => source,
            "acc" => accuracy_m,
            "speed" => speed_mps,
        },
    )?;
    Ok(())
}

/// Like [`record`], but a failure is logged rather than returned: the history
/// is secondary, and must never make a vehicle's location update fail.
pub fn record_best_effort(
    registration_number: &str,
    source: &str,
    latitude: f64,
    longitude: f64,
    recorded_at: i64,
    accuracy_m: Option<f64>,
    speed_mps: Option<f64>,
) {
    if let Err(err) = record(
        registration_number,
        source,
        latitude,
        longitude,
        recorded_at,
        accuracy_m,
        speed_mps,
    ) {
        eprintln!("location history write failed for {registration_number}: {err}");
    }
}

/// Points for one vehicle with `from <= recorded_at <= to`, oldest first, at
/// most [`MAX_HISTORY_POINTS`]. Only rows recorded while `org_id` owned the
/// vehicle, so a registration number re-added by another org starts empty.
pub fn between(
    registration_number: &str,
    org_id: Uuid,
    from: i64,
    to: i64,
) -> Result<Vec<LocationHistoryPoint>, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    let rows: Vec<(f64, f64, i64, String, Option<f64>, Option<f64>)> = conn.exec(
        "SELECT latitude, longitude, recorded_at, source, accuracy_m, speed_mps
         FROM VehicleLocationHistory
         WHERE registration_number = :reg AND org_id = :org
           AND recorded_at BETWEEN :from AND :to
         ORDER BY recorded_at, id LIMIT :limit",
        params! {
            "reg" => registration_number,
            "org" => org_id.to_string(),
            "from" => from,
            "to" => to,
            "limit" => MAX_HISTORY_POINTS,
        },
    )?;
    Ok(rows
        .into_iter()
        .map(
            |(latitude, longitude, recorded_at, source, accuracy_m, speed_mps)| {
                LocationHistoryPoint {
                    latitude,
                    longitude,
                    recorded_at,
                    source,
                    accuracy_m,
                    speed_mps,
                }
            },
        )
        .collect())
}

/// Delete every history row for one vehicle. Called when the vehicle is
/// removed, so the trail does not pass to whoever registers the number next.
pub fn delete_for_vehicle(registration_number: &str) -> Result<(), Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    conn.exec_drop(
        "DELETE FROM VehicleLocationHistory WHERE registration_number = :reg",
        params! { "reg" => registration_number },
    )?;
    Ok(())
}

/// Like [`delete_for_vehicle`], but a failure is logged rather than returned:
/// the vehicle is already gone, and `between` filters by org anyway.
pub fn delete_for_vehicle_best_effort(registration_number: &str) {
    if let Err(err) = delete_for_vehicle(registration_number) {
        eprintln!("location history delete failed for {registration_number}: {err}");
    }
}

/// Delete rows captured before `cutoff` (unix seconds), [`PRUNE_CHUNK_ROWS`]
/// at a time. Returns how many.
pub fn prune_before(cutoff: i64) -> Result<u64, Box<dyn Error>> {
    prune_before_in_chunks(cutoff, PRUNE_CHUNK_ROWS)
}

fn prune_before_in_chunks(cutoff: i64, chunk: u32) -> Result<u64, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    let mut total = 0;
    loop {
        conn.exec_drop(
            "DELETE FROM VehicleLocationHistory WHERE recorded_at < :cutoff
             ORDER BY recorded_at LIMIT :chunk",
            params! { "cutoff" => cutoff, "chunk" => chunk },
        )?;
        let deleted = conn.affected_rows();
        total += deleted;
        if deleted < u64::from(chunk) {
            return Ok(total);
        }
    }
}

/// Retention window in days, from `LOCATION_HISTORY_RETENTION_DAYS`
/// (positive integer), else [`DEFAULT_RETENTION_DAYS`].
pub fn retention_days() -> i64 {
    parse_retention_days(
        std::env::var("LOCATION_HISTORY_RETENTION_DAYS")
            .ok()
            .as_deref(),
    )
}

fn parse_retention_days(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// Prune everything older than the retention window.
pub fn prune_expired() -> Result<u64, Box<dyn Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    prune_before(now - retention_days() * 24 * 3600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::test_support::TestDb;

    #[test]
    fn retention_defaults_and_rejects_nonsense() {
        assert_eq!(parse_retention_days(None), 30);
        assert_eq!(parse_retention_days(Some("7")), 7);
        assert_eq!(parse_retention_days(Some(" 90 ")), 90);
        assert_eq!(parse_retention_days(Some("0")), 30);
        assert_eq!(parse_retention_days(Some("-3")), 30);
        assert_eq!(parse_retention_days(Some("abc")), 30);
    }

    fn org_with_vehicles(name: &str, regs: &[&str]) -> Uuid {
        use crate::logistics::orgs::orgs::Organization;
        use crate::logistics::vehicle::vehicle::{Unit, Vehicle};
        let org = Organization::create_organization(name, "1 Depot Rd").unwrap();
        for reg in regs {
            Vehicle::new(*reg, 10, Unit::MetricTon)
                .add_new_vehicle_to_org(&org)
                .unwrap();
        }
        org.id
    }

    #[test]
    fn records_dedupes_filters_by_range_and_prunes() {
        let _db = TestDb::create();
        let org = org_with_vehicles("History Unit Org", &["MH01", "MH02"]);
        record("MH01", "gps_tracker", 1.0, 1.0, 100, None, None).unwrap();
        record("MH01", "driver_phone", 2.0, 2.0, 200, Some(5.0), Some(3.0)).unwrap();
        // A resent fix is not stored twice.
        record("MH01", "driver_phone", 2.0, 2.0, 200, Some(5.0), Some(3.0)).unwrap();
        record("MH01", "manual", 3.0, 3.0, 300, None, None).unwrap();
        record("MH02", "manual", 9.0, 9.0, 200, None, None).unwrap();
        // No such vehicle: nothing to own the row, so nothing is stored.
        record("NOPE", "manual", 9.0, 9.0, 200, None, None).unwrap();

        let all = between("MH01", org, 0, 1000).unwrap();
        assert_eq!(
            all.iter().map(|p| p.recorded_at).collect::<Vec<_>>(),
            [100, 200, 300]
        );
        assert_eq!(all[1].accuracy_m, Some(5.0));
        assert_eq!(all[1].source, "driver_phone");

        let mid = between("MH01", org, 150, 250).unwrap();
        assert_eq!(mid.len(), 1);
        assert_eq!(mid[0].latitude, 2.0);

        // Another org sees none of it.
        assert!(between("MH01", Uuid::new_v4(), 0, 1000).unwrap().is_empty());

        assert_eq!(prune_before(250).unwrap(), 3);
        assert_eq!(between("MH01", org, 0, 1000).unwrap().len(), 1);
        assert!(between("MH02", org, 0, 1000).unwrap().is_empty());

        delete_for_vehicle("MH01").unwrap();
        assert!(between("MH01", org, 0, 1000).unwrap().is_empty());
    }

    #[test]
    fn prune_deletes_a_backlog_in_chunks() {
        let _db = TestDb::create();
        let org = org_with_vehicles("History Chunk Org", &["MH03"]);
        for at in 1..=7 {
            record("MH03", "manual", 1.0, 1.0, at, None, None).unwrap();
        }
        record("MH03", "manual", 1.0, 1.0, 100, None, None).unwrap();
        // Seven expired rows, three per statement: 3 + 3 + 1, then it stops.
        assert_eq!(prune_before_in_chunks(50, 3).unwrap(), 7);
        // An exact multiple of the chunk ends on an empty statement.
        record("MH03", "manual", 1.0, 1.0, 60, None, None).unwrap();
        record("MH03", "manual", 1.0, 1.0, 61, None, None).unwrap();
        assert_eq!(prune_before_in_chunks(99, 2).unwrap(), 2);
        let left = between("MH03", org, 0, 1000).unwrap();
        assert_eq!(
            left.iter().map(|p| p.recorded_at).collect::<Vec<_>>(),
            [100]
        );
    }
}
