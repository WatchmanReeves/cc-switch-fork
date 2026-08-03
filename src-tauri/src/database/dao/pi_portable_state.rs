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
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::fmt::Write as _;
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
        let sql =
            fs::read_to_string(source_path).map_err(|error| AppError::io(source_path, error))?;
        self.import_sql_string(&append_pi_device_local_state(&sql, &local)?)
    }

    /// Import a cloud-sync snapshot while retaining this device's evidence.
    pub(crate) fn import_portable_sql_string_for_sync(
        &self,
        sql: &str,
    ) -> Result<String, AppError> {
        let local = self.capture_pi_device_local_state()?;
        self.import_sql_string_for_sync(&append_pi_device_local_state(sql, &local)?)
    }

    /// Fail closed before the legacy whole-database restore can import
    /// device-local Pi ownership evidence.
    ///
    /// Canonical binary restore hardening is a separate project. Until that
    /// boundary can retain the receiving device's ledgers atomically, binary
    /// restore is supported only when neither side carries ownership rows.
    pub(crate) fn ensure_binary_restore_has_no_pi_ownership(
        &self,
        filename: &str,
    ) -> Result<(), AppError> {
        if filename.contains("..")
            || filename.contains('/')
            || filename.contains('\\')
            || !filename.ends_with(".db")
        {
            return Err(AppError::InvalidInput(
                "Invalid backup filename".to_string(),
            ));
        }

        {
            let conn = lock_conn!(self.conn);
            if pi_device_local_rows_exist(&conn)? {
                return Err(binary_restore_ownership_error(
                    "当前数据库",
                    "current database",
                ));
            }
        }

        let backup_path = crate::config::get_app_config_dir()
            .join("backups")
            .join(filename);
        if !backup_path.exists() {
            return Err(AppError::InvalidInput(format!(
                "Backup file not found: {filename}"
            )));
        }
        let source = Connection::open_with_flags(
            &backup_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| AppError::Database(format!("无法只读检查备份: {error}")))?;
        if pi_device_local_rows_exist(&source)? {
            return Err(binary_restore_ownership_error(
                "所选备份",
                "selected backup",
            ));
        }
        Ok(())
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
}

fn pi_device_local_rows_exist(conn: &Connection) -> Result<bool, AppError> {
    let schema_object_exists = |name: &str| -> Result<bool, AppError> {
        conn.query_row(
            "SELECT 1 FROM sqlite_master
              WHERE name = ?1 COLLATE NOCASE",
            [name],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(|error| AppError::Database(error.to_string()))
    };

    let projections_exist = if schema_object_exists("pi_provider_projections")? {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pi_provider_projections LIMIT 1)",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| AppError::Database(error.to_string()))?
    } else {
        false
    };
    if projections_exist {
        return Ok(true);
    }

    if schema_object_exists("skill_deployments")? {
        return conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM skill_deployments
                     WHERE app_type = 'pi'
                     LIMIT 1
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| AppError::Database(error.to_string()));
    }
    Ok(false)
}

