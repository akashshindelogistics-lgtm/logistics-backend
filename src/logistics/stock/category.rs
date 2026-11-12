use crate::logistics::db::connection::DbConnection;
use mysql::prelude::*;
use mysql::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use uuid::Uuid;

/// The GST HSN code an org assigns to one of its stock categories. Categories
/// themselves stay free text on `Stock.category`; this row is looked up by
/// category name, so the HSN is set once per category. See `docs/gst.md`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StockCategory {
    pub org_id: Uuid,
    pub name: String,
    pub hsn_code: String,
}

impl StockCategory {
    /// Create the `StockCategories` table if it does not exist. Kept in sync
    /// with `test_support::migrate`.
    pub fn ensure_table(conn: &mut mysql::PooledConn) -> Result<(), Box<dyn Error>> {
        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS StockCategories (
                org_id VARCHAR(36) NOT NULL,
                name VARCHAR(255) NOT NULL,
                hsn_code VARCHAR(8) NOT NULL,
                PRIMARY KEY (org_id, name),
                CONSTRAINT fk_stock_category_org FOREIGN KEY (org_id) REFERENCES Orgs(id) ON DELETE CASCADE
            )",
        )?;
        Ok(())
    }

    /// Set the HSN code for `name` in `org_id`, replacing any previous one.
    /// The name is trimmed and must not be blank; the code must be 2 to 8
    /// digits.
    pub fn set(org_id: Uuid, name: &str, hsn_code: &str) -> Result<Self, Box<dyn Error>> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("category name is required".into());
        }
        let hsn_code = crate::logistics::gst::normalize_hsn_code(Some(hsn_code.to_string()))?
            .ok_or("HSN code is required")?;
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        conn.exec_drop(
            "INSERT INTO StockCategories (org_id, name, hsn_code) VALUES (:org_id, :name, :hsn_code)
             ON DUPLICATE KEY UPDATE hsn_code = :hsn_code",
            params! { "org_id" => org_id.to_string(), "name" => &name, "hsn_code" => &hsn_code },
        )?;
        Ok(StockCategory {
            org_id,
            name,
            hsn_code,
        })
    }

    /// The HSN code for `name` in `org_id`, if one has been set.
    pub fn get(org_id: Uuid, name: &str) -> Result<Option<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        let hsn: Option<String> = conn.exec_first(
            "SELECT hsn_code FROM StockCategories WHERE org_id = :org_id AND name = :name",
            params! { "org_id" => org_id.to_string(), "name" => name.trim() },
        )?;
        Ok(hsn.map(|hsn_code| StockCategory {
            org_id,
            name: name.trim().to_string(),
            hsn_code,
        }))
    }

    /// Every category of `org_id` that has an HSN code, by name.
    pub fn list_by_org(org_id: Uuid) -> Result<Vec<Self>, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        Ok(conn.exec_map(
            "SELECT name, hsn_code FROM StockCategories WHERE org_id = :org_id ORDER BY name",
            params! { "org_id" => org_id.to_string() },
            |(name, hsn_code)| StockCategory {
                org_id,
                name,
                hsn_code,
            },
        )?)
    }

    /// Remove the HSN code for `name`. Returns whether one existed.
    pub fn delete(org_id: Uuid, name: &str) -> Result<bool, Box<dyn Error>> {
        let mut conn = DbConnection::from_env().get_connection()?;
        Self::ensure_table(&mut conn)?;
        conn.exec_drop(
            "DELETE FROM StockCategories WHERE org_id = :org_id AND name = :name",
            params! { "org_id" => org_id.to_string(), "name" => name.trim() },
        )?;
        Ok(conn.affected_rows() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logistics::orgs::orgs::Organization;
    use crate::logistics::test_support::TestDb;

    #[test]
    fn test_set_get_replace_list_and_delete() {
        let _db = TestDb::create();
        let org = Organization::create_organization("Org", "Addr").unwrap();
        assert!(StockCategory::get(org.id, "Cement").unwrap().is_none());

        StockCategory::set(org.id, " Cement ", "2523").unwrap();
        StockCategory::set(org.id, "Cement", "25232900").unwrap();
        StockCategory::set(org.id, "Steel", "7214").unwrap();

        assert_eq!(
            StockCategory::get(org.id, "Cement")
                .unwrap()
                .unwrap()
                .hsn_code,
            "25232900"
        );
        let names: Vec<_> = StockCategory::list_by_org(org.id)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["Cement", "Steel"]);

        assert!(StockCategory::delete(org.id, "Cement").unwrap());
        assert!(!StockCategory::delete(org.id, "Cement").unwrap());
        assert!(StockCategory::get(org.id, "Cement").unwrap().is_none());
    }

    #[test]
    fn test_set_rejects_bad_input_and_is_per_org() {
        let _db = TestDb::create();
        let a = Organization::create_organization("A", "Addr").unwrap();
        let b = Organization::create_organization("B", "Addr").unwrap();
        assert!(StockCategory::set(a.id, "  ", "2523").is_err());
        assert!(StockCategory::set(a.id, "Cement", "25").is_ok());
        assert!(StockCategory::set(a.id, "Cement", "abc").is_err());
        assert!(StockCategory::set(a.id, "Cement", "").is_err());
        assert!(StockCategory::get(b.id, "Cement").unwrap().is_none());
    }
}
