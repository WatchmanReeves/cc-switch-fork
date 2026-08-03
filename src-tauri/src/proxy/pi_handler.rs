//! Native Pi gateway transport.
//!
//! Pi's SDK has already serialized the request before it reaches this route.
//! The handler therefore preserves method, path, query, and body bytes; it only
//! replaces gateway/client transport headers with candidate-local material
//! and selects a wire-compatible failover target from one immutable lease.

use super::pi_runtime::{
    infer_family, PiMaterializedAttempt, PiRequestCandidate, PiRuntimeSnapshot,
};
use super::server::ProxyState;
use super::usage::{InputTokenSemantics, TokenUsage, UsageLogger};
use super::ProxyError;
use crate::database::PRICING_SOURCE_REQUEST;
use crate::pi_config::gateway::gateway_replaces_incoming_header;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use http::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE,
    TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, HeaderName, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use std::time::{Duration, Instant};

const USAGE_CAPTURE_LIMIT: usize = 4 * 1024 * 1024;
const SSE_PREFLIGHT_LIMIT: usize = 1024 * 1024;

pub(crate) async fn handle_pi_native(
    State(state): State<ProxyState>,
    Path((route_token, wildcard_path)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Result<Response, ProxyError> {
    let forwarded_path = format!("/{}", wildcard_path.trim_start_matches('/'));
    let family = infer_family(&forwarded_path).ok_or_else(|| {
        ProxyError::InvalidRequest(format!(
            "unsupported Pi native gateway path: {forwarded_path}"
        ))
    })?;
    let snapshot = state
        .pi_runtime
        .lease(state.pi_server_generation)
        .ok_or(ProxyError::NoAvailableProvider)?;
    authenticate_gateway(&snapshot, family, request.headers())?;

    let (parts, body) = request.into_parts();
    let method = parts.method;
    let uri = parts.uri;
    let path_and_query = uri.query().map_or_else(
        || forwarded_path.clone(),
        |query| format!("{forwarded_path}?{query}"),
    );
    let incoming_headers = parts.headers;
    let body = body
        .collect()
        .await
        .map_err(|error| ProxyError::InvalidRequest(format!("failed to read Pi request: {error}")))?
        .to_bytes();
    let request_json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice::<Value>(&body).map_err(|error| {
            ProxyError::InvalidRequest(format!("Pi request body is not valid JSON: {error}"))
        })?
    };
    let model_id = request_model(family, &forwarded_path, &request_json)?;
    let route = snapshot
        .route(&route_token, family, &model_id)
        .map_err(|error| ProxyError::ConfigError(error.to_string()))?;
    let admission = state
        .pi_runtime
        .admission_guard(state.pi_server_generation, &snapshot)
        .await
        .ok_or(ProxyError::NoAvailableProvider)?;
    drop(admission);

    let is_streaming = request_is_streaming(&uri, &incoming_headers, &request_json);
    // Retry policy counts actual upstream sends. Circuit-open candidates,
    // protocol-ineligible failovers, and materialization failures must not
    // consume the budget or hide a later eligible candidate.
    let mut network_budget =
        NetworkAttemptBudget::new((route.app_config.max_retries as usize).saturating_add(1));
    let attempts = route.candidates;
    let request_headers = filtered_incoming_headers(&incoming_headers);
    let started = Instant::now();
    let session_id =
        crate::proxy::extract_session_id(&incoming_headers, &request_json, "pi").session_id;
    let mut protocol_anchor = ProtocolAnchor::for_primary(attempts.first());
    let mut last_error = None;
    let mut pending_retryable: Option<PendingRetryableResponse> = None;
    record_request_start(&state).await;

    let mut index = 0;
    while index < attempts.len() && network_budget.has_remaining() {
        let provider_id = attempts[index].provider_id.clone();
        let provider_end = attempts[index..]
            .iter()
            .position(|candidate| candidate.provider_id != provider_id)
            .map_or(attempts.len(), |offset| index + offset);
        let permit = state
            .provider_router
            .allow_provider_request(&provider_id, "pi")
            .await;
        if !permit.allowed {
            last_error = Some(format!("Pi provider '{provider_id}' circuit is open"));
            index = provider_end;
            continue;
        }

        let mut provider_health_failure = None;
        for candidate in attempts[index..provider_end].iter().cloned() {
            if !network_budget.has_remaining() {
                break;
            }
            let Some(single_direct_attempt) =
                begin_protocol_materialization(&mut protocol_anchor, candidate.is_failover)
            else {
                continue;
            };
            let materialized = match materialize_candidate(candidate, path_and_query.clone()).await
            {
                Ok(candidate) => candidate,
                Err(error) => {
                    last_error = Some(error.to_string());
                    // Deferred materialization is provider-level state, not an
                    // endpoint health result. Do not execute the same command
                    // again for every endpoint; move to a compatible provider.
                    break;
                }
            };
            let protocol_identity = materialized
                .transport
                .failover_protocol_identity()
                .map(|(family, headers)| (family.as_str().to_string(), headers.clone()));
            if !single_direct_attempt
                && !protocol_identity_allows_attempt(&mut protocol_anchor, protocol_identity)
            {
                continue;
            }

            let outgoing_headers =
                merge_candidate_headers(&request_headers, &materialized.transport.headers);
            let timeout_seconds = if is_streaming {
                route.app_config.streaming_first_byte_timeout
            } else {
                route.app_config.non_streaming_timeout
            };
            if !network_budget.begin_send() {
                break;
            }
            if let Some(pending) = pending_retryable.take() {
                if pending.provider_id != provider_id {
                    settle_provider_health(
                        &state,
                        route.catalog_epoch,
                        &pending.provider_id,
                        pending.used_half_open_permit,
                        pending.provider_health.clone(),
                    )
                    .await;
                } else {
                    debug_assert_eq!(
                        pending.used_half_open_permit, permit.used_half_open_permit,
                        "one provider group must retain one circuit-breaker permit"
                    );
                }
                // A real later send has now begun, so the earlier fallback
                // response is no longer client-visible.
                drop(pending);
            }
            let send = crate::proxy::http_client::get()
                .request(method.clone(), materialized.url.clone())
                .headers(outgoing_headers)
                .body(body.clone())
                .send();
            let response = match if timeout_seconds > 0 {
                tokio::time::timeout(Duration::from_secs(u64::from(timeout_seconds)), send)
                    .await
                    .map_err(|_| ())
            } else {
                Ok(send.await)
            } {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    let error = if error.is_timeout() {
                        "Pi upstream request timed out".to_string()
                    } else {
                        "Pi upstream request failed before response".to_string()
                    };
                    provider_health_failure = Some(error.clone());
                    last_error = Some(error);
                    continue;
                }
                Err(()) => {
                    let error = "Pi upstream response-header timeout".to_string();
                    provider_health_failure = Some(error.clone());
                    last_error = Some(error);
                    continue;
                }
            };

            let status = response.status();
            let status_disposition = upstream_status_disposition(status);
            if status_disposition.is_retryable() && network_budget.has_remaining() {
                let error = format!("Pi upstream returned retryable status {status}");
                let status_health = status_disposition.provider_health();
                if status_health == ProviderHealthDisposition::Unhealthy {
                    provider_health_failure = Some(error.clone());
                }
                last_error = Some(error);
                let selected_is_failover = materialized.is_failover;
                pending_retryable = Some(PendingRetryableResponse {
                    response,
                    materialized,
                    provider_id: provider_id.clone(),
                    used_half_open_permit: permit.used_half_open_permit,
                    selected_is_failover,
                    provider_health: ProviderHealthOutcome::from_status(
                        status,
                        provider_health_failure.as_deref(),
                    ),
                });
                match status_disposition {
                    UpstreamStatusDisposition::RetryEndpoint => continue,
                    // Every endpoint in one provider group is cloned from the
                    // same credential plan. Preserve this response as a
                    // fallback, but reserve the remaining network budget for
                    // a provider that can own a different credential.
                    UpstreamStatusDisposition::RetryProvider => break,
                    UpstreamStatusDisposition::ReturnResponse => {
                        unreachable!("a non-retryable status cannot enter the retry branch")
                    }
                }
            }
            let selected_is_failover = materialized.is_failover;
            let provider_health =
                ProviderHealthOutcome::from_status(status, provider_health_failure.as_deref());
            match prepare_response(
                state.clone(),
                response,
                materialized,
                route.catalog_epoch,
                model_id.clone(),
                session_id.clone(),
                started,
                is_streaming,
                route.app_config.streaming_first_byte_timeout,
                route.app_config.streaming_idle_timeout,
                route.app_config.non_streaming_timeout,
                permit.used_half_open_permit,
                selected_is_failover,
                provider_health.clone(),
            )
            .await
            {
                Ok(prepared) => {
                    if !prepared.finalization_deferred {
                        settle_provider_health(
                            &state,
                            route.catalog_epoch,
                            &provider_id,
                            permit.used_half_open_permit,
                            provider_health,
                        )
                        .await;
                        record_request_finish(
                            &state,
                            status.is_success(),
                            selected_is_failover,
                            (!status.is_success())
                                .then(|| format!("Pi upstream returned {status}")),
                        )
                        .await;
                    }
                    return Ok(prepared.response);
                }
                Err(ProxyError::ForwardFailed(error)) | Err(ProxyError::Timeout(error)) => {
                    provider_health_failure = Some(error.clone());
                    last_error = Some(error);
                    continue;
                }
                Err(error) => {
                    release_or_record_provider(
                        &state,
                        route.catalog_epoch,
                        &provider_id,
                        permit.used_half_open_permit,
                        provider_health_failure.clone(),
                    )
                    .await;
                    record_request_finish(&state, false, false, Some(error.to_string())).await;
                    return Err(error);
                }
            }
        }

        if pending_retryable
            .as_ref()
            .is_none_or(|pending| pending.provider_id != provider_id)
        {
            release_or_record_provider(
                &state,
                route.catalog_epoch,
                &provider_id,
                permit.used_half_open_permit,
                provider_health_failure,
            )
            .await;
        }
        index = provider_end;
    }

    if let Some(pending) = pending_retryable {
        let status = pending.response.status();
        let provider_id = pending.provider_id.clone();
        let used_half_open_permit = pending.used_half_open_permit;
        let selected_is_failover = pending.selected_is_failover;
        let provider_health = pending.provider_health;
        match prepare_response(
            state.clone(),
            pending.response,
            pending.materialized,
            route.catalog_epoch,
            model_id.clone(),
            session_id.clone(),
            started,
            is_streaming,
            route.app_config.streaming_first_byte_timeout,
            route.app_config.streaming_idle_timeout,
            route.app_config.non_streaming_timeout,
            used_half_open_permit,
            selected_is_failover,
            provider_health.clone(),
        )
        .await
        {
            Ok(prepared) => {
                if !prepared.finalization_deferred {
                    settle_provider_health(
                        &state,
                        route.catalog_epoch,
                        &provider_id,
                        used_half_open_permit,
                        provider_health,
                    )
                    .await;
                    record_request_finish(
                        &state,
                        status.is_success(),
                        selected_is_failover,
                        (!status.is_success()).then(|| format!("Pi upstream returned {status}")),
                    )
                    .await;
                }
                return Ok(prepared.response);
            }
            Err(error) => {
                record_provider_result(
                    &state,
                    route.catalog_epoch,
                    &provider_id,
                    used_half_open_permit,
                    false,
                    Some(error.to_string()),
                )
                .await;
                record_request_finish(&state, false, selected_is_failover, Some(error.to_string()))
                    .await;
                return Err(error);
            }
        }
    }

    let error =
        last_error.unwrap_or_else(|| "no wire-compatible Pi candidate was available".to_string());
    record_request_finish(&state, false, false, Some(error.clone())).await;
    Err(ProxyError::ForwardFailed(error))
}

