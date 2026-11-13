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

/// Days of history kept when `LOCATION_HISTORY_RETENTION_DAYS` is unset.
pub const DEFAULT_RETENTION_DAYS: i64 = 30;

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

/// Append a fix. A repeat of the same vehicle/time/source (a phone resending
/// a batch) is ignored.
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
            (registration_number, latitude, longitude, recorded_at, source, accuracy_m, speed_mps)
         VALUES (:reg, :lat, :lng, :at, :source, :acc, :speed)",
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
/// most [`MAX_HISTORY_POINTS`].
pub fn between(
    registration_number: &str,
    from: i64,
    to: i64,
) -> Result<Vec<LocationHistoryPoint>, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    let rows: Vec<(f64, f64, i64, String, Option<f64>, Option<f64>)> = conn.exec(
        "SELECT latitude, longitude, recorded_at, source, accuracy_m, speed_mps
         FROM VehicleLocationHistory
         WHERE registration_number = :reg AND recorded_at BETWEEN :from AND :to
         ORDER BY recorded_at, id LIMIT :limit",
        params! { "reg" => registration_number, "from" => from, "to" => to, "limit" => MAX_HISTORY_POINTS },
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

/// Delete rows captured before `cutoff` (unix seconds). Returns how many.
pub fn prune_before(cutoff: i64) -> Result<u64, Box<dyn Error>> {
    let mut conn = DbConnection::from_env().get_connection()?;
    ensure_table(&mut conn)?;
    conn.exec_drop(
        "DELETE FROM VehicleLocationHistory WHERE recorded_at < :cutoff",
        params! { "cutoff" => cutoff },
    )?;
    Ok(conn.affected_rows())
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

    #[test]
    fn records_dedupes_filters_by_range_and_prunes() {
        let _db = TestDb::create();
        record("MH01", "gps_tracker", 1.0, 1.0, 100, None, None).unwrap();
        record("MH01", "driver_phone", 2.0, 2.0, 200, Some(5.0), Some(3.0)).unwrap();
        // A resent fix is not stored twice.
        record("MH01", "driver_phone", 2.0, 2.0, 200, Some(5.0), Some(3.0)).unwrap();
        record("MH01", "manual", 3.0, 3.0, 300, None, None).unwrap();
        record("MH02", "manual", 9.0, 9.0, 200, None, None).unwrap();

        let all = between("MH01", 0, 1000).unwrap();
        assert_eq!(
            all.iter().map(|p| p.recorded_at).collect::<Vec<_>>(),
            [100, 200, 300]
        );
        assert_eq!(all[1].accuracy_m, Some(5.0));
        assert_eq!(all[1].source, "driver_phone");

        let mid = between("MH01", 150, 250).unwrap();
        assert_eq!(mid.len(), 1);
        assert_eq!(mid[0].latitude, 2.0);

        assert_eq!(prune_before(250).unwrap(), 3);
        assert_eq!(between("MH01", 0, 1000).unwrap().len(), 1);
        assert!(between("MH02", 0, 1000).unwrap().is_empty());
    }
}
