//! Transactional database half of Pi catalog coordination.
//!
//! Provider row/endpoint SQL remains owned by the certified provider-write
//! primitives. This module only composes those primitives with Pi's exact-key
//! ownership ledger in one SQLite transaction.

use super::pi_projections::PiProviderProjection;
use super::provider_write::{
    insert_endpoint, insert_row, restore_provider_aggregate_on_tx, NewEndpoint,
    NewProviderAggregate, ProviderKey, ProviderRowUpdate,
};
use super::providers::delete_provider_on_tx;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::provider::{ProviderAggregate, ProviderMutationInput};
use indexmap::IndexMap;
use rusqlite::params;

impl Database {
    pub(crate) fn restore_pi_catalog_snapshot(
        &self,
        aggregates: &IndexMap<String, ProviderAggregate>,
        projections: &[PiProviderProjection],
        current_provider: Option<&str>,
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        tx.execute("DELETE FROM pi_provider_projections", [])
            .map_err(|error| AppError::Database(error.to_string()))?;
        let current_ids = {
            let mut statement = tx
                .prepare("SELECT id FROM providers WHERE app_type = 'pi'")
                .map_err(|error| AppError::Database(error.to_string()))?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| AppError::Database(error.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| AppError::Database(error.to_string()))?;
            ids
        };
        for provider_id in current_ids
            .iter()
            .filter(|provider_id| !aggregates.contains_key(provider_id.as_str()))
        {
            // Only rows created after the snapshot are removed. Updating
            // providers which existed in the snapshot preserves dependent
            // provider_health history instead of triggering ON DELETE CASCADE.
            delete_provider_on_tx(&tx, "pi", provider_id)?;
        }

        for aggregate in aggregates.values() {
            let key = ProviderKey::new("pi", aggregate.provider.id.clone())?;
            let mut input = provider_mutation_input(aggregate);
            if let Some(meta) = input.meta.as_mut() {
                meta.custom_endpoints.clear();
            }
            let row = ProviderRowUpdate::from_input(&input)?;
            let endpoints = aggregate
                .endpoints
                .values()
                .cloned()
                .map(NewEndpoint::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            restore_provider_aggregate_on_tx(
                &tx,
                &key,
                &row,
                aggregate.provider.created_at,
                aggregate.provider.sort_index,
                current_provider == Some(key.id()),
                aggregate.provider.in_failover_queue,
                &endpoints,
            )?;
        }
        for projection in projections {
            tx.execute(
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
        tx.commit()
            .map_err(|error| AppError::Database(error.to_string()))
    }

    pub(crate) fn create_pi_catalog_provider(
        &self,
        input: NewProviderAggregate,
        provider_key: &str,
    ) -> Result<PiProviderProjection, AppError> {
        if input.key.app_type() != "pi" || provider_key.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Pi catalog create requires app_type=pi and a non-empty native key".to_string(),
            ));
        }
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        insert_row(
            &tx,
            &input.key,
            &input.row.content,
            input.row.created_at,
            input.sort_index,
            false,
            input.in_failover_queue,
        )?;
        for endpoint in &input.initial_endpoints {
            insert_endpoint(&tx, &input.key, endpoint)?;
        }
        let now = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO pi_provider_projections
                (provider_id, provider_key, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)",
            params![input.key.id(), provider_key, now],
        )
        .map_err(|error| match &error {
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.extended_code,
                    rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                        | rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                ) =>
            {
                AppError::Conflict(format!(
                    "Pi native provider key '{provider_key}' is already claimed"
                ))
            }
            _ => AppError::Database(error.to_string()),
        })?;
        tx.commit()
            .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(PiProviderProjection {
            provider_id: input.key.id().to_string(),
            provider_key: provider_key.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    pub(crate) fn update_pi_catalog_provider(
        &self,
        key: &ProviderKey,
        row: &ProviderRowUpdate,
    ) -> Result<(), AppError> {
        if key.app_type() != "pi" {
            return Err(AppError::InvalidInput(
                "Pi catalog update requires app_type=pi".to_string(),
            ));
        }
        self.update_provider(key, row)
    }

    pub(crate) fn delete_pi_catalog_provider(
        &self,
        provider_id: &str,
    ) -> Result<Option<PiProviderProjection>, AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        let projection = tx
            .query_row(
                "SELECT provider_id, provider_key, created_at, updated_at
                   FROM pi_provider_projections
                  WHERE provider_id = ?1",
                [provider_id],
                |row| {
                    Ok(PiProviderProjection {
                        provider_id: row.get(0)?,
                        provider_key: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|error| AppError::Database(error.to_string()))?;
        delete_provider_on_tx(&tx, "pi", provider_id)?;
        tx.execute(
            "DELETE FROM pi_provider_projections WHERE provider_id = ?1",
            [provider_id],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        tx.commit()
            .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(projection)
    }

    pub(crate) fn restore_pi_catalog_provider(
        &self,
        aggregate: &ProviderAggregate,
        was_current: bool,
        projection: Option<&PiProviderProjection>,
    ) -> Result<(), AppError> {
        let key = ProviderKey::new("pi", aggregate.provider.id.clone())?;
        let mut input = provider_mutation_input(aggregate);
        if let Some(meta) = input.meta.as_mut() {
            meta.custom_endpoints.clear();
        }
        let row = ProviderRowUpdate::from_input(&input)?;
        let endpoints = aggregate
            .endpoints
            .values()
            .cloned()
            .map(NewEndpoint::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        restore_provider_aggregate_on_tx(
            &tx,
            &key,
            &row,
            aggregate.provider.created_at,
            aggregate.provider.sort_index,
            was_current,
            aggregate.provider.in_failover_queue,
            &endpoints,
        )?;
        tx.execute(
            "DELETE FROM pi_provider_projections WHERE provider_id = ?1",
            [key.id()],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        if let Some(projection) = projection {
            tx.execute(
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
        tx.commit()
            .map_err(|error| AppError::Database(error.to_string()))
    }
}

fn provider_mutation_input(aggregate: &ProviderAggregate) -> ProviderMutationInput {
    let provider = &aggregate.provider;
    ProviderMutationInput {
        id: provider.id.clone(),
        name: provider.name.clone(),
        settings_config: provider.settings_config.clone(),
        website_url: provider.website_url.clone(),
        category: provider.category.clone(),
        created_at: provider.created_at,
        sort_index: provider.sort_index,
        notes: provider.notes.clone(),
        meta: provider.meta.clone(),
        icon: provider.icon.clone(),
        icon_color: provider.icon_color.clone(),
        in_failover_queue: provider.in_failover_queue,
    }
}

use rusqlite::OptionalExtension;