struct PendingRetryableResponse {
    response: reqwest::Response,
    materialized: PiMaterializedAttempt,
    provider_id: String,
    used_half_open_permit: bool,
    selected_is_failover: bool,
    provider_health: ProviderHealthOutcome,
}

#[derive(Debug)]
struct NetworkAttemptBudget {
    remaining: usize,
}

impl NetworkAttemptBudget {
    fn new(max_attempts: usize) -> Self {
        Self {
            remaining: max_attempts,
        }
    }

    fn has_remaining(&self) -> bool {
        self.remaining > 0
    }

    /// Consume budget only immediately before an actual upstream send.
    fn begin_send(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

/// Return whether this attempt consumes the one direct-only grant, or `None`
/// when the candidate must not even be materialized.
fn begin_protocol_materialization(anchor: &mut ProtocolAnchor, is_failover: bool) -> Option<bool> {
    match anchor {
        ProtocolAnchor::DirectOnlyPending if !is_failover => {
            // Consume before materialization so an error in a later
            // credential/custom header cannot run the protocol command again.
            *anchor = ProtocolAnchor::Ineligible;
            Some(true)
        }
        ProtocolAnchor::DirectOnlyPending | ProtocolAnchor::Ineligible => None,
        ProtocolAnchor::Unset | ProtocolAnchor::Predictable(_) => Some(false),
    }
}

#[derive(Debug)]
enum ProtocolAnchor {
    Unset,
    Predictable((String, HeaderMap)),
    DirectOnlyPending,
    Ineligible,
}

impl ProtocolAnchor {
    fn for_primary(primary: Option<&PiRequestCandidate>) -> Self {
        let Some(primary) = primary else {
            return Self::Unset;
        };
        if !primary.protocol_identity_is_predictable() {
            return Self::DirectOnlyPending;
        }
        match primary.planned_protocol_identity() {
            Ok(Some(identity)) => Self::Predictable(identity),
            Ok(None) => Self::DirectOnlyPending,
            // If the primary's protocol identity cannot be established, a
            // backup must not self-declare compatibility. Give the primary
            // exactly one direct materialization; failure remains fail-closed.
            Err(_) => Self::DirectOnlyPending,
        }
    }
}

fn protocol_identity_allows_attempt(
    anchor: &mut ProtocolAnchor,
    candidate: Option<(String, HeaderMap)>,
) -> bool {
    match (&*anchor, candidate) {
        (ProtocolAnchor::Unset, Some(candidate)) => {
            *anchor = ProtocolAnchor::Predictable(candidate);
            true
        }
        (ProtocolAnchor::Unset, None) => false,
        (ProtocolAnchor::Predictable(primary), Some(candidate)) => primary == &candidate,
        (ProtocolAnchor::Predictable(_), None)
        | (ProtocolAnchor::DirectOnlyPending, _)
        | (ProtocolAnchor::Ineligible, _) => false,
    }
}

async fn materialize_candidate(
    candidate: PiRequestCandidate,
    path_and_query: String,
) -> Result<PiMaterializedAttempt, ProxyError> {
    tokio::task::spawn_blocking(move || candidate.materialize(&path_and_query))
        .await
        .map_err(|error| {
            ProxyError::Internal(format!("Pi candidate materialization task failed: {error}"))
        })?
        .map_err(|error| ProxyError::ConfigError(error.to_string()))
}

async fn record_provider_result(
    state: &ProxyState,
    catalog_epoch: u64,
    provider_id: &str,
    used_half_open_permit: bool,
    success: bool,
    error: Option<String>,
) {
    let Some(_guard) = state.pi_runtime.writeback_guard(catalog_epoch).await else {
        state
            .provider_router
            .release_permit_neutral(provider_id, "pi", used_half_open_permit)
            .await;
        return;
    };
    if let Err(record_error) = state
        .provider_router
        .record_result(provider_id, "pi", used_half_open_permit, success, error)
        .await
    {
        log::warn!("failed to update Pi provider health: {record_error}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderHealthDisposition {
    Healthy,
    Unhealthy,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamStatusDisposition {
    ReturnResponse,
    RetryEndpoint,
    RetryProvider,
}

impl UpstreamStatusDisposition {
    const fn is_retryable(self) -> bool {
        !matches!(self, Self::ReturnResponse)
    }

    const fn provider_health(self) -> ProviderHealthDisposition {
        match self {
            Self::ReturnResponse => ProviderHealthDisposition::Healthy,
            Self::RetryEndpoint => ProviderHealthDisposition::Unhealthy,
            Self::RetryProvider => ProviderHealthDisposition::Neutral,
        }
    }
}

fn upstream_status_disposition(status: StatusCode) -> UpstreamStatusDisposition {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        // Pi's provider owns authentication (API key or OAuth), while its
        // custom endpoints only replace the URL. Authentication rejection is
        // therefore neutral for endpoint health and can only benefit from a
        // distinct provider credential.
        return UpstreamStatusDisposition::RetryProvider;
    }
    if (!status.is_client_error() && !status.is_server_error())
        || matches!(
            status,
            StatusCode::BAD_REQUEST
                | StatusCode::METHOD_NOT_ALLOWED
                | StatusCode::NOT_ACCEPTABLE
                | StatusCode::PAYLOAD_TOO_LARGE
                | StatusCode::URI_TOO_LONG
                | StatusCode::UNSUPPORTED_MEDIA_TYPE
                | StatusCode::UNPROCESSABLE_ENTITY
                | StatusCode::NOT_IMPLEMENTED
        )
    {
        UpstreamStatusDisposition::ReturnResponse
    } else {
        UpstreamStatusDisposition::RetryEndpoint
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderHealthOutcome {
    disposition: ProviderHealthDisposition,
    error: Option<String>,
}

impl ProviderHealthOutcome {
    fn from_status(status: StatusCode, prior_failure: Option<&str>) -> Self {
        match upstream_status_disposition(status).provider_health() {
            ProviderHealthDisposition::Healthy => Self {
                disposition: ProviderHealthDisposition::Healthy,
                error: None,
            },
            ProviderHealthDisposition::Unhealthy => Self {
                disposition: ProviderHealthDisposition::Unhealthy,
                error: Some(format!("Pi upstream returned {status}")),
            },
            ProviderHealthDisposition::Neutral => prior_failure.map_or_else(
                || Self {
                    disposition: ProviderHealthDisposition::Neutral,
                    error: None,
                },
                |error| Self {
                    // A credential rejection is neutral by itself, but it
                    // cannot erase a real failure from an earlier endpoint
                    // covered by the same provider-level permit.
                    disposition: ProviderHealthDisposition::Unhealthy,
                    error: Some(error.to_string()),
                },
            ),
        }
    }
}

async fn settle_provider_health(
    state: &ProxyState,
    catalog_epoch: u64,
    provider_id: &str,
    used_half_open_permit: bool,
    outcome: ProviderHealthOutcome,
) {
    match outcome.disposition {
        ProviderHealthDisposition::Healthy => {
            record_provider_result(
                state,
                catalog_epoch,
                provider_id,
                used_half_open_permit,
                true,
                None,
            )
            .await;
        }
        ProviderHealthDisposition::Unhealthy => {
            record_provider_result(
                state,
                catalog_epoch,
                provider_id,
                used_half_open_permit,
                false,
                outcome.error,
            )
            .await;
        }
        ProviderHealthDisposition::Neutral => {
            state
                .provider_router
                .release_permit_neutral(provider_id, "pi", used_half_open_permit)
                .await;
        }
    }
}

async fn release_or_record_provider(
    state: &ProxyState,
    catalog_epoch: u64,
    provider_id: &str,
    used_half_open_permit: bool,
    provider_health_failure: Option<String>,
) {
    if provider_health_failure.is_some() {
        record_provider_result(
            state,
            catalog_epoch,
            provider_id,
            used_half_open_permit,
            false,
            provider_health_failure,
        )
        .await;
    } else {
        state
            .provider_router
            .release_permit_neutral(provider_id, "pi", used_half_open_permit)
            .await;
    }
}

async fn record_request_start(state: &ProxyState) {
    let mut status = state.status.write().await;
    status.total_requests = status.total_requests.saturating_add(1);
    status.last_request_at = Some(chrono::Utc::now().to_rfc3339());
}

async fn record_request_finish(
    state: &ProxyState,
    success: bool,
    used_failover: bool,
    error: Option<String>,
) {
    let mut status = state.status.write().await;
    if success {
        status.success_requests = status.success_requests.saturating_add(1);
        status.last_error = None;
    } else {
        status.failed_requests = status.failed_requests.saturating_add(1);
        status.last_error = error;
    }
    if used_failover {
        status.failover_count = status.failover_count.saturating_add(1);
    }
    if status.total_requests > 0 {
        status.success_rate =
            (status.success_requests as f32 / status.total_requests as f32) * 100.0;
    }
}

fn authenticate_gateway(
    snapshot: &PiRuntimeSnapshot,
    family: crate::pi_config::gateway::PiGatewayApiFamily,
    headers: &HeaderMap,
) -> Result<(), ProxyError> {
    let value = match family {
        crate::pi_config::gateway::PiGatewayApiFamily::AnthropicMessages => headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        crate::pi_config::gateway::PiGatewayApiFamily::GoogleGenerativeAi => headers
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok()),
        crate::pi_config::gateway::PiGatewayApiFamily::OpenAiCompletions
        | crate::pi_config::gateway::PiGatewayApiFamily::OpenAiResponses => headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer ")),
    }
    .ok_or_else(|| ProxyError::AuthError("missing Pi gateway credential".to_string()))?;
    if !snapshot.token_matches(value) {
        return Err(ProxyError::AuthError(
            "invalid Pi gateway credential".to_string(),
        ));
    }
    Ok(())
}

fn request_model(
    family: crate::pi_config::gateway::PiGatewayApiFamily,
    path: &str,
    body: &Value,
) -> Result<String, ProxyError> {
    if family == crate::pi_config::gateway::PiGatewayApiFamily::GoogleGenerativeAi {
        let encoded = path
            .strip_prefix("/models/")
            .and_then(|rest| rest.split(':').next())
            .filter(|model| !model.is_empty())
            .ok_or_else(|| {
                ProxyError::InvalidRequest("Pi Google request has no model in its path".to_string())
            })?;
        return percent_decode(encoded).ok_or_else(|| {
            ProxyError::InvalidRequest("Pi Google model path has invalid escaping".to_string())
        });
    }
    body.get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ProxyError::InvalidRequest("Pi request body has no model identifier".to_string())
        })
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = *bytes.get(index + 1)?;
        let low = *bytes.get(index + 2)?;
        decoded.push(hex(high)? << 4 | hex(low)?);
        index += 3;
    }
    String::from_utf8(decoded).ok()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn request_is_streaming(uri: &http::Uri, headers: &HeaderMap, body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
        || uri
            .query()
            .is_some_and(|query| query.split('&').any(|part| part == "alt=sse"))
        || headers
            .get(http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"))
}

fn filtered_incoming_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_named = connection_named_headers(headers);
    let mut filtered = HeaderMap::new();
    for (name, value) in headers {
        if matches!(
            *name,
            HOST | CONTENT_LENGTH
                | CONNECTION
                | TRANSFER_ENCODING
                | TE
                | TRAILER
                | UPGRADE
                | AUTHORIZATION
                | PROXY_AUTHENTICATE
                | PROXY_AUTHORIZATION
        ) || gateway_replaces_incoming_header(name)
            || connection_named.contains(name)
        {
            continue;
        }
        filtered.append(name.clone(), value.clone());
    }
    filtered
}

fn merge_candidate_headers(incoming: &HeaderMap, candidate: &HeaderMap) -> HeaderMap {
    let mut merged = incoming.clone();
    for (name, value) in candidate {
        merged.insert(name.clone(), value.clone());
    }
    merged.remove(CONTENT_LENGTH);
    merged.remove(TRANSFER_ENCODING);
    merged
}

struct PreparedPiResponse {
    response: Response,
    finalization_deferred: bool,
}

struct PiStreamFinalization {
    state: ProxyState,
    candidate: PiMaterializedAttempt,
    catalog_epoch: u64,
    request_model: String,
    session_id: String,
    started: Instant,
    is_streaming: bool,
    status: StatusCode,
    content_is_sse: bool,
    used_half_open_permit: bool,
    selected_is_failover: bool,
    complete_provider_health: ProviderHealthOutcome,
}

enum PiStreamTermination {
    Complete { captured: Option<Vec<u8>> },
    UpstreamFailure { message: String },
    DownstreamDropped,
}

struct PiStreamDisposition {
    provider_health: ProviderHealthDisposition,
    provider_error: Option<String>,
    request_success: bool,
    request_error: Option<String>,
}

fn pi_stream_disposition(
    status: StatusCode,
    complete_provider_health: &ProviderHealthOutcome,
    termination: &PiStreamTermination,
) -> PiStreamDisposition {
    match termination {
        PiStreamTermination::Complete { .. } => PiStreamDisposition {
            provider_health: complete_provider_health.disposition,
            provider_error: complete_provider_health.error.clone(),
            request_success: status.is_success(),
            request_error: (!status.is_success()).then(|| format!("Pi upstream returned {status}")),
        },
        PiStreamTermination::UpstreamFailure { message } => PiStreamDisposition {
            provider_health: ProviderHealthDisposition::Unhealthy,
            provider_error: Some(message.clone()),
            request_success: false,
            request_error: Some(message.clone()),
        },
        PiStreamTermination::DownstreamDropped => PiStreamDisposition {
            // A downstream cancellation says nothing about upstream health.
            provider_health: ProviderHealthDisposition::Neutral,
            provider_error: None,
            request_success: false,
            request_error: Some(
                "Pi downstream client closed before the upstream stream completed".to_string(),
            ),
        },
    }
}

struct PiStreamFinalizer {
    pending: Option<PiStreamFinalization>,
}

impl PiStreamFinalizer {
    fn new(finalization: PiStreamFinalization) -> Self {
        Self {
            pending: Some(finalization),
        }
    }

    fn finish(mut self, termination: PiStreamTermination) {
        if let Some(finalization) = self.pending.take() {
            finalization.spawn(termination);
        }
    }
}

impl Drop for PiStreamFinalizer {
    fn drop(&mut self) {
        if let Some(finalization) = self.pending.take() {
            finalization.spawn(PiStreamTermination::DownstreamDropped);
        }
    }
}

impl PiStreamFinalization {
    fn spawn(self, termination: PiStreamTermination) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            log::error!("Pi stream finalization lost because no Tokio runtime is available");
            return;
        };
        runtime.spawn(async move {
            self.apply(termination).await;
        });
    }

    async fn apply(self, termination: PiStreamTermination) {
        let disposition =
            pi_stream_disposition(self.status, &self.complete_provider_health, &termination);
        settle_provider_health(
            &self.state,
            self.catalog_epoch,
            &self.candidate.provider_id,
            self.used_half_open_permit,
            ProviderHealthOutcome {
                disposition: disposition.provider_health,
                error: disposition.provider_error,
            },
        )
        .await;
        record_request_finish(
            &self.state,
            disposition.request_success,
            self.selected_is_failover,
            disposition.request_error.clone(),
        )
        .await;

        match termination {
            PiStreamTermination::Complete { captured } => {
                if let Some(_guard) = self
                    .state
                    .pi_runtime
                    .writeback_guard(self.catalog_epoch)
                    .await
                {
                    log_pi_usage(
                        &self.state,
                        &self.candidate,
                        &self.request_model,
                        &self.session_id,
                        self.started,
                        self.is_streaming,
                        self.status,
                        self.content_is_sse,
                        captured.as_deref(),
                    )
                    .await;
                }
            }
            PiStreamTermination::UpstreamFailure { message } => {
                if let Some(_guard) = self
                    .state
                    .pi_runtime
                    .writeback_guard(self.catalog_epoch)
                    .await
                {
                    log_pi_stream_error(&self, StatusCode::BAD_GATEWAY.as_u16(), &message);
                }
            }
            PiStreamTermination::DownstreamDropped => {
                if let Some(_guard) = self
                    .state
                    .pi_runtime
                    .writeback_guard(self.catalog_epoch)
                    .await
                {
                    if let Some(message) = disposition.request_error.as_deref() {
                        log_pi_stream_error(&self, 499, message);
                    }
                }
            }
        }
    }
}

fn log_pi_stream_error(finalization: &PiStreamFinalization, status_code: u16, message: &str) {
    let logging_enabled = finalization
        .state
        .config
        .try_read()
        .map(|config| config.enable_logging)
        .unwrap_or(true);
    if !logging_enabled {
        return;
    }
    let logger = UsageLogger::new(&finalization.state.db);
    if let Err(error) = logger.log_error_with_context(
        uuid::Uuid::new_v4().to_string(),
        finalization.candidate.provider_id.clone(),
        "pi".to_string(),
        finalization.request_model.clone(),
        status_code,
        message.to_string(),
        finalization.started.elapsed().as_millis() as u64,
        finalization.is_streaming,
        (!finalization.session_id.is_empty()).then(|| finalization.session_id.clone()),
        Some(finalization.candidate.transport.family_name().to_string()),
        InputTokenSemantics::for_pi_family(finalization.candidate.transport.family()),
    ) {
        log::warn!("failed to record Pi gateway stream error: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_response(
    state: ProxyState,
    response: reqwest::Response,
    candidate: PiMaterializedAttempt,
    catalog_epoch: u64,
    request_model: String,
    session_id: String,
    started: Instant,
    is_streaming: bool,
    first_semantic_timeout_seconds: u32,
    streaming_idle_timeout_seconds: u32,
    non_streaming_timeout_seconds: u32,
    used_half_open_permit: bool,
    selected_is_failover: bool,
    complete_provider_health: ProviderHealthOutcome,
) -> Result<PreparedPiResponse, ProxyError> {
    let status = response.status();
    let headers = filtered_response_headers(response.headers());
    let content_is_sse = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !is_streaming && !content_is_sse {
        let read = response.bytes();
        let bytes = if non_streaming_timeout_seconds > 0 {
            tokio::time::timeout(
                Duration::from_secs(u64::from(non_streaming_timeout_seconds)),
                read,
            )
            .await
            .map_err(|_| {
                ProxyError::Timeout(
                    "Pi non-streaming response body exceeded its timeout".to_string(),
                )
            })?
            .map_err(|error| {
                ProxyError::ForwardFailed(format!("Pi upstream response body failed: {error}"))
            })?
        } else {
            read.await.map_err(|error| {
                ProxyError::ForwardFailed(format!("Pi upstream response body failed: {error}"))
            })?
        };
        if let Some(_guard) = state.pi_runtime.writeback_guard(catalog_epoch).await {
            state.current_providers.write().await.insert(
                "pi".to_string(),
                (
                    candidate.provider_id.clone(),
                    candidate.provider_name.clone(),
                ),
            );
            log_pi_usage(
                &state,
                &candidate,
                &request_model,
                &session_id,
                started,
                false,
                status,
                false,
                Some(&bytes),
            )
            .await;
        }
        let mut builder = Response::builder().status(status);
        *builder.headers_mut().ok_or_else(|| {
            ProxyError::Internal("failed to build Pi response headers".to_string())
        })? = headers;
        let response = builder.body(Body::from(bytes)).map_err(|error| {
            ProxyError::Internal(format!("failed to build Pi response: {error}"))
        })?;
        return Ok(PreparedPiResponse {
            response,
            finalization_deferred: false,
        });
    }

    let mut stream = response.bytes_stream().boxed();
    let prefix = if content_is_sse || (is_streaming && status.is_success()) {
        preflight_sse(
            &mut stream,
            first_semantic_timeout_seconds,
            SSE_PREFLIGHT_LIMIT,
        )
        .await?
    } else {
        Vec::new()
    };
    if let Some(_guard) = state.pi_runtime.writeback_guard(catalog_epoch).await {
        state.current_providers.write().await.insert(
            "pi".to_string(),
            (
                candidate.provider_id.clone(),
                candidate.provider_name.clone(),
            ),
        );
    }
    let body_stream = logged_body_stream(
        state,
        stream,
        prefix,
        candidate,
        catalog_epoch,
        request_model,
        session_id,
        started,
        is_streaming || content_is_sse,
        status,
        content_is_sse,
        streaming_idle_timeout_seconds,
        used_half_open_permit,
        selected_is_failover,
        complete_provider_health,
    );
    let mut builder = Response::builder().status(status);
    *builder
        .headers_mut()
        .ok_or_else(|| ProxyError::Internal("failed to build Pi response headers".to_string()))? =
        headers;
    let response = builder
        .body(Body::from_stream(body_stream))
        .map_err(|error| ProxyError::Internal(format!("failed to build Pi response: {error}")))?;
    Ok(PreparedPiResponse {
        response,
        finalization_deferred: true,
    })
}

fn filtered_response_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_named = connection_named_headers(headers);
    let mut filtered = HeaderMap::new();
    for (name, value) in headers {
        if matches!(
            *name,
            CONNECTION
                | CONTENT_LENGTH
                | TRANSFER_ENCODING
                | TE
                | TRAILER
                | UPGRADE
                | PROXY_AUTHENTICATE
                | PROXY_AUTHORIZATION
        ) || connection_named.contains(name)
        {
            continue;
        }
        filtered.append(name.clone(), value.clone());
    }
    filtered
}

fn connection_named_headers(headers: &HeaderMap) -> std::collections::HashSet<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect()
}

async fn preflight_sse(
    stream: &mut BoxStream<'static, Result<Bytes, reqwest::Error>>,
    timeout_seconds: u32,
    byte_limit: usize,
) -> Result<Vec<Bytes>, ProxyError> {
    let deadline = (timeout_seconds > 0)
        .then(|| tokio::time::Instant::now() + Duration::from_secs(u64::from(timeout_seconds)));
    let mut chunks = Vec::new();
    let mut buffer = Vec::new();
    loop {
        let next = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, stream.next())
                .await
                .map_err(|_| {
                    ProxyError::Timeout(
                        "Pi SSE produced no semantic event before the first-event timeout"
                            .to_string(),
                    )
                })?,
            None => stream.next().await,
        };
        let chunk = next
            .ok_or_else(|| {
                ProxyError::ForwardFailed(
                    "Pi SSE ended before its first semantic event".to_string(),
                )
            })?
            .map_err(|_| {
                ProxyError::ForwardFailed(
                    "Pi SSE failed before its first semantic event".to_string(),
                )
            })?;
        if buffer.len().saturating_add(chunk.len()) > byte_limit {
            return Err(ProxyError::ForwardFailed(
                "Pi SSE prelude exceeded the bounded commit fence".to_string(),
            ));
        }
        buffer.extend_from_slice(&chunk);
        chunks.push(chunk);
        if contains_semantic_sse_event(&buffer) {
            return Ok(chunks);
        }
    }
}

