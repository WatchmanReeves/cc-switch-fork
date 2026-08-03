//! Device-local evidence for Pi Skill deployments.

// Pi skill reconciliation consumes this ledger in a later contract-ordered commit.
#![allow(dead_code)]

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkillDeploymentMethod {
    Symlink,
    Copy,
}

impl SkillDeploymentMethod {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::Copy => "copy",
        }
    }
}

impl FromStr for SkillDeploymentMethod {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "symlink" => Ok(Self::Symlink),
            "copy" => Ok(Self::Copy),
            _ => Err(AppError::Database(format!(
                "unknown Pi Skill deployment method '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillDeployment {
    pub skill_id: String,
    pub destination: String,
    pub destination_key: String,
    pub method: SkillDeploymentMethod,
    pub source_identity: String,
    pub deployed_digest: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn decode_deployment(row: &rusqlite::Row<'_>) -> rusqlite::Result<SkillDeployment> {
    let method: String = row.get(3)?;
    let method = method.parse().map_err(|error: AppError| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(SkillDeployment {
        skill_id: row.get(0)?,
        destination: row.get(1)?,
        destination_key: row.get(2)?,
        method,
        source_identity: row.get(4)?,
        deployed_digest: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

impl Database {
    pub(crate) fn set_pi_skill_desired(
        &self,
        skill_id: &str,
        desired_enabled: bool,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        let changed = conn
            .execute(
                "UPDATE skills SET enabled_pi = ?1 WHERE id = ?2",
                params![desired_enabled, skill_id],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        if changed != 1 {
            return Err(AppError::Conflict(format!(
                "Pi Skill '{skill_id}' disappeared before desired state was saved"
            )));
        }
        Ok(())
    }

    pub(crate) fn get_pi_skill_deployment(
        &self,
        skill_id: &str,
        destination_key: &str,
    ) -> Result<Option<SkillDeployment>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT skill_id, destination, destination_key, method,
                    source_identity, deployed_digest, created_at, updated_at
             FROM skill_deployments
             WHERE app_type = 'pi' AND skill_id = ?1 AND destination_key = ?2",
            params![skill_id, destination_key],
            decode_deployment,
        )
        .optional()
        .map_err(|error| AppError::Database(error.to_string()))
    }

    pub(crate) fn get_pi_skill_deployments(
        &self,
        skill_id: &str,
    ) -> Result<Vec<SkillDeployment>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT skill_id, destination, destination_key, method,
                        source_identity, deployed_digest, created_at, updated_at
                 FROM skill_deployments
                 WHERE app_type = 'pi' AND skill_id = ?1
                 ORDER BY created_at, destination_key",
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        let rows = stmt
            .query_map([skill_id], decode_deployment)
            .map_err(|error| AppError::Database(error.to_string()))?;
        rows.map(|row| row.map_err(|error| AppError::Database(error.to_string())))
            .collect()
    }

    pub(crate) fn save_pi_skill_deployment(
        &self,
        deployment: &SkillDeployment,
    ) -> Result<(), AppError> {
        self.save_pi_skill_deployment_with_desired(deployment, None)
    }

    /// Commit ledger evidence and, for a user toggle, the desired Pi bit in
    /// the same SQLite transaction. Filesystem publication happens before
    /// this point; a failed transaction is therefore safe to compensate by
    /// restoring the staged destination without exposing split DB authority.
    pub(crate) fn save_pi_skill_deployment_with_desired(
        &self,
        deployment: &SkillDeployment,
        desired_enabled: Option<bool>,
    ) -> Result<(), AppError> {
        if deployment.skill_id.trim().is_empty()
            || deployment.destination.trim().is_empty()
            || deployment.destination_key.trim().is_empty()
            || deployment.source_identity.trim().is_empty()
        {
            return Err(AppError::Config(
                "Pi Skill deployment identity fields must be non-empty".to_string(),
            ));
        }
        let mut conn = lock_conn!(self.conn);
        let transaction = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        transaction
            .execute(
                "INSERT INTO skill_deployments (
                app_type, skill_id, destination, destination_key, method,
                source_identity, deployed_digest, created_at, updated_at
             ) VALUES ('pi', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(app_type, skill_id, destination_key) DO UPDATE SET
                destination = excluded.destination,
                method = excluded.method,
                source_identity = excluded.source_identity,
                deployed_digest = excluded.deployed_digest,
                updated_at = excluded.updated_at",
                params![
                    deployment.skill_id,
                    deployment.destination,
                    deployment.destination_key,
                    deployment.method.as_str(),
                    deployment.source_identity,
                    deployment.deployed_digest,
                    deployment.created_at,
                    deployment.updated_at,
                ],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        if let Some(desired_enabled) = desired_enabled {
            let changed = transaction
                .execute(
                    "UPDATE skills SET enabled_pi = ?1 WHERE id = ?2",
                    params![desired_enabled, deployment.skill_id],
                )
                .map_err(|error| AppError::Database(error.to_string()))?;
            if changed != 1 {
                return Err(AppError::Conflict(format!(
                    "Pi Skill '{}' disappeared before deployment commit",
                    deployment.skill_id
                )));
            }
        }
        transaction
            .commit()
            .map_err(|error| AppError::Database(error.to_string()))
    }

    pub(crate) fn delete_pi_skill_deployment(
        &self,
        skill_id: &str,
        destination_key: &str,
    ) -> Result<bool, AppError> {
        self.delete_pi_skill_deployment_with_desired(skill_id, destination_key, None)
    }

    pub(crate) fn delete_pi_skill_deployment_with_desired(
        &self,
        skill_id: &str,
        destination_key: &str,
        desired_enabled: Option<bool>,
    ) -> Result<bool, AppError> {
        let mut conn = lock_conn!(self.conn);
        let transaction = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        if let Some(desired_enabled) = desired_enabled {
            let changed = transaction
                .execute(
                    "UPDATE skills SET enabled_pi = ?1 WHERE id = ?2",
                    params![desired_enabled, skill_id],
                )
                .map_err(|error| AppError::Database(error.to_string()))?;
            if changed != 1 {
                return Err(AppError::Conflict(format!(
                    "Pi Skill '{skill_id}' disappeared before deployment cleanup"
                )));
            }
        }
        let removed = transaction
            .execute(
                "DELETE FROM skill_deployments
             WHERE app_type = 'pi' AND skill_id = ?1 AND destination_key = ?2",
                params![skill_id, destination_key],
            )
            .map_err(|error| AppError::Database(error.to_string()))?
            == 1;
        transaction
            .commit()
            .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(skill_id: &str, destination_key: &str) -> SkillDeployment {
        SkillDeployment {
            skill_id: skill_id.into(),
            destination: format!("/tmp/{destination_key}"),
            destination_key: destination_key.into(),
            method: SkillDeploymentMethod::Copy,
            source_identity: format!("source:{skill_id}"),
            deployed_digest: Some("sha256:initial".into()),
            created_at: 10,
            updated_at: 10,
        }
    }

    #[test]
    fn skill_ledger_preserves_created_at_and_rejects_destination_collision() -> Result<(), AppError>
    {
        let db = Database::memory()?;
        db.save_pi_skill_deployment(&deployment("one", "destination"))?;
        let mut updated = deployment("one", "destination");
        updated.updated_at = 20;
        updated.deployed_digest = Some("sha256:updated".into());
        db.save_pi_skill_deployment(&updated)?;
        let saved = db
            .get_pi_skill_deployment("one", "destination")?
            .expect("deployment");
        assert_eq!(saved.created_at, 10);
        assert_eq!(saved.updated_at, 20);
        assert_eq!(saved.deployed_digest.as_deref(), Some("sha256:updated"));

        assert!(db
            .save_pi_skill_deployment(&deployment("two", "destination"))
            .is_err());
        assert_eq!(db.get_pi_skill_deployments("one")?.len(), 1);
        assert!(db.delete_pi_skill_deployment("one", "destination")?);
        Ok(())
    }
}
