//! 提示词数据访问对象
//!
//! 提供提示词（Prompt）的 CRUD 操作。

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::prompt::Prompt;
use indexmap::IndexMap;
use rusqlite::{params, Connection, Transaction};

fn query_prompts(conn: &Connection, app_type: &str) -> Result<IndexMap<String, Prompt>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, content, description, enabled, created_at, updated_at
             FROM prompts WHERE app_type = ?1
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

    let prompt_iter = stmt
        .query_map(params![app_type], |row| {
            let id: String = row.get(0)?;
            let name: String = row.get(1)?;
            let content: String = row.get(2)?;
            let description: Option<String> = row.get(3)?;
            let enabled: bool = row.get(4)?;
            let created_at: Option<i64> = row.get(5)?;
            let updated_at: Option<i64> = row.get(6)?;

            Ok((
                id.clone(),
                Prompt {
                    id,
                    name,
                    content,
                    description,
                    enabled,
                    created_at,
                    updated_at,
                },
            ))
        })
        .map_err(|e| AppError::Database(e.to_string()))?;

    let mut prompts = IndexMap::new();
    for prompt_res in prompt_iter {
        let (id, prompt) = prompt_res.map_err(|e| AppError::Database(e.to_string()))?;
        prompts.insert(id, prompt);
    }
    Ok(prompts)
}

fn validate_prompt_selection(prompts: &IndexMap<String, Prompt>) -> Result<(), AppError> {
    if prompts.values().filter(|prompt| prompt.enabled).count() > 1 {
        return Err(AppError::InvalidInput(
            "at most one prompt may be enabled for an app".to_string(),
        ));
    }
    Ok(())
}

fn replace_prompt_rows(
    transaction: &Transaction<'_>,
    app_type: &str,
    prompts: &IndexMap<String, Prompt>,
) -> Result<(), AppError> {
    transaction
        .execute("DELETE FROM prompts WHERE app_type = ?1", [app_type])
        .map_err(|error| AppError::Database(error.to_string()))?;
    let mut statement = transaction
        .prepare(
            "INSERT OR REPLACE INTO prompts (
                id, app_type, name, content, description, enabled, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
    for prompt in prompts.values() {
        statement
            .execute(params![
                prompt.id,
                app_type,
                prompt.name,
                prompt.content,
                prompt.description,
                prompt.enabled,
                prompt.created_at,
                prompt.updated_at,
            ])
            .map_err(|error| AppError::Database(error.to_string()))?;
    }
    Ok(())
}

fn prompt_libraries_equal(
    left: &IndexMap<String, Prompt>,
    right: &IndexMap<String, Prompt>,
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(id, prompt)| right.get(id) == Some(prompt))
}

impl Database {
    /// 获取指定应用类型的所有提示词
    pub fn get_prompts(&self, app_type: &str) -> Result<IndexMap<String, Prompt>, AppError> {
        let conn = lock_conn!(self.conn);
        query_prompts(&conn, app_type)
    }

    /// 保存提示词
    pub fn save_prompt(&self, app_type: &str, prompt: &Prompt) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT OR REPLACE INTO prompts (
                id, app_type, name, content, description, enabled, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                prompt.id,
                app_type,
                prompt.name,
                prompt.content,
                prompt.description,
                prompt.enabled,
                prompt.created_at,
                prompt.updated_at,
            ],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// Persist a complete prompt-library selection atomically.
    ///
    /// Pi projects the single enabled row into AGENTS.md. A sequence of
    /// individual `save_prompt` calls can expose two enabled rows (or none) to
    /// concurrent readers, so selection changes use one SQLite transaction.
    pub(crate) fn save_prompt_selection(
        &self,
        app_type: &str,
        prompts: &IndexMap<String, Prompt>,
    ) -> Result<(), AppError> {
        validate_prompt_selection(prompts)?;
        let mut conn = lock_conn!(self.conn);
        let transaction = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        replace_prompt_rows(&transaction, app_type, prompts)?;
        transaction
            .commit()
            .map_err(|error| AppError::Database(error.to_string()))
    }

    /// Atomically publish a complete prompt library only while its full
    /// before-image still matches. This is the database half of Pi's
    /// native-file/portable-library compare-and-swap boundary.
    pub(crate) fn compare_exchange_prompt_selection(
        &self,
        app_type: &str,
        expected: &IndexMap<String, Prompt>,
        replacement: &IndexMap<String, Prompt>,
    ) -> Result<(), AppError> {
        validate_prompt_selection(replacement)?;
        self.compare_exchange_prompt_selection_unchecked(app_type, expected, replacement)
    }

    /// Restore a captured before-image only if the database still contains the
    /// exact attempted projection. The before-image may predate the current
    /// single-selection invariant, so compensation must preserve it byte for
    /// byte instead of refusing to restore legacy rows.
    pub(crate) fn restore_prompt_selection_if_attempted(
        &self,
        app_type: &str,
        attempted: &IndexMap<String, Prompt>,
        before: &IndexMap<String, Prompt>,
    ) -> Result<(), AppError> {
        self.compare_exchange_prompt_selection_unchecked(app_type, attempted, before)
    }

    fn compare_exchange_prompt_selection_unchecked(
        &self,
        app_type: &str,
        expected: &IndexMap<String, Prompt>,
        replacement: &IndexMap<String, Prompt>,
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let transaction = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        let observed = query_prompts(&transaction, app_type)?;
        if !prompt_libraries_equal(&observed, expected) {
            return Err(AppError::Conflict(format!(
                "{app_type} prompt library changed since it was read"
            )));
        }
        replace_prompt_rows(&transaction, app_type, replacement)?;
        transaction
            .commit()
            .map_err(|error| AppError::Database(error.to_string()))
    }

    /// 删除提示词
    pub fn delete_prompt(&self, app_type: &str, id: &str) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "DELETE FROM prompts WHERE id = ?1 AND app_type = ?2",
            params![id, app_type],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }
}