fn contains_semantic_sse_event(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    text.split("\n\n").any(|block| {
        block.lines().any(|line| {
            line.strip_prefix("data:")
                .is_some_and(|data| !data.trim().is_empty())
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn logged_body_stream(
    state: ProxyState,
    mut stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    prefix: Vec<Bytes>,
    candidate: PiMaterializedAttempt,
    catalog_epoch: u64,
    request_model: String,
    session_id: String,
    started: Instant,
    is_streaming: bool,
    status: StatusCode,
    content_is_sse: bool,
    streaming_idle_timeout_seconds: u32,
    used_half_open_permit: bool,
    selected_is_failover: bool,
    complete_provider_health: ProviderHealthOutcome,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    // Construct the guard before the generator is polled. Axum may drop a
    // response body without ever polling it when the client disconnects after
    // headers, and the HalfOpen permit must still be released in that case.
    let finalizer = PiStreamFinalizer::new(PiStreamFinalization {
        state,
        candidate,
        catalog_epoch,
        request_model,
        session_id,
        started,
        is_streaming,
        status,
        content_is_sse,
        used_half_open_permit,
        selected_is_failover,
        complete_provider_health,
    });
    async_stream::stream! {
        let mut captured = Vec::new();
        let mut capture_open = true;
        for chunk in prefix {
            capture_usage_bytes(&mut captured, &mut capture_open, &chunk);
            yield Ok(chunk);
        }
        loop {
            let next = if streaming_idle_timeout_seconds > 0 {
                match tokio::time::timeout(
                    Duration::from_secs(u64::from(streaming_idle_timeout_seconds)),
                    stream.next(),
                )
                .await
                {
                    Ok(next) => next,
                    Err(_) => {
                        let message = "Pi upstream stream exceeded its idle timeout".to_string();
                        finalizer.finish(PiStreamTermination::UpstreamFailure {
                            message: message.clone(),
                        });
                        yield Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            message,
                        ));
                        return;
                    }
                }
            } else {
                stream.next().await
            };
            let Some(result) = next else {
                break;
            };
            match result {
                Ok(chunk) => {
                    capture_usage_bytes(&mut captured, &mut capture_open, &chunk);
                    yield Ok(chunk);
                }
                Err(error) => {
                    let message = format!("Pi upstream response failed: {error}");
                    finalizer.finish(PiStreamTermination::UpstreamFailure {
                        message: message.clone(),
                    });
                    yield Err(std::io::Error::other(message));
                    return;
                }
            }
        }
        finalizer.finish(PiStreamTermination::Complete {
            captured: capture_open.then_some(captured),
        });
    }
}

fn capture_usage_bytes(captured: &mut Vec<u8>, capture_open: &mut bool, chunk: &[u8]) {
    if !*capture_open {
        return;
    }
    if captured.len().saturating_add(chunk.len()) > USAGE_CAPTURE_LIMIT {
        captured.clear();
        *capture_open = false;
        return;
    }
    captured.extend_from_slice(chunk);
}

#[allow(clippy::too_many_arguments)]
async fn log_pi_usage(
    state: &ProxyState,
    candidate: &PiMaterializedAttempt,
    request_model: &str,
    session_id: &str,
    started: Instant,
    is_streaming: bool,
    status: StatusCode,
    content_is_sse: bool,
    captured: Option<&[u8]>,
) {
    let logging_enabled = state
        .config
        .try_read()
        .map(|config| config.enable_logging)
        .unwrap_or(true);
    if !logging_enabled {
        return;
    }
    let usage = captured
        .and_then(|bytes| parse_usage(candidate.transport.family_name(), bytes, content_is_sse))
        .unwrap_or_default();
    let response_model = usage
        .model
        .clone()
        .unwrap_or_else(|| request_model.to_string());
    let logger = UsageLogger::new(&state.db);
    let input_token_semantics = InputTokenSemantics::for_pi_family(candidate.transport.family());
    if !status.is_success() {
        let _ = logger.log_error(
            uuid::Uuid::new_v4().to_string(),
            candidate.provider_id.clone(),
            "pi".to_string(),
            response_model,
            status.as_u16(),
            format!("Pi upstream returned {status}"),
            started.elapsed().as_millis() as u64,
            input_token_semantics,
        );
        return;
    }
    let (multiplier, pricing_source) = logger
        .resolve_pricing_config(&candidate.provider_id, "pi")
        .await;
    let pricing_model = if pricing_source == PRICING_SOURCE_REQUEST {
        request_model.to_string()
    } else {
        response_model.clone()
    };
    let request_id = usage.dedup_request_id(Some(("pi", candidate.provider_id.as_str())));
    if let Err(error) = logger.log_with_calculation(
        request_id,
        candidate.provider_id.clone(),
        "pi".to_string(),
        response_model,
        request_model.to_string(),
        pricing_model,
        input_token_semantics,
        usage,
        multiplier,
        started.elapsed().as_millis() as u64,
        None,
        status.as_u16(),
        (!session_id.is_empty()).then(|| session_id.to_string()),
        Some(candidate.transport.family_name().to_string()),
        is_streaming,
    ) {
        log::warn!("failed to record Pi gateway usage: {error}");
    }
}

fn parse_usage(family: &str, bytes: &[u8], is_sse: bool) -> Option<TokenUsage> {
    if is_sse {
        let events = sse_json_events(bytes);
        return match family {
            "anthropic-messages" => TokenUsage::from_claude_stream_events(&events),
            "openai-completions" => TokenUsage::from_openai_stream_events(&events),
            "openai-responses" => TokenUsage::from_codex_stream_events_auto(&events),
            "google-generative-ai" => TokenUsage::from_gemini_stream_chunks(&events),
            _ => None,
        };
    }
    let body = serde_json::from_slice::<Value>(bytes).ok()?;
    match family {
        "anthropic-messages" => TokenUsage::from_claude_response(&body),
        "openai-completions" => TokenUsage::from_openai_response(&body),
        "openai-responses" => TokenUsage::from_codex_response_auto(&body),
        "google-generative-ai" => TokenUsage::from_gemini_response(&body),
        _ => None,
    }
}

fn sse_json_events(bytes: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    text.split("\n\n")
        .flat_map(str::lines)
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| !data.is_empty() && *data != "[DONE]")
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_matches_pi_contract_matrix() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            assert_eq!(
                upstream_status_disposition(status),
                UpstreamStatusDisposition::RetryProvider,
                "{status}"
            );
        }
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::CONFLICT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::IM_A_TEAPOT,
            StatusCode::BAD_GATEWAY,
        ] {
            assert_eq!(
                upstream_status_disposition(status),
                UpstreamStatusDisposition::RetryEndpoint,
                "{status}"
            );
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::NOT_ACCEPTABLE,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::URI_TOO_LONG,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::NOT_IMPLEMENTED,
        ] {
            assert_eq!(
                upstream_status_disposition(status),
                UpstreamStatusDisposition::ReturnResponse,
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn credential_rejections_remain_retryable_but_health_neutral() {
        let app = axum::Router::new()
            .route(
                "/unauthorized",
                axum::routing::get(|| async { StatusCode::UNAUTHORIZED }),
            )
            .route(
                "/forbidden",
                axum::routing::get(|| async { StatusCode::FORBIDDEN }),
            );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local Pi capture endpoint");
        let address = listener.local_addr().expect("local capture address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve local Pi capture endpoint");
        });

        for (path, expected) in [
            ("unauthorized", StatusCode::UNAUTHORIZED),
            ("forbidden", StatusCode::FORBIDDEN),
        ] {
            let response = reqwest::get(format!("http://{address}/{path}"))
                .await
                .expect("request local Pi capture endpoint");
            assert_eq!(response.status(), expected);
            assert_eq!(
                upstream_status_disposition(response.status()),
                UpstreamStatusDisposition::RetryProvider
            );
            assert_eq!(
                ProviderHealthOutcome::from_status(response.status(), None),
                ProviderHealthOutcome {
                    disposition: ProviderHealthDisposition::Neutral,
                    error: None,
                }
            );
        }
        assert_eq!(
            ProviderHealthOutcome::from_status(
                StatusCode::UNAUTHORIZED,
                Some("earlier endpoint failed"),
            ),
            ProviderHealthOutcome {
                disposition: ProviderHealthDisposition::Unhealthy,
                error: Some("earlier endpoint failed".to_string()),
            }
        );
        server.abort();
    }

    #[test]
    fn sse_comments_do_not_cross_the_commit_fence() {
        assert!(!contains_semantic_sse_event(
            b": keep-alive\n\n: another\n\n"
        ));
        assert!(contains_semantic_sse_event(
            b": keep-alive\n\nevent: message_start\ndata: {\"type\":\"message_start\"}\n\n"
        ));
    }

    #[test]
    fn gateway_header_filter_removes_client_auth_and_protocol_identity() {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            AUTHORIZATION,
            http::HeaderValue::from_static("Bearer gateway"),
        );
        incoming.insert("x-api-key", http::HeaderValue::from_static("gateway"));
        incoming.insert(
            "anthropic-version",
            http::HeaderValue::from_static("client-version"),
        );
        incoming.insert("x-request-local", http::HeaderValue::from_static("kept"));
        let filtered = filtered_incoming_headers(&incoming);
        assert!(filtered.get(AUTHORIZATION).is_none());
        assert!(filtered.get("x-api-key").is_none());
        assert!(filtered.get("anthropic-version").is_none());
        assert_eq!(filtered["x-request-local"], "kept");
    }

    #[test]
    fn percent_decoding_is_strict_utf8() {
        assert_eq!(
            percent_decode("gemini%2D2.5").as_deref(),
            Some("gemini-2.5")
        );
        assert!(percent_decode("%ZZ").is_none());
        assert!(percent_decode("%ff").is_none());
    }

    #[test]
    fn unavailable_primary_identity_is_direct_only_and_cannot_self_anchor_from_failover() {
        let mut anchor = ProtocolAnchor::DirectOnlyPending;
        assert_eq!(
            begin_protocol_materialization(&mut anchor, false),
            Some(true)
        );
        assert_eq!(begin_protocol_materialization(&mut anchor, true), None);
        assert!(matches!(anchor, ProtocolAnchor::Ineligible));
    }

    #[test]
    fn skipped_candidates_do_not_reduce_the_network_retry_budget() {
        let mut budget = NetworkAttemptBudget::new(2);

        // Circuit, protocol, and materialization skips never call begin_send.
        for _ in 0..4 {
            assert!(budget.has_remaining());
        }
        assert!(budget.begin_send());
        assert!(budget.has_remaining());
        assert!(budget.begin_send());
        assert!(!budget.has_remaining());
        assert!(!budget.begin_send());
    }

    #[test]
    fn protocol_anchor_blocks_replay_when_identity_is_unpredictable_or_changes() {
        let mut unpredictable = ProtocolAnchor::DirectOnlyPending;
        assert_eq!(
            begin_protocol_materialization(&mut unpredictable, false),
            Some(true)
        );
        assert_eq!(
            begin_protocol_materialization(&mut unpredictable, false),
            None
        );
        assert_eq!(
            begin_protocol_materialization(&mut unpredictable, true),
            None
        );

        let identity = Some(("openai-responses".to_string(), HeaderMap::new()));
        let mut predictable = ProtocolAnchor::Unset;
        assert!(protocol_identity_allows_attempt(
            &mut predictable,
            identity.clone()
        ));
        assert!(protocol_identity_allows_attempt(&mut predictable, identity));
        let mut changed_headers = HeaderMap::new();
        changed_headers.insert("openai-version", http::HeaderValue::from_static("changed"));
        assert!(!protocol_identity_allows_attempt(
            &mut predictable,
            Some(("openai-responses".to_string(), changed_headers))
        ));
    }

    #[test]
    fn gateway_header_filters_share_protected_and_dynamic_hop_by_hop_rules() {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            CONNECTION,
            http::HeaderValue::from_static("x-private-hop, x-another-hop"),
        );
        incoming.insert(
            "x-private-hop",
            http::HeaderValue::from_static("must-not-forward"),
        );
        incoming.insert(
            "x-another-hop",
            http::HeaderValue::from_static("must-not-forward"),
        );
        incoming.insert(
            "cf-connecting-ip",
            http::HeaderValue::from_static("203.0.113.5"),
        );
        incoming.insert("traceparent", http::HeaderValue::from_static("00-spoofed"));
        incoming.insert(
            "x-candidate-local",
            http::HeaderValue::from_static("preserved"),
        );

        let request = filtered_incoming_headers(&incoming);
        assert!(request.get("x-private-hop").is_none());
        assert!(request.get("x-another-hop").is_none());
        assert!(request.get("cf-connecting-ip").is_none());
        assert!(request.get("traceparent").is_none());
        assert_eq!(request["x-candidate-local"], "preserved");

        let response = filtered_response_headers(&incoming);
        assert!(response.get("x-private-hop").is_none());
        assert!(response.get("x-another-hop").is_none());
        assert_eq!(response["x-candidate-local"], "preserved");
    }

    #[test]
    fn streaming_health_waits_for_the_terminal_outcome() {
        let complete = pi_stream_disposition(
            StatusCode::OK,
            &ProviderHealthOutcome::from_status(StatusCode::OK, None),
            &PiStreamTermination::Complete {
                captured: Some(Vec::new()),
            },
        );
        assert_eq!(complete.provider_health, ProviderHealthDisposition::Healthy);
        assert!(complete.request_success);

        let truncated = pi_stream_disposition(
            StatusCode::OK,
            &ProviderHealthOutcome::from_status(StatusCode::OK, None),
            &PiStreamTermination::UpstreamFailure {
                message: "truncated".to_string(),
            },
        );
        assert_eq!(
            truncated.provider_health,
            ProviderHealthDisposition::Unhealthy
        );
        assert!(!truncated.request_success);
        assert_eq!(truncated.provider_error.as_deref(), Some("truncated"));

        let client_drop = pi_stream_disposition(
            StatusCode::OK,
            &ProviderHealthOutcome::from_status(StatusCode::OK, None),
            &PiStreamTermination::DownstreamDropped,
        );
        assert_eq!(
            client_drop.provider_health,
            ProviderHealthDisposition::Neutral
        );
        assert!(!client_drop.request_success);

        let non_retryable_error = pi_stream_disposition(
            StatusCode::BAD_REQUEST,
            &ProviderHealthOutcome::from_status(StatusCode::BAD_REQUEST, None),
            &PiStreamTermination::Complete { captured: None },
        );
        assert_eq!(
            non_retryable_error.provider_health,
            ProviderHealthDisposition::Healthy
        );
        assert!(!non_retryable_error.request_success);

        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let credential_rejection = pi_stream_disposition(
                status,
                &ProviderHealthOutcome::from_status(status, None),
                &PiStreamTermination::Complete { captured: None },
            );
            assert_eq!(
                credential_rejection.provider_health,
                ProviderHealthDisposition::Neutral
            );
            assert!(!credential_rejection.request_success);
        }
    }
}
