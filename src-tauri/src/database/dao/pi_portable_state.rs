//! Portable SQL boundary for Pi's device-local ownership evidence.
//!
//! Provider and Skill configuration is portable. Exact `models.json` claims and
//! native Skill deployment receipts are not: they describe files on one device
//! and must never become ownership proof on another. This adapter keeps that
//! policy outside the frozen generic backup/restore implementation while giving
//! manual SQL, WebDAV, and S3 one shared boundary.

#[cfg(test)]
use crate::database::SkillDeploymentMethod;
use crate::database::{lock_conn, Database, PiProviderProjection, SkillDeployment};
use crate::error::AppError;
use rusqlite::params;
use std::fs;
use std::path::Path;

const DEVICE_LOCAL_INSERT_PREFIXES: &[&str] = &[
    "INSERT INTO \"pi_provider_projections\"",
    "INSERT INTO \"skill_deployments\"",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct PiDeviceLocalState {
    projections: Vec<PiProviderProjection>,
    skill_deployments: Vec<SkillDeployment>,
}

impl Database {
    /// Export a user-portable SQL backup without device-local ownership rows.
    pub(crate) fn export_portable_sql_string(&self) -> Result<String, AppError> {
        strip_device_local_insert_statements(&self.export_sql_string()?)
    }

    /// Export a cloud-sync SQL snapshot without device-local ownership rows.
    pub(crate) fn export_portable_sql_string_for_sync(&self) -> Result<String, AppError> {
        strip_device_local_insert_statements(&self.export_sql_string_for_sync()?)
    }

    pub(crate) fn export_portable_sql(&self, target_path: &Path) -> Result<(), AppError> {
        let dump = self.export_portable_sql_string()?;
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
        }
        crate::config::atomic_write(target_path, dump.as_bytes())
    }

    /// Import a user-portable SQL backup while retaining this device's evidence.
    pub(crate) fn import_portable_sql(&self, source_path: &Path) -> Result<String, AppError> {
        let local = self.capture_pi_device_local_state()?;
        let backup_id = self.import_sql(source_path)?;
        self.replace_pi_device_local_state(&local)?;
        Ok(backup_id)
    }

    /// Import a cloud-sync snapshot while retaining this device's evidence.
    pub(crate) fn import_portable_sql_string_for_sync(
        &self,
        sql: &str,
    ) -> Result<String, AppError> {
        let local = self.capture_pi_device_local_state()?;
        let backup_id = self.import_sql_string_for_sync(sql)?;
        self.replace_pi_device_local_state(&local)?;
        Ok(backup_id)
    }

    fn capture_pi_device_local_state(&self) -> Result<PiDeviceLocalState, AppError> {
        let conn = lock_conn!(self.conn);

        let projections = {
            let mut statement = conn
                .prepare(
                    "SELECT provider_id, provider_key, created_at, updated_at
                       FROM pi_provider_projections
                      ORDER BY provider_id",
                )
                .map_err(|error| AppError::Database(error.to_string()))?;
            let rows = statement
                .query_map([], |row| {
                    Ok(PiProviderProjection {
                        provider_id: row.get(0)?,
                        provider_key: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                })
                .map_err(|error| AppError::Database(error.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| AppError::Database(error.to_string()))?
        };

        let skill_deployments = {
            let mut statement = conn
                .prepare(
                    "SELECT skill_id, destination, destination_key, method,
                            source_identity, deployed_digest, created_at, updated_at
                       FROM skill_deployments
                      WHERE app_type = 'pi'
                      ORDER BY skill_id, destination_key",
                )
                .map_err(|error| AppError::Database(error.to_string()))?;
            let rows = statement
                .query_map([], |row| {
                    let method = row.get::<_, String>(3)?;
                    let method = method.parse().map_err(|error: AppError| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
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
                })
                .map_err(|error| AppError::Database(error.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| AppError::Database(error.to_string()))?
        };

        Ok(PiDeviceLocalState {
            projections,
            skill_deployments,
        })
    }

    fn replace_pi_device_local_state(&self, local: &PiDeviceLocalState) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let transaction = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        transaction
            .execute("DELETE FROM pi_provider_projections", [])
            .map_err(|error| AppError::Database(error.to_string()))?;
        transaction
            .execute("DELETE FROM skill_deployments WHERE app_type = 'pi'", [])
            .map_err(|error| AppError::Database(error.to_string()))?;

        for projection in &local.projections {
            transaction
                .execute(
                    "INSERT INTO pi_provider_projections
                        (provider_id, provider_key, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        projection.provider_id,
                        projection.provider_key,
                        projection.created_at,
                        projection.updated_at
                    ],
                )
                .map_err(|error| AppError::Database(error.to_string()))?;
        }
        for deployment in &local.skill_deployments {
            transaction
                .execute(
                    "INSERT INTO skill_deployments (
                        app_type, skill_id, destination, destination_key, method,
                        source_identity, deployed_digest, created_at, updated_at
                     ) VALUES ('pi', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        deployment.skill_id,
                        deployment.destination,
                        deployment.destination_key,
                        deployment.method.as_str(),
                        deployment.source_identity,
                        deployment.deployed_digest,
                        deployment.created_at,
                        deployment.updated_at
                    ],
                )
                .map_err(|error| AppError::Database(error.to_string()))?;
        }
        transaction
            .commit()
            .map_err(|error| AppError::Database(error.to_string()))
    }
}

/// Remove complete INSERT statements for the two device-local tables from SQL
/// generated by `Database::dump_sql`. Values may contain quotes, semicolons, or
/// newlines, so line filtering is insufficient; statement boundaries are found
/// only outside SQLite single-quoted literals. Unknown output fails closed.
fn strip_device_local_insert_statements(sql: &str) -> Result<String, AppError> {
    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len());
    let mut statement_start = 0;
    let mut cursor = 0;
    let mut in_string = false;

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' if in_string && bytes.get(cursor + 1) == Some(&b'\'') => {
                cursor += 2;
                continue;
            }
            b'\'' => in_string = !in_string,
            b';' if !in_string => {
                let statement_end = cursor + 1;
                let statement = &sql[statement_start..statement_end];
                if !DEVICE_LOCAL_INSERT_PREFIXES
                    .iter()
                    .any(|prefix| statement.trim_start().starts_with(prefix))
                {
                    output.push_str(statement);
                }
                statement_start = statement_end;
            }
            _ => {}
        }
        cursor += 1;
    }

    if in_string {
        return Err(AppError::Config(
            "portable SQL export ended inside a quoted value".to_string(),
        ));
    }
    output.push_str(&sql[statement_start..]);
    if DEVICE_LOCAL_INSERT_PREFIXES
        .iter()
        .any(|prefix| output.contains(prefix))
    {
        return Err(AppError::Config(
            "portable SQL export retained device-local Pi ownership rows".to_string(),
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(skill_id: &str, destination_key: &str) -> SkillDeployment {
        SkillDeployment {
            skill_id: skill_id.to_string(),
            destination: format!("/device/{destination_key}"),
            destination_key: destination_key.to_string(),
            method: SkillDeploymentMethod::Copy,
            source_identity: format!("path:/source/{skill_id};digest:sha256:one"),
            deployed_digest: Some("sha256:one".to_string()),
            created_at: 10,
            updated_at: 20,
        }
    }

    fn seed_portable_provider(db: &Database) -> Result<(), AppError> {
        let conn = lock_conn!(db.conn);
        conn.execute(
            "INSERT INTO providers
                (id, app_type, name, settings_config, meta, is_current)
             VALUES ('portable-sentinel', 'codex', 'Portable sentinel', '{}', '{}', 0)",
            [],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(())
    }

    #[test]
    fn portable_sql_scrubber_handles_multiline_quoted_values() -> Result<(), AppError> {
        let sql = concat!(
            "-- CC Switch SQLite 导出\n",
            "CREATE TABLE \"pi_provider_projections\" (value TEXT);\n",
            "INSERT INTO \"pi_provider_projections\" (value) VALUES ('one;\n",
            "two ''quoted''');\n",
            "CREATE TABLE \"providers\" (value TEXT);\n",
            "INSERT INTO \"providers\" (value) VALUES ('portable;\nvalue');\n",
            "COMMIT;\n",
        );

        let scrubbed = strip_device_local_insert_statements(sql)?;
        assert!(!scrubbed.contains("INSERT INTO \"pi_provider_projections\""));
        assert!(scrubbed.contains("CREATE TABLE \"pi_provider_projections\""));
        assert!(scrubbed.contains("INSERT INTO \"providers\""));
        assert!(scrubbed.contains("'portable;\nvalue'"));
        Ok(())
    }

    #[test]
    fn portable_exports_keep_schema_but_omit_device_evidence() -> Result<(), AppError> {
        let db = Database::memory()?;
        db.claim_pi_projection_key("local-provider", "local-key")?;
        db.save_pi_skill_deployment(&deployment("local-skill", "local-destination"))?;

        for exported in [
            db.export_portable_sql_string()?,
            db.export_portable_sql_string_for_sync()?,
        ] {
            assert!(exported.contains("CREATE TABLE pi_provider_projections"));
            assert!(exported.contains("CREATE TABLE skill_deployments"));
            assert!(!exported.contains("INSERT INTO \"pi_provider_projections\""));
            assert!(!exported.contains("INSERT INTO \"skill_deployments\""));
        }
        Ok(())
    }

    #[test]
    fn sync_import_discards_remote_evidence_and_restores_local_evidence() -> Result<(), AppError> {
        let remote = Database::memory()?;
        seed_portable_provider(&remote)?;
        remote.claim_pi_projection_key("remote-provider", "shared-key")?;
        remote.save_pi_skill_deployment(&deployment("remote-skill", "remote-destination"))?;
        // Model an older remote snapshot created before portable row scrubbing.
        let remote_sql = remote.export_sql_string()?;

        let local = Database::memory()?;
        local.claim_pi_projection_key("local-provider", "shared-key")?;
        local.save_pi_skill_deployment(&deployment("local-skill", "local-destination"))?;
        local.import_portable_sql_string_for_sync(&remote_sql)?;

        assert_eq!(
            local
                .get_pi_projection_for_key("shared-key")?
                .map(|projection| projection.provider_id),
            Some("local-provider".to_string())
        );
        assert!(local.get_pi_projection("remote-provider")?.is_none());
        assert_eq!(
            local.get_pi_skill_deployments("local-skill")?,
            vec![deployment("local-skill", "local-destination")]
        );
        assert!(local.get_pi_skill_deployments("remote-skill")?.is_empty());
        Ok(())
    }

    #[test]
    fn manual_sql_import_preserves_local_evidence() -> Result<(), AppError> {
        let remote = Database::memory()?;
        seed_portable_provider(&remote)?;
        remote.claim_pi_projection_key("remote-provider", "remote-key")?;
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("portable.sql");
        fs::write(&path, remote.export_sql_string()?).expect("write SQL backup");

        let local = Database::memory()?;
        local.claim_pi_projection_key("local-provider", "local-key")?;
        local.import_portable_sql(&path)?;

        assert!(local.get_pi_projection("remote-provider")?.is_none());
        assert_eq!(
            local
                .get_pi_projection("local-provider")?
                .map(|projection| projection.provider_key),
            Some("local-key".to_string())
        );
        Ok(())
    }
}