/// Merge receiving-device evidence into the same temporary database that the
/// generic importer validates and publishes. No live database replacement can
/// occur before these statements have succeeded.
fn append_pi_device_local_state(sql: &str, local: &PiDeviceLocalState) -> Result<String, AppError> {
    ensure_sql_append_boundary(sql)?;
    let mut merged = String::with_capacity(sql.len() + 1024);
    merged.push_str(sql);
    // The leading newline closes a trailing `--` comment; the standalone
    // semicolon terminates a final statement that omitted its delimiter.
    merged.push_str(
        "\n;\n\
         DROP TABLE IF EXISTS pi_provider_projections;\n\
         CREATE TABLE pi_provider_projections (\n\
             provider_id TEXT PRIMARY KEY,\n\
             provider_key TEXT NOT NULL UNIQUE,\n\
             created_at INTEGER NOT NULL,\n\
             updated_at INTEGER NOT NULL\n\
         );\n\
         DROP TABLE IF EXISTS skill_deployments;\n\
         CREATE TABLE skill_deployments (\n\
             app_type TEXT NOT NULL CHECK (app_type = 'pi'),\n\
             skill_id TEXT NOT NULL,\n\
             destination TEXT NOT NULL,\n\
             destination_key TEXT NOT NULL,\n\
             method TEXT NOT NULL CHECK (method IN ('symlink', 'copy')),\n\
             source_identity TEXT NOT NULL,\n\
             deployed_digest TEXT,\n\
             created_at INTEGER NOT NULL,\n\
             updated_at INTEGER NOT NULL,\n\
             PRIMARY KEY (app_type, skill_id, destination_key),\n\
             UNIQUE (app_type, destination_key)\n\
         );\n",
    );

    for projection in &local.projections {
        writeln!(
            merged,
            "INSERT INTO pi_provider_projections \
             (provider_id, provider_key, created_at, updated_at) \
             VALUES ({}, {}, {}, {});",
            sql_text(&projection.provider_id)?,
            sql_text(&projection.provider_key)?,
            projection.created_at,
            projection.updated_at,
        )
        .expect("writing to String cannot fail");
    }
    for deployment in &local.skill_deployments {
        writeln!(
            merged,
            "INSERT INTO skill_deployments \
             (app_type, skill_id, destination, destination_key, method, \
              source_identity, deployed_digest, created_at, updated_at) \
             VALUES ('pi', {}, {}, {}, {}, {}, {}, {}, {});",
            sql_text(&deployment.skill_id)?,
            sql_text(&deployment.destination)?,
            sql_text(&deployment.destination_key)?,
            sql_text(deployment.method.as_str())?,
            sql_text(&deployment.source_identity)?,
            sql_optional_text(deployment.deployed_digest.as_deref())?,
            deployment.created_at,
            deployment.updated_at,
        )
        .expect("writing to String cannot fail");
    }
    Ok(merged)
}

fn sql_optional_text(value: Option<&str>) -> Result<String, AppError> {
    value.map_or_else(|| Ok("NULL".to_string()), sql_text)
}

fn sql_text(value: &str) -> Result<String, AppError> {
    if value.contains('\0') {
        return Err(AppError::InvalidInput(
            "device-local Pi ownership text cannot contain NUL".to_string(),
        ));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

/// SQLite accepts an unterminated block comment at EOF. Reject such input so
/// it cannot swallow the receiving-device statements appended above. Other
/// unterminated quoted forms are rejected here as a clearer pre-publish error.
fn ensure_sql_append_boundary(sql: &str) -> Result<(), AppError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        SingleQuote,
        DoubleQuote,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let current = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Normal => match (current, next) {
                (b'\'', _) => state = State::SingleQuote,
                (b'"', _) => state = State::DoubleQuote,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 1;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 1;
                }
                _ => {}
            },
            State::SingleQuote if current == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 1;
                } else {
                    state = State::Normal;
                }
            }
            State::DoubleQuote if current == b'"' => {
                if next == Some(b'"') {
                    cursor += 1;
                } else {
                    state = State::Normal;
                }
            }
            State::Backtick if current == b'`' => {
                if next == Some(b'`') {
                    cursor += 1;
                } else {
                    state = State::Normal;
                }
            }
            State::Bracket if current == b']' => state = State::Normal,
            State::LineComment if current == b'\n' => state = State::Normal,
            State::BlockComment if current == b'*' && next == Some(b'/') => {
                state = State::Normal;
                cursor += 1;
            }
            _ => {}
        }
        cursor += 1;
    }

    if matches!(state, State::Normal | State::LineComment) {
        Ok(())
    } else {
        Err(AppError::InvalidInput(
            "portable SQL ends inside an unterminated quoted value or comment".to_string(),
        ))
    }
}

