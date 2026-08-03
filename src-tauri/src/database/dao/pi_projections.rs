//! Device-local ownership ledger for exact keys in Pi's shared models.json.

// The projection writer is introduced in a later contract-ordered commit.
#![allow(dead_code)]

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use indexmap::IndexMap;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiProviderProjection {
    pub provider_id: String,
    pub provider_key: String,
    pub created_at: i64,
    pub updated_at: i64,
}

fn decode_projection(row: &rusqlite::Row<'_>) -> rusqlite::Result<PiProviderProjection> {
    Ok(PiProviderProjection {
        provider_id: row.get(0)?,
        provider_key: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
    })
}

impl Database {
    pub(crate) fn get_pi_projection(
        &self,
        provider_id: &str,
    ) -> Result<Option<PiProviderProjection>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT provider_id, provider_key, created_at, updated_at
             FROM pi_provider_projections WHERE provider_id = ?1",
            [provider_id],
            decode_projection,
        )
        .optional()
        .map_err(|error| AppError::Database(error.to_string()))
    }

    pub(crate) fn get_pi_projection_for_key(
        &self,
        provider_key: &str,
    ) -> Result<Option<PiProviderProjection>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT provider_id, provider_key, created_at, updated_at
             FROM pi_provider_projections WHERE provider_key = ?1",
            [provider_key],
            decode_projection,
        )
        .optional()
        .map_err(|error| AppError::Database(error.to_string()))
    }

    pub(crate) fn get_pi_projection_manifest(
        &self,
    ) -> Result<IndexMap<String, PiProviderProjection>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT provider_id, provider_key, created_at, updated_at
                 FROM pi_provider_projections ORDER BY provider_id",
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        let rows = stmt
            .query_map([], decode_projection)
            .map_err(|error| AppError::Database(error.to_string()))?;
        let mut manifest = IndexMap::new();
        for row in rows {
            let projection = row.map_err(|error| AppError::Database(error.to_string()))?;
            manifest.insert(projection.provider_id.clone(), projection);
        }
        Ok(manifest)
    }

    /// Claim an exact key. Existing exact claims are idempotent; either-side
    /// collisions fail and are never rewritten.
    pub(crate) fn claim_pi_projection_key(
        &self,
        provider_id: &str,
        provider_key: &str,
    ) -> Result<PiProviderProjection, AppError> {
        if provider_id.trim().is_empty() || provider_key.trim().is_empty() {
            return Err(AppError::Config(
                "Pi projection provider id and key must be non-empty".to_string(),
            ));
        }
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        let by_provider = tx
            .query_row(
                "SELECT provider_id, provider_key, created_at, updated_at
                 FROM pi_provider_projections WHERE provider_id = ?1",
                [provider_id],
                decode_projection,
            )
            .optional()
            .map_err(|error| AppError::Database(error.to_string()))?;
        if let Some(existing) = by_provider {
            if existing.provider_key != provider_key {
                return Err(AppError::Config(format!(
                    "Pi provider '{provider_id}' already owns key '{}', not '{provider_key}'",
                    existing.provider_key
                )));
            }
            tx.commit()
                .map_err(|error| AppError::Database(error.to_string()))?;
            return Ok(existing);
        }
        if let Some(existing_owner) = tx
            .query_row(
                "SELECT provider_id FROM pi_provider_projections WHERE provider_key = ?1",
                [provider_key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| AppError::Database(error.to_string()))?
        {
            return Err(AppError::Config(format!(
                "Pi key '{provider_key}' is already owned by provider '{existing_owner}'"
            )));
        }
        let now = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO pi_provider_projections
                (provider_id, provider_key, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)",
            params![provider_id, provider_key, now],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        tx.commit()
            .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(PiProviderProjection {
            provider_id: provider_id.to_string(),
            provider_key: provider_key.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    pub(crate) fn delete_pi_projection_key(
        &self,
        provider_id: &str,
        expected_key: &str,
    ) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let removed = conn
            .execute(
                "DELETE FROM pi_provider_projections
                 WHERE provider_id = ?1 AND provider_key = ?2",
                params![provider_id, expected_key],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        if removed == 0
            && conn
                .query_row(
                    "SELECT 1 FROM pi_provider_projections WHERE provider_id = ?1",
                    [provider_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|error| AppError::Database(error.to_string()))?
                .is_some()
        {
            return Err(AppError::Config(format!(
                "refusing to delete Pi projection '{provider_id}': expected key changed"
            )));
        }
        Ok(removed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_claims_are_exact_idempotent_and_collision_safe() -> Result<(), AppError> {
        let db = Database::memory()?;
        let first = db.claim_pi_projection_key("provider-a", "native-a")?;
        let repeated = db.claim_pi_projection_key("provider-a", "native-a")?;
        assert_eq!(first, repeated);
        assert!(db
            .claim_pi_projection_key("provider-a", "native-b")
            .is_err());
        assert!(db
            .claim_pi_projection_key("provider-b", "native-a")
            .is_err());
        assert_eq!(db.get_pi_projection_manifest()?.len(), 1);
        assert!(db.delete_pi_projection_key("provider-a", "wrong").is_err());
        assert!(db.get_pi_projection("provider-a")?.is_some());
        assert!(db.delete_pi_projection_key("provider-a", "native-a")?);
        assert!(db.get_pi_projection_for_key("native-a")?.is_none());
        Ok(())
    }
}
