use crate::error::AppError;
use crate::services::provider::ProviderService;
use crate::services::PromptService;
use crate::settings;
use crate::store::AppState;
use serde_json::{json, Value};

pub(crate) fn run_post_import_sync(app_state: &AppState) -> Result<(), AppError> {
    // Provider synchronization reopens/reconciles Pi's runtime admission after
    // the pre-import boundary closed it. Run that recovery first, then execute
    // every remaining independent projection even if one of them fails.
    run_post_import_steps(
        || ProviderService::sync_current_to_live(app_state),
        || PromptService::reconcile_pi_portable_import(app_state),
        settings::reload_settings,
    )
}

fn run_post_import_steps(
    live_sync: impl FnOnce() -> Result<(), AppError>,
    prompt_sync: impl FnOnce() -> Result<(), AppError>,
    settings_reload: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let mut failures = Vec::new();
    for (stage, result) in [
        ("live", live_sync()),
        ("pi_prompt", prompt_sync()),
        ("settings", settings_reload()),
    ] {
        if let Err(error) = result {
            failures.push(format!("{stage}={error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "post-import reconciliation incomplete: {}",
            failures.join("; ")
        )))
    }
}

fn post_sync_warning<E: std::fmt::Display>(err: E) -> String {
    AppError::localized(
        "sync.post_operation_sync_failed",
        format!("后置同步状态失败: {err}"),
        format!("Post-operation synchronization failed: {err}"),
    )
    .to_string()
}

pub(crate) fn post_sync_warning_from_result(
    result: Result<Result<(), AppError>, String>,
) -> Option<String> {
    match result {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(post_sync_warning(err)),
        Err(err) => Some(post_sync_warning(err)),
    }
}

pub(crate) fn attach_warning(mut value: Value, warning: Option<String>) -> Value {
    if let Some(message) = warning {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("warning".to_string(), Value::String(message));
        }
    }
    value
}

pub(crate) fn success_payload_with_warning(backup_id: String, warning: Option<String>) -> Value {
    attach_warning(
        json!({
            "success": true,
            "message": "SQL imported successfully",
            "backupId": backup_id
        }),
        warning,
    )
}

#[cfg(test)]
mod tests {
    use super::{attach_warning, post_sync_warning_from_result, run_post_import_steps};
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn post_sync_warning_from_result_returns_none_on_success() {
        let warning = post_sync_warning_from_result(Ok(Ok(())));
        assert!(warning.is_none());
    }

    #[test]
    fn post_sync_warning_from_result_returns_some_on_sync_error() {
        let warning =
            post_sync_warning_from_result(Ok(Err(crate::error::AppError::Config("boom".into()))));
        assert!(warning.is_some());
    }

    #[tokio::test]
    async fn post_sync_warning_from_result_returns_some_on_join_error() {
        let handle = tokio::spawn(async move {
            panic!("forced join error");
        });
        let join_err = handle.await.expect_err("task should panic");
        let warning = post_sync_warning_from_result(Err(join_err.to_string()));
        assert!(warning.is_some());
    }

    #[test]
    fn attach_warning_adds_warning_without_dropping_existing_fields() {
        let payload = json!({ "status": "downloaded" });
        let updated = attach_warning(payload, Some("post sync warning".to_string()));
        assert_eq!(
            updated.get("status").and_then(|v| v.as_str()),
            Some("downloaded")
        );
        assert_eq!(
            updated.get("warning").and_then(|v| v.as_str()),
            Some("post sync warning")
        );
    }

    #[test]
    fn post_import_steps_recover_live_first_and_do_not_short_circuit() {
        let calls = RefCell::new(Vec::new());
        let error = run_post_import_steps(
            || {
                calls.borrow_mut().push("live");
                Ok(())
            },
            || {
                calls.borrow_mut().push("prompt");
                Err(crate::error::AppError::Config("invalid AGENTS.md".into()))
            },
            || {
                calls.borrow_mut().push("settings");
                Err(crate::error::AppError::Config("reload failed".into()))
            },
        )
        .expect_err("independent failures must be reported");

        assert_eq!(*calls.borrow(), ["live", "prompt", "settings"]);
        let message = error.to_string();
        assert!(message.contains("pi_prompt="));
        assert!(message.contains("settings="));
    }
}