fn binary_restore_ownership_error(source_zh: &str, source_en: &str) -> AppError {
    AppError::localized(
        "pi.binary_restore_device_ownership_unsupported",
        format!(
            "为防止历史设备所有权记录覆盖当前 Pi 原生文件，{source_zh}含 Pi 所有权状态时不能使用数据库备份恢复；请改用可移植 SQL 导入"
        ),
        format!(
            "Binary database restore is unavailable because the {source_en} contains device-local Pi ownership state; use portable SQL import instead"
        ),
    )
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

    fn add_restore_poison_trigger(sql: &str) -> String {
        let insertion = sql
            .rfind("COMMIT;")
            .expect("CC Switch dump must contain a final COMMIT");
        let mut poisoned = sql.to_string();
        poisoned.insert_str(
            insertion,
            "CREATE TRIGGER poison_pi_restore \
             BEFORE INSERT ON pi_provider_projections \
             BEGIN SELECT RAISE(ABORT, 'poisoned local restore'); END;\n",
        );
        poisoned
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
        let remote_sql = add_restore_poison_trigger(&remote.export_sql_string()?);

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
        fs::write(
            &path,
            add_restore_poison_trigger(&remote.export_sql_string()?),
        )
        .expect("write SQL backup");

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

    #[test]
    fn binary_restore_guard_rejects_live_ownership_before_opening_the_backup(
    ) -> Result<(), AppError> {
        let db = Database::memory()?;
        db.claim_pi_projection_key("local-provider", "local-key")?;

        let error = db
            .ensure_binary_restore_has_no_pi_ownership("missing.db")
            .expect_err("live ownership must reject before inspecting a source");
        assert!(error.to_string().contains("portable SQL"));
        Ok(())
    }

    #[test]
    fn binary_restore_guard_rejects_backup_ownership_but_allows_empty_tables(
    ) -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let empty_path = temp.path().join("empty.db");
        let owned_path = temp.path().join("owned.db");
        for path in [&empty_path, &owned_path] {
            let conn =
                Connection::open(path).map_err(|error| AppError::Database(error.to_string()))?;
            conn.execute_batch(
                "CREATE TABLE PI_PROVIDER_PROJECTIONS (
                    provider_id TEXT PRIMARY KEY,
                    provider_key TEXT NOT NULL
                 );
                 CREATE TABLE SKILL_DEPLOYMENTS (
                    app_type TEXT NOT NULL
                 );",
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        }
        let owned =
            Connection::open(&owned_path).map_err(|error| AppError::Database(error.to_string()))?;
        owned
            .execute(
                "INSERT INTO pi_provider_projections (provider_id, provider_key)
                 VALUES ('historical-provider', 'native-key')",
                [],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;

        let empty = Connection::open_with_flags(
            &empty_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        assert!(!pi_device_local_rows_exist(&empty)?);
        let owned = Connection::open_with_flags(
            &owned_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        assert!(pi_device_local_rows_exist(&owned)?);
        drop(owned);

        let owned =
            Connection::open(&owned_path).map_err(|error| AppError::Database(error.to_string()))?;
        owned
            .execute("DELETE FROM pi_provider_projections", [])
            .map_err(|error| AppError::Database(error.to_string()))?;
        owned
            .execute("INSERT INTO skill_deployments (app_type) VALUES ('pi')", [])
            .map_err(|error| AppError::Database(error.to_string()))?;
        drop(owned);
        let skill_only = Connection::open_with_flags(
            &owned_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        assert!(pi_device_local_rows_exist(&skill_only)?);

        let view_backed =
            Connection::open_in_memory().map_err(|error| AppError::Database(error.to_string()))?;
        view_backed
            .execute_batch(
                "CREATE VIEW PI_PROVIDER_PROJECTIONS AS
                 SELECT 'view-provider' AS provider_id, 'view-key' AS provider_key;",
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        assert!(
            pi_device_local_rows_exist(&view_backed)?,
            "a reserved-name view must not bypass binary ownership rejection"
        );
        Ok(())
    }

    #[test]
    fn portable_import_rejects_an_unclosed_comment_before_live_replacement() -> Result<(), AppError>
    {
        let remote = Database::memory()?;
        seed_portable_provider(&remote)?;
        let malicious = format!("{}/*", remote.export_sql_string()?);

        let local = Database::memory()?;
        local.claim_pi_projection_key("local-provider", "local-key")?;
        let error = local
            .import_portable_sql_string_for_sync(&malicious)
            .expect_err("the appended ownership program must not be swallowed");
        assert!(error.to_string().contains("unterminated"));
        assert_eq!(
            local
                .get_pi_projection("local-provider")?
                .map(|projection| projection.provider_key),
            Some("local-key".to_string())
        );
        assert!(
            local
                .get_provider_by_id("portable-sentinel", "codex")?
                .is_none(),
            "the remote database must not be published"
        );
        Ok(())
    }
}
