//! Gateway-only Pi API families and candidate header planning.
//!
//! The four-family enum is intentionally confined to this module. Raw and
//! managed layers retain opaque API identifiers and cannot accidentally reject
//! a future Pi family merely because the gateway has not implemented it.

#![allow(dead_code)]

use super::composer::{
    PiComposedHeader, PiComposedNativeModel, PiComposerStatus, PiNativeComposition,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use std::fmt;
use url::Url;

/// Headers owned by HTTP framing, the proxy hop, or gateway-generated request
/// identity. Configured providers may not override them.
const GATEWAY_OWNED_HEADERS: &[&str] = &[
    "connection",
    "content-length",
    "forwarded",
    "host",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-proto",
    "x-real-ip",
    "cf-connecting-ip",
    "cf-ipcountry",
    "cf-ray",
    "cf-visitor",
    "true-client-ip",
    "fastly-client-ip",
    "x-azure-clientip",
    "x-azure-fdid",
    "x-azure-ref",
    "akamai-origin-hop",
    "x-akamai-config-log-detail",
    "x-request-id",
    "x-correlation-id",
    "x-trace-id",
    "x-amzn-trace-id",
    "x-b3-traceid",
    "x-b3-spanid",
    "x-b3-parentspanid",
    "x-b3-sampled",
    "traceparent",
    "tracestate",
];
const CANDIDATE_AUTH_HEADERS: &[&str] = &["authorization", "x-api-key", "x-goog-api-key"];
const PROTOCOL_HEADERS: &[&str] = &[
    "anthropic-version",
    "anthropic-beta",
    "openai-beta",
    "openai-version",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfiguredHeaderClass {
    GatewayOwned,
    Protocol,
    CandidateAuth,
    CandidateLocal,
}

fn configured_header_class(name: &HeaderName) -> ConfiguredHeaderClass {
    let name = name.as_str();
    if name.starts_with("proxy-") || GATEWAY_OWNED_HEADERS.contains(&name) {
        ConfiguredHeaderClass::GatewayOwned
    } else if PROTOCOL_HEADERS.contains(&name) {
        ConfiguredHeaderClass::Protocol
    } else if CANDIDATE_AUTH_HEADERS.contains(&name) {
        ConfiguredHeaderClass::CandidateAuth
    } else {
        ConfiguredHeaderClass::CandidateLocal
    }
}

/// Whether an inbound client header must be replaced by candidate-local or
/// gateway-owned transport state before forwarding to an upstream.
///
/// Keep this predicate beside configured-header classification so request
/// filtering and provider validation cannot drift into two deny lists.
pub(crate) fn gateway_replaces_incoming_header(name: &HeaderName) -> bool {
    configured_header_class(name) != ConfiguredHeaderClass::CandidateLocal
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PiGatewayApiFamily {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
    GoogleGenerativeAi,
}

impl PiGatewayApiFamily {
    pub(super) const ALL: [Self; 4] = [
        Self::AnthropicMessages,
        Self::OpenAiCompletions,
        Self::OpenAiResponses,
        Self::GoogleGenerativeAi,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::GoogleGenerativeAi => "google-generative-ai",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|family| family.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiGatewayCapability {
    Proxyable,
    DirectOnly,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiGatewayReasonCode {
    UnsupportedFamily,
    UnsupportedCredentialKind,
    InvalidEndpoint,
    MissingCredential,
    InvalidHeaderName,
    InvalidHeaderValue,
    ProtectedHeader,
    DeferredValueUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiGatewayReason {
    pub code: PiGatewayReasonCode,
    pub json_pointer: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PiGatewayAssessment {
    pub capability: PiGatewayCapability,
    pub reasons: Vec<PiGatewayReason>,
    pub plans: Vec<CandidateHeaderPlan>,
}

#[derive(Clone, PartialEq, Eq)]
struct DeferredHeaderValue {
    raw: String,
}

impl fmt::Debug for DeferredHeaderValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeferredHeaderValue(<redacted>)")
    }
}

impl DeferredHeaderValue {
    fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    fn materialize(
        &self,
        resolver: &impl DeferredValueResolver,
        pointer: &str,
    ) -> Result<HeaderValue, PiGatewayReason> {
        let materialized = if is_deferred(&self.raw) {
            resolver.resolve(&self.raw).ok_or_else(|| PiGatewayReason {
                code: PiGatewayReasonCode::DeferredValueUnavailable,
                json_pointer: pointer.to_string(),
            })?
        } else {
            self.raw.clone()
        };
        parse_transport_header_value(&materialized).ok_or_else(|| PiGatewayReason {
            code: PiGatewayReasonCode::InvalidHeaderValue,
            json_pointer: pointer.to_string(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct CandidateHeaderPlan {
    family: PiGatewayApiFamily,
    endpoint: Url,
    credential: DeferredHeaderValue,
    auth_header: bool,
    provider_headers: Vec<PlannedHeader>,
    model_headers: Vec<PlannedHeader>,
    protocol_identity_predictable: bool,
}

impl fmt::Debug for CandidateHeaderPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CandidateHeaderPlan")
            .field("family", &self.family)
            .field("endpoint", &"<redacted>")
            .field("credential", &"<redacted>")
            .field("auth_header", &self.auth_header)
            .field("provider_headers", &self.provider_headers)
            .field("model_headers", &self.model_headers)
            .field(
                "protocol_identity_predictable",
                &self.protocol_identity_predictable,
            )
            .finish()
    }
}

#[derive(Debug, Clone)]
struct PlannedHeader {
    name: HeaderName,
    json_pointer: String,
    value: DeferredHeaderValue,
    class: ConfiguredHeaderClass,
}

#[derive(Clone)]
pub(crate) struct MaterializedCandidate {
    pub endpoint: Url,
    pub headers: HeaderMap,
    family: PiGatewayApiFamily,
    protocol_headers: HeaderMap,
    protocol_identity_predictable: bool,
}

impl fmt::Debug for MaterializedCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names = self.headers.keys().collect::<Vec<_>>();
        let protocol_header_names = self.protocol_headers.keys().collect::<Vec<_>>();
        formatter
            .debug_struct("MaterializedCandidate")
            .field("endpoint", &"<redacted>")
            .field("header_names", &header_names)
            .field("family", &self.family)
            .field("protocol_header_names", &protocol_header_names)
            .field(
                "protocol_identity_predictable",
                &self.protocol_identity_predictable,
            )
            .finish()
    }
}

pub(crate) trait DeferredValueResolver {
    fn resolve(&self, expression: &str) -> Option<String>;
}

impl<F> DeferredValueResolver for F
where
    F: Fn(&str) -> Option<String>,
{
    fn resolve(&self, expression: &str) -> Option<String> {
        self(expression)
    }
}

impl CandidateHeaderPlan {
    fn build(
        model: &PiComposedNativeModel,
        model_index: usize,
        allow_anthropic_oauth: bool,
    ) -> Result<Self, Vec<PiGatewayReason>> {
        let mut reasons = Vec::new();
        let Some(family) = PiGatewayApiFamily::parse(model.api.as_str()) else {
            reasons.push(PiGatewayReason {
                code: PiGatewayReasonCode::UnsupportedFamily,
                json_pointer: format!("/models/{model_index}/api"),
            });
            return Err(reasons);
        };
        let endpoint = match parse_pi_gateway_endpoint(
            &model.base_url,
            format!("/models/{model_index}/baseUrl"),
        ) {
            Ok(endpoint) => endpoint,
            Err(reason) => {
                reasons.push(reason);
                return Err(reasons);
            }
        };
        let credential = model.api_key.as_ref();
        match credential {
            None => reasons.push(PiGatewayReason {
                code: PiGatewayReasonCode::MissingCredential,
                json_pointer: "/apiKey".to_string(),
            }),
            Some(credential) if !is_deferred(credential) => {
                if parse_transport_header_value(credential).is_none() {
                    reasons.push(PiGatewayReason {
                        code: PiGatewayReasonCode::InvalidHeaderValue,
                        json_pointer: "/apiKey".to_string(),
                    });
                } else if family == PiGatewayApiFamily::AnthropicMessages
                    && is_anthropic_oauth_credential(credential)
                    && !allow_anthropic_oauth
                {
                    reasons.push(PiGatewayReason {
                        code: PiGatewayReasonCode::UnsupportedCredentialKind,
                        json_pointer: "/apiKey".to_string(),
                    });
                }
            }
            Some(_) => {}
        };

        // Anthropic's credential kind changes the pinned wire protocol:
        // OAuth adds a different beta profile. A command-valued credential
        // therefore cannot be pre-classified without executing the command,
        // and commands are deliberately executed only for the one real
        // network attempt.
        let mut protocol_identity_predictable = !matches!(
            (family, credential),
            (PiGatewayApiFamily::AnthropicMessages, Some(value)) if value.starts_with('!')
        );
        let provider_headers = plan_configured_headers(
            &model.provider_headers,
            &mut reasons,
            &mut protocol_identity_predictable,
        );
        let model_headers = plan_configured_headers(
            &model.model_headers,
            &mut reasons,
            &mut protocol_identity_predictable,
        );
        if !reasons.is_empty() {
            return Err(reasons);
        }
        let Some(credential) = credential.cloned() else {
            // Missing credentials are accumulated above so configured-header
            // diagnostics can be returned in the same assessment.
            return Err(vec![PiGatewayReason {
                code: PiGatewayReasonCode::MissingCredential,
                json_pointer: "/apiKey".to_string(),
            }]);
        };

        Ok(Self {
            family,
            endpoint,
            credential: DeferredHeaderValue::new(credential),
            auth_header: model.auth_header,
            provider_headers,
            model_headers,
            protocol_identity_predictable,
        })
    }

    pub(super) fn materialize(
        &self,
        resolver: &impl DeferredValueResolver,
    ) -> Result<MaterializedCandidate, PiGatewayReason> {
        self.materialize_with_policy(resolver, false)
    }

    pub(crate) fn materialize_for_runtime(
        &self,
        resolver: &impl DeferredValueResolver,
    ) -> Result<MaterializedCandidate, PiGatewayReason> {
        self.materialize_with_policy(resolver, true)
    }

    pub(crate) fn with_endpoint(&self, endpoint: &str) -> Result<Self, PiGatewayReason> {
        let endpoint = parse_pi_gateway_endpoint(endpoint, "/customEndpoints")?;
        let mut candidate = self.clone();
        candidate.endpoint = endpoint;
        Ok(candidate)
    }

    /// Resolve only the fields which define failover protocol identity.
    ///
    /// This deliberately does not touch tenant/custom headers and does not
    /// synthesize outbound authentication. The handler uses it before circuit
    /// admission so a skipped primary can still constrain compatible
    /// failovers without executing unrelated credential commands.
    pub(crate) fn materialize_protocol_identity(
        &self,
        resolver: &impl DeferredValueResolver,
    ) -> Result<Option<(PiGatewayApiFamily, HeaderMap)>, PiGatewayReason> {
        if !self.protocol_identity_predictable {
            return Ok(None);
        }

        let mut protocol_headers = HeaderMap::new();
        if self.family == PiGatewayApiFamily::AnthropicMessages {
            // OAuth changes the pinned Anthropic beta contract, so credential
            // kind is the sole auth detail needed by protocol identity.
            let credential = self.credential.materialize(resolver, "/apiKey")?;
            let anthropic_oauth = credential.to_str().is_ok_and(is_anthropic_oauth_credential);
            protocol_headers.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static("2023-06-01"),
            );
            protocol_headers.insert(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_static(if anthropic_oauth {
                    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"
                } else {
                    "interleaved-thinking-2025-05-14"
                }),
            );
        }
        apply_protocol_headers(&self.provider_headers, resolver, &mut protocol_headers)?;
        apply_protocol_headers(&self.model_headers, resolver, &mut protocol_headers)?;
        Ok(Some((self.family, protocol_headers)))
    }

    fn materialize_with_policy(
        &self,
        resolver: &impl DeferredValueResolver,
        allow_anthropic_oauth: bool,
    ) -> Result<MaterializedCandidate, PiGatewayReason> {
        // A new map is allocated for every candidate. No value from a prior
        // candidate can survive failover.
        let mut headers = HeaderMap::new();
        let mut protocol_headers = HeaderMap::new();
        let credential = self.credential.materialize(resolver, "/apiKey")?;
        if self.family == PiGatewayApiFamily::AnthropicMessages
            && credential.to_str().is_ok_and(is_anthropic_oauth_credential)
            && !allow_anthropic_oauth
        {
            return Err(PiGatewayReason {
                code: PiGatewayReasonCode::UnsupportedCredentialKind,
                json_pointer: "/apiKey".to_string(),
            });
        }
        let bearer_credential = credential.clone();
        let anthropic_oauth = self.family == PiGatewayApiFamily::AnthropicMessages
            && credential.to_str().is_ok_and(is_anthropic_oauth_credential);
        let (auth_name, auth_value) = match self.family {
            PiGatewayApiFamily::AnthropicMessages if anthropic_oauth => {
                let credential = credential.to_str().map_err(|_| PiGatewayReason {
                    code: PiGatewayReasonCode::InvalidHeaderValue,
                    json_pointer: "/apiKey".to_string(),
                })?;
                let bearer =
                    HeaderValue::from_str(&format!("Bearer {credential}")).map_err(|_| {
                        PiGatewayReason {
                            code: PiGatewayReasonCode::InvalidHeaderValue,
                            json_pointer: "/apiKey".to_string(),
                        }
                    })?;
                (HeaderName::from_static("authorization"), bearer)
            }
            PiGatewayApiFamily::AnthropicMessages => {
                (HeaderName::from_static("x-api-key"), credential)
            }
            PiGatewayApiFamily::GoogleGenerativeAi => {
                (HeaderName::from_static("x-goog-api-key"), credential)
            }
            PiGatewayApiFamily::OpenAiCompletions | PiGatewayApiFamily::OpenAiResponses => {
                let credential = credential.to_str().map_err(|_| PiGatewayReason {
                    code: PiGatewayReasonCode::InvalidHeaderValue,
                    json_pointer: "/apiKey".to_string(),
                })?;
                let bearer =
                    HeaderValue::from_str(&format!("Bearer {credential}")).map_err(|_| {
                        PiGatewayReason {
                            code: PiGatewayReasonCode::InvalidHeaderValue,
                            json_pointer: "/apiKey".to_string(),
                        }
                    })?;
                (HeaderName::from_static("authorization"), bearer)
            }
        };
        headers.insert(auth_name, auth_value);

        // The pinned Anthropic SDK contributes this protocol default before
        // configured headers. Identity must therefore compare the final value
        // even when the user omitted it.
        if self.family == PiGatewayApiFamily::AnthropicMessages {
            let name = HeaderName::from_static("anthropic-version");
            let value = HeaderValue::from_static("2023-06-01");
            protocol_headers.insert(name.clone(), value.clone());
            headers.insert(name, value);
            let beta = if anthropic_oauth {
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"
            } else {
                "interleaved-thinking-2025-05-14"
            };
            let name = HeaderName::from_static("anthropic-beta");
            let value = HeaderValue::from_static(beta);
            protocol_headers.insert(name.clone(), value.clone());
            headers.insert(name, value);
        }

        // Provider headers are part of provider auth resolution. Pinned SDKs
        // merge them after synthesized family auth.
        apply_planned_headers(
            &self.provider_headers,
            resolver,
            &mut headers,
            &mut protocol_headers,
        )?;

        // Provider-level authHeader is applied after provider headers.
        if self.auth_header {
            let credential = bearer_credential.to_str().map_err(|_| PiGatewayReason {
                code: PiGatewayReasonCode::InvalidHeaderValue,
                json_pointer: "/apiKey".to_string(),
            })?;
            let bearer = HeaderValue::from_str(&format!("Bearer {credential}")).map_err(|_| {
                PiGatewayReason {
                    code: PiGatewayReasonCode::InvalidHeaderValue,
                    json_pointer: "/apiKey".to_string(),
                }
            })?;
            headers.insert(HeaderName::from_static("authorization"), bearer);
        }

        // ModelRuntime then performs a case-insensitive model-header overlay.
        // Keeping this as a separate phase is essential: flattening the two
        // layers before authHeader can send a different credential than Pi.
        apply_planned_headers(
            &self.model_headers,
            resolver,
            &mut headers,
            &mut protocol_headers,
        )?;

        // Main-project OAuth transport is a completed policy boundary, not a
        // partial SDK-header overlay. A configured auth header may override
        // synthesized auth for ordinary credentials (matching pinned Pi), but
        // an Anthropic OAuth credential is always transported as that exact
        // Bearer and never alongside x-api-key.
        if anthropic_oauth {
            headers.remove(HeaderName::from_static("x-api-key"));
            let credential = bearer_credential.to_str().map_err(|_| PiGatewayReason {
                code: PiGatewayReasonCode::InvalidHeaderValue,
                json_pointer: "/apiKey".to_string(),
            })?;
            let bearer = HeaderValue::from_str(&format!("Bearer {credential}")).map_err(|_| {
                PiGatewayReason {
                    code: PiGatewayReasonCode::InvalidHeaderValue,
                    json_pointer: "/apiKey".to_string(),
                }
            })?;
            headers.insert(HeaderName::from_static("authorization"), bearer);
            let beta = HeaderValue::from_static(
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14",
            );
            headers.insert(HeaderName::from_static("anthropic-beta"), beta.clone());
            protocol_headers.insert(HeaderName::from_static("anthropic-beta"), beta);
        }

        let host = authority_header(&self.endpoint).ok_or_else(|| PiGatewayReason {
            code: PiGatewayReasonCode::InvalidEndpoint,
            json_pointer: "/baseUrl".to_string(),
        })?;
        headers.insert(HeaderName::from_static("host"), host);
        Ok(MaterializedCandidate {
            endpoint: self.endpoint.clone(),
            headers,
            family: self.family,
            protocol_headers,
            protocol_identity_predictable: self.protocol_identity_predictable,
        })
    }
}

fn plan_configured_headers(
    entries: &[PiComposedHeader],
    reasons: &mut Vec<PiGatewayReason>,
    protocol_identity_predictable: &mut bool,
) -> Vec<PlannedHeader> {
    let mut planned = Vec::with_capacity(entries.len());
    for entry in entries {
        let Ok(name) = HeaderName::from_bytes(entry.name.as_bytes()) else {
            reasons.push(PiGatewayReason {
                code: PiGatewayReasonCode::InvalidHeaderName,
                json_pointer: entry.json_pointer.clone(),
            });
            continue;
        };
        let class = configured_header_class(&name);
        if class == ConfiguredHeaderClass::GatewayOwned {
            reasons.push(PiGatewayReason {
                code: PiGatewayReasonCode::ProtectedHeader,
                json_pointer: entry.json_pointer.clone(),
            });
            continue;
        }
        if !is_deferred(&entry.value) && parse_transport_header_value(&entry.value).is_none() {
            reasons.push(PiGatewayReason {
                code: PiGatewayReasonCode::InvalidHeaderValue,
                json_pointer: entry.json_pointer.clone(),
            });
            continue;
        }
        if class == ConfiguredHeaderClass::Protocol {
            *protocol_identity_predictable &= !entry.value.starts_with('!');
        }
        planned.push(PlannedHeader {
            name,
            json_pointer: entry.json_pointer.clone(),
            value: DeferredHeaderValue::new(entry.value.clone()),
            class,
        });
    }
    planned
}

fn apply_planned_headers(
    planned: &[PlannedHeader],
    resolver: &impl DeferredValueResolver,
    headers: &mut HeaderMap,
    protocol_headers: &mut HeaderMap,
) -> Result<(), PiGatewayReason> {
    for entry in planned {
        let value = entry.value.materialize(resolver, &entry.json_pointer)?;
        if entry.class == ConfiguredHeaderClass::Protocol {
            protocol_headers.insert(entry.name.clone(), value.clone());
        }
        headers.insert(entry.name.clone(), value);
    }
    Ok(())
}

fn apply_protocol_headers(
    planned: &[PlannedHeader],
    resolver: &impl DeferredValueResolver,
    protocol_headers: &mut HeaderMap,
) -> Result<(), PiGatewayReason> {
    for entry in planned
        .iter()
        .filter(|entry| entry.class == ConfiguredHeaderClass::Protocol)
    {
        protocol_headers.insert(
            entry.name.clone(),
            entry.value.materialize(resolver, &entry.json_pointer)?,
        );
    }
    Ok(())
}

impl MaterializedCandidate {
    pub(crate) fn family(&self) -> PiGatewayApiFamily {
        self.family
    }

    pub(crate) fn failover_protocol_identity(&self) -> Option<(PiGatewayApiFamily, &HeaderMap)> {
        // Auth, tenant and arbitrary custom headers are deliberately excluded.
        self.protocol_identity_predictable
            .then_some((self.family, &self.protocol_headers))
    }

    pub(crate) fn family_name(&self) -> &'static str {
        self.family.as_str()
    }
}

impl CandidateHeaderPlan {
    pub(crate) fn family(&self) -> PiGatewayApiFamily {
        self.family
    }

    pub(crate) fn protocol_identity_is_predictable(&self) -> bool {
        self.protocol_identity_predictable
    }

    pub(crate) fn endpoint(&self) -> &Url {
        &self.endpoint
    }
}

pub(super) fn assess_composition(composition: &PiNativeComposition) -> PiGatewayAssessment {
    if composition.status != PiComposerStatus::Composed {
        return PiGatewayAssessment {
            capability: PiGatewayCapability::Unknown,
            reasons: Vec::new(),
            plans: Vec::new(),
        };
    }
    let mut plans = Vec::with_capacity(composition.models.len());
    let mut reasons = Vec::new();
    for (index, model) in composition.models.iter().enumerate() {
        match CandidateHeaderPlan::build(model, index, false) {
            Ok(plan) => plans.push(plan),
            Err(mut model_reasons) => reasons.append(&mut model_reasons),
        }
    }
    if reasons.is_empty() && plans.len() == composition.models.len() {
        PiGatewayAssessment {
            capability: PiGatewayCapability::Proxyable,
            reasons,
            plans,
        }
    } else {
        PiGatewayAssessment {
            capability: PiGatewayCapability::DirectOnly,
            reasons,
            plans: Vec::new(),
        }
    }
}

/// Main-project data plane assessment. The certified Pre-C assessment remains
/// unchanged and honestly reports Anthropic OAuth as DirectOnly; this entry
/// point becomes reachable only with the complete OAuth transport policy.
pub(crate) fn assess_composition_for_runtime(
    composition: &PiNativeComposition,
) -> PiGatewayAssessment {
    if composition.status != PiComposerStatus::Composed {
        return PiGatewayAssessment {
            capability: PiGatewayCapability::Unknown,
            reasons: Vec::new(),
            plans: Vec::new(),
        };
    }
    let mut plans = Vec::with_capacity(composition.models.len());
    let mut reasons = Vec::new();
    for (index, model) in composition.models.iter().enumerate() {
        match CandidateHeaderPlan::build(model, index, true) {
            Ok(plan) => plans.push(plan),
            Err(mut model_reasons) => reasons.append(&mut model_reasons),
        }
    }
    if reasons.is_empty() && plans.len() == composition.models.len() {
        PiGatewayAssessment {
            capability: PiGatewayCapability::Proxyable,
            reasons,
            plans,
        }
    } else {
        PiGatewayAssessment {
            capability: PiGatewayCapability::DirectOnly,
            reasons,
            plans: Vec::new(),
        }
    }
}

fn authority_header(url: &Url) -> Option<HeaderValue> {
    let host = match url.host()? {
        url::Host::Domain(value) => value.to_string(),
        url::Host::Ipv4(value) => value.to_string(),
        url::Host::Ipv6(value) => format!("[{value}]"),
    };
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };
    HeaderValue::from_str(&authority).ok()
}

fn is_deferred(value: &str) -> bool {
    value.starts_with('!') || value.contains('$')
}

fn is_anthropic_oauth_credential(value: &str) -> bool {
    // Pinned Pi's Anthropic adapter uses `includes`, not a prefix test.
    value.contains("sk-ant-oat")
}

fn parse_transport_header_value(value: &str) -> Option<HeaderValue> {
    if !value.bytes().all(|byte| matches!(byte, 0x20..=0x7e)) {
        return None;
    }
    HeaderValue::from_str(value).ok()
}

/// Parse the one endpoint domain accepted by Pi's gateway.
///
/// Control-plane endpoint mutations and runtime candidate construction both
/// use this function so a value cannot be accepted for storage and rejected
/// only after a request starts. Pi-native base URLs may remain visible as
/// direct-only diagnostics; this validator governs gateway-owned routes.
pub(crate) fn parse_pi_gateway_endpoint(
    value: &str,
    json_pointer: impl Into<String>,
) -> Result<Url, PiGatewayReason> {
    let json_pointer = json_pointer.into();
    let endpoint = Url::parse(value).map_err(|_| PiGatewayReason {
        code: PiGatewayReasonCode::InvalidEndpoint,
        json_pointer: json_pointer.clone(),
    })?;
    if matches!(endpoint.scheme(), "http" | "https")
        && endpoint.host().is_some()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
    {
        Ok(endpoint)
    } else {
        Err(PiGatewayReason {
            code: PiGatewayReasonCode::InvalidEndpoint,
            json_pointer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi_config::composer::compose_explicit_custom_catalog;
    use crate::pi_config::raw_schema::evaluate_provider_value;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    const TRANSPORT_ORACLE_SOURCE: &str =
        include_str!("../../../tests/fixtures/pi/native-oracle/transport-oracle-v1.json");

    fn composed(input: serde_json::Value) -> PiNativeComposition {
        let raw = evaluate_provider_value(&input);
        compose_explicit_custom_catalog(
            "candidate",
            raw.valid_provider.as_ref().expect("raw-valid input"),
        )
    }

    #[test]
    fn only_gateway_module_closes_the_four_family_set() {
        assert_eq!(
            PiGatewayApiFamily::ALL.map(PiGatewayApiFamily::as_str),
            [
                "anthropic-messages",
                "openai-completions",
                "openai-responses",
                "google-generative-ai",
            ]
        );
    }

    #[test]
    fn sensitive_gateway_debug_output_redacts_credentials_and_header_values() {
        let credential = "sk-debug-credential-never-log";
        let header_secret = "debug-header-value-never-log";
        let query_secret = "debug-query-never-log";
        let composition = composed(json!({
            "api": "openai-responses",
            "baseUrl": format!("https://example.test/v1?token={query_secret}"),
            "apiKey": credential,
            "headers": {"x-private": header_secret},
            "models": [{"id": "m"}]
        }));
        let assessment = assess_composition(&composition);
        assert_eq!(assessment.capability, PiGatewayCapability::Proxyable);
        let assessment_debug = format!("{assessment:?}");
        assert!(!assessment_debug.contains(credential));
        assert!(!assessment_debug.contains(header_secret));
        assert!(!assessment_debug.contains(query_secret));

        let materialized = assessment.plans[0]
            .materialize(&|_expression: &str| None)
            .expect("literal plan materializes");
        let materialized_debug = format!("{materialized:?}");
        assert!(!materialized_debug.contains(credential));
        assert!(!materialized_debug.contains(header_secret));
        assert!(!materialized_debug.contains(query_secret));
        assert!(materialized_debug.contains("authorization"));
        assert!(materialized_debug.contains("x-private"));
    }

    #[test]
    fn gateway_rejects_endpoint_userinfo_and_never_debugs_it() {
        let userinfo_secret = "userinfo-secret-never-log";
        let composition = composed(json!({
            "api": "openai-responses",
            "baseUrl": format!("https://user:{userinfo_secret}@example.test/v1"),
            "apiKey": "credential",
            "models": [{"id": "m"}]
        }));
        let assessment = assess_composition(&composition);
        assert_eq!(assessment.capability, PiGatewayCapability::DirectOnly);
        assert_eq!(
            assessment.reasons[0].code,
            PiGatewayReasonCode::InvalidEndpoint
        );
        assert!(!format!("{assessment:?}").contains(userinfo_secret));

        let safe = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://example.test/v1",
            "apiKey": "credential",
            "models": [{"id": "m"}]
        }));
        let plan = assess_composition(&safe).plans.remove(0);
        let error = plan
            .with_endpoint(&format!(
                "https://user:{userinfo_secret}@failover.example/v1"
            ))
            .expect_err("custom endpoint userinfo must be rejected");
        assert_eq!(error.code, PiGatewayReasonCode::InvalidEndpoint);
    }

    #[test]
    fn unknown_api_is_composed_but_direct_only() {
        let composition = composed(json!({
            "api": "future-wire-v9",
            "baseUrl": "https://future.example/v9",
            "apiKey": "literal",
            "models": [{"id": "future"}]
        }));
        assert_eq!(composition.status, PiComposerStatus::Composed);
        let gateway = assess_composition(&composition);
        assert_eq!(gateway.capability, PiGatewayCapability::DirectOnly);
        assert_eq!(
            gateway.reasons[0].code,
            PiGatewayReasonCode::UnsupportedFamily
        );
    }

    #[test]
    fn capability_plan_rejects_invalid_protected_and_hop_headers() {
        for (name, expected) in [
            ("bad header", PiGatewayReasonCode::InvalidHeaderName),
            ("Host", PiGatewayReasonCode::ProtectedHeader),
            ("Content-Length", PiGatewayReasonCode::ProtectedHeader),
            ("Connection", PiGatewayReasonCode::ProtectedHeader),
        ] {
            let composition = composed(json!({
                "api": "openai-responses",
                "baseUrl": "https://example.test/v1",
                "apiKey": "literal",
                "headers": {name: "value"},
                "models": [{"id": "m"}]
            }));
            let gateway = assess_composition(&composition);
            assert_eq!(
                gateway.capability,
                PiGatewayCapability::DirectOnly,
                "{name}"
            );
            assert_eq!(gateway.reasons[0].code, expected, "{name}");
        }

        let composition = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://example.test/v1",
            "apiKey": "literal",
            "headers": {"x-inject": "ok\r\nbad: value"},
            "models": [{"id": "m"}]
        }));
        // TypeBox rejects non-header string syntax only at the gateway layer;
        // the raw schema intentionally accepts arbitrary strings.
        let gateway = assess_composition(&composition);
        assert_eq!(
            gateway.reasons[0].code,
            PiGatewayReasonCode::InvalidHeaderValue
        );
    }

    #[test]
    fn deferred_materialization_is_per_candidate_and_precedes_network_io() {
        let first = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://first.example:8443/v1",
            "apiKey": "${FIRST_KEY}",
            "headers": {"x-tenant": "${FIRST_TENANT}"},
            "models": [{"id": "m"}]
        }));
        let second = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://second.example/v1",
            "apiKey": "${SECOND_KEY}",
            "headers": {"x-tenant": "${SECOND_TENANT}"},
            "models": [{"id": "m"}]
        }));
        let first_plan = assess_composition(&first).plans.remove(0);
        let second_plan = assess_composition(&second).plans.remove(0);

        let first_values = BTreeMap::from([
            ("${FIRST_KEY}".to_string(), "first-secret".to_string()),
            ("${FIRST_TENANT}".to_string(), "tenant-a".to_string()),
        ]);
        let first_materialized = first_plan
            .materialize(&|expression: &str| first_values.get(expression).cloned())
            .expect("first materialization");
        assert_eq!(
            first_materialized.headers[&HeaderName::from_static("host")],
            "first.example:8443"
        );
        assert_eq!(
            first_materialized.headers[&HeaderName::from_static("authorization")],
            "Bearer first-secret"
        );

        let missing = second_plan.materialize(&|_expression: &str| None);
        assert_eq!(
            missing.expect_err("missing deferred value").code,
            PiGatewayReasonCode::DeferredValueUnavailable
        );

        let second_values = BTreeMap::from([
            ("${SECOND_KEY}".to_string(), "second-secret".to_string()),
            ("${SECOND_TENANT}".to_string(), "tenant-b".to_string()),
        ]);
        let second_materialized = second_plan
            .materialize(&|expression: &str| second_values.get(expression).cloned())
            .expect("second materialization");
        assert_eq!(
            second_materialized.headers[&HeaderName::from_static("host")],
            "second.example"
        );
        assert_eq!(
            second_materialized.headers[&HeaderName::from_static("authorization")],
            "Bearer second-secret"
        );
        assert_eq!(
            second_materialized.headers[&HeaderName::from_static("x-tenant")],
            "tenant-b"
        );
        assert_ne!(
            first_materialized.headers[&HeaderName::from_static("authorization")],
            second_materialized.headers[&HeaderName::from_static("authorization")]
        );
        assert_eq!(
            first_materialized.failover_protocol_identity(),
            second_materialized.failover_protocol_identity()
        );
    }

    #[test]
    fn protocol_identity_uses_final_values_but_excludes_auth_tenant_and_custom_headers() {
        let first = composed(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://first.example/v1",
            "apiKey": "first-secret",
            "headers": {
                "anthropic-version": "${VERSION_A}",
                "x-tenant": "tenant-a",
                "x-private": "private-a"
            },
            "models": [{"id": "m"}]
        }));
        let second = composed(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://second.example/v1",
            "apiKey": "second-secret",
            "headers": {
                "anthropic-version": "${VERSION_B}",
                "x-tenant": "tenant-b",
                "x-private": "private-b"
            },
            "models": [{"id": "m"}]
        }));
        let first = assess_composition(&first)
            .plans
            .remove(0)
            .materialize(&|expression: &str| {
                (expression == "${VERSION_A}").then(|| "2023-06-01".to_string())
            })
            .expect("first protocol materialization");
        let same_protocol = assess_composition(&second)
            .plans
            .remove(0)
            .materialize(&|expression: &str| {
                (expression == "${VERSION_B}").then(|| "2023-06-01".to_string())
            })
            .expect("second protocol materialization");
        assert_eq!(
            first.failover_protocol_identity(),
            same_protocol.failover_protocol_identity(),
            "auth, origin, tenant and arbitrary custom headers do not affect wire identity"
        );

        let changed = composed(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://third.example/v1",
            "apiKey": "third-secret",
            "headers": {"anthropic-version": "2024-01-01"},
            "models": [{"id": "m"}]
        }));
        let changed = assess_composition(&changed)
            .plans
            .remove(0)
            .materialize(&|_expression: &str| None)
            .expect("changed protocol materialization");
        assert_ne!(
            first.failover_protocol_identity(),
            changed.failover_protocol_identity()
        );
    }

    #[test]
    fn deferred_protocol_headers_fail_before_candidate_use_and_commands_are_ineligible() {
        let composition = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://candidate.example/v1",
            "apiKey": "literal",
            "headers": {"openai-version": "${OPENAI_VERSION}"},
            "models": [{"id": "m"}]
        }));
        let plan = assess_composition(&composition).plans.remove(0);
        assert_eq!(
            plan.materialize(&|_expression: &str| None)
                .expect_err("unresolved protocol material")
                .code,
            PiGatewayReasonCode::DeferredValueUnavailable
        );

        let command = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://candidate.example/v1",
            "apiKey": "literal",
            "headers": {"openai-version": "!resolve-version"},
            "models": [{"id": "m"}]
        }));
        let command = assess_composition(&command)
            .plans
            .remove(0)
            .materialize(&|expression: &str| {
                (expression == "!resolve-version").then(|| "2024-01-01".to_string())
            })
            .expect("command materializes for direct candidate use");
        assert!(
            command.failover_protocol_identity().is_none(),
            "unpredictable protocol commands are failover-ineligible"
        );
    }

    #[test]
    fn deferred_header_values_use_one_post_resolution_validator() {
        let expression = "!echo café";
        let composition = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://candidate.example/v1",
            "apiKey": "literal",
            "headers": {"x-tenant": expression},
            "models": [{"id": "m"}]
        }));
        let plan = assess_composition(&composition).plans.remove(0);
        let resolved = plan
            .materialize(&|value: &str| {
                (value == expression).then(|| "resolved-secret".to_string())
            })
            .expect("the resolved visible-ASCII value is valid");
        assert_eq!(
            resolved.headers[&HeaderName::from_static("x-tenant")],
            "resolved-secret"
        );
        assert_eq!(
            plan.materialize(&|value: &str| (value == expression).then(|| "café".to_string()))
                .expect_err("the resolved value still passes through transport validation")
                .code,
            PiGatewayReasonCode::InvalidHeaderValue
        );

        let literal = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://candidate.example/v1",
            "apiKey": "literal",
            "headers": {"x-tenant": "café"},
            "models": [{"id": "m"}]
        }));
        assert_eq!(
            assess_composition(&literal).reasons[0].code,
            PiGatewayReasonCode::InvalidHeaderValue
        );
    }

    #[test]
    fn auth_header_adds_candidate_local_bearer_without_reusing_another_candidate() {
        let composition = composed(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://candidate.example/v1",
            "apiKey": "${KEY}",
            "authHeader": true,
            "models": [{"id": "m"}]
        }));
        let materialized = assess_composition(&composition)
            .plans
            .remove(0)
            .materialize(&|expression: &str| {
                (expression == "${KEY}").then(|| "candidate-secret".to_string())
            })
            .expect("authHeader materialization");
        assert_eq!(
            materialized.headers[&HeaderName::from_static("x-api-key")],
            "candidate-secret"
        );
        assert_eq!(
            materialized.headers[&HeaderName::from_static("authorization")],
            "Bearer candidate-secret"
        );
    }

    #[test]
    fn model_headers_overlay_provider_auth_case_insensitively() {
        for (provider_name, model_name) in [
            ("authorization", "Authorization"),
            ("Authorization", "authorization"),
        ] {
            let composition = composed(json!({
                "api": "anthropic-messages",
                "baseUrl": "https://candidate.example/v1",
                "apiKey": "candidate-secret",
                "authHeader": true,
                "headers": {provider_name: "Bearer provider-token"},
                "models": [{
                    "id": "m",
                    "headers": {model_name: "Bearer model-token"}
                }]
            }));
            let materialized = assess_composition(&composition)
                .plans
                .remove(0)
                .materialize(&|_expression: &str| None)
                .expect("layered header materialization");
            assert_eq!(
                materialized.headers[&HeaderName::from_static("authorization")],
                "Bearer model-token",
                "pinned ModelRuntime applies model headers after provider authHeader"
            );
        }
    }

    #[test]
    fn anthropic_protocol_identity_includes_the_sdk_default() {
        let omitted = composed(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://first.example/v1",
            "apiKey": "first",
            "models": [{"id": "m"}]
        }));
        let explicit = composed(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://second.example/v1",
            "apiKey": "second",
            "headers": {"anthropic-version": "2023-06-01"},
            "models": [{"id": "m"}]
        }));
        let omitted = assess_composition(&omitted)
            .plans
            .remove(0)
            .materialize(&|_expression: &str| None)
            .expect("omitted SDK default");
        let explicit = assess_composition(&explicit)
            .plans
            .remove(0)
            .materialize(&|_expression: &str| None)
            .expect("explicit SDK default");
        assert_eq!(
            omitted.headers[&HeaderName::from_static("anthropic-version")],
            "2023-06-01"
        );
        assert_eq!(
            omitted.failover_protocol_identity(),
            explicit.failover_protocol_identity(),
            "wire-equivalent omitted and explicit SDK defaults must remain failover-compatible"
        );
    }

    #[test]
    fn four_families_materialize_their_own_auth_headers() {
        for (family, auth_name, expected_value) in [
            ("anthropic-messages", "x-api-key", "secret"),
            ("openai-completions", "authorization", "Bearer secret"),
            ("openai-responses", "authorization", "Bearer secret"),
            ("google-generative-ai", "x-goog-api-key", "secret"),
        ] {
            let composition = composed(json!({
                "api": family,
                "baseUrl": "https://candidate.example/v1",
                "apiKey": "secret",
                "models": [{"id": "m"}]
            }));
            let mut assessment = assess_composition(&composition);
            assert_eq!(assessment.capability, PiGatewayCapability::Proxyable);
            let materialized = assessment
                .plans
                .remove(0)
                .materialize(&|_expression: &str| None)
                .expect("literal materialization");
            assert_eq!(
                materialized.headers[&HeaderName::from_bytes(auth_name.as_bytes()).unwrap()],
                expected_value
            );
        }
    }

    #[test]
    fn candidate_host_header_preserves_ipv6_authority_brackets() {
        let composition = composed(json!({
            "api": "openai-responses",
            "baseUrl": "http://[::1]:8443/v1",
            "apiKey": "secret",
            "models": [{"id": "m"}]
        }));
        let materialized = assess_composition(&composition)
            .plans
            .remove(0)
            .materialize(&|_expression: &str| None)
            .expect("IPv6 endpoint");
        assert_eq!(
            materialized.headers[&HeaderName::from_static("host")],
            "[::1]:8443"
        );
    }

    #[test]
    fn completed_runtime_oauth_policy_forces_bearer_beta_and_no_x_api_key() {
        let composition = composed(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://candidate.example",
            "apiKey": "prefix-sk-ant-oat01-token-suffix",
            "headers": {
                "authorization": "Bearer configured",
                "x-api-key": "configured-api-key",
                "anthropic-beta": "configured-beta"
            },
            "models": [{"id": "m"}]
        }));
        let certified = assess_composition(&composition);
        assert_eq!(certified.capability, PiGatewayCapability::DirectOnly);
        assert_eq!(
            certified.reasons[0].code,
            PiGatewayReasonCode::UnsupportedCredentialKind
        );

        let mut runtime = assess_composition_for_runtime(&composition);
        assert_eq!(runtime.capability, PiGatewayCapability::Proxyable);
        let materialized = runtime
            .plans
            .remove(0)
            .materialize_for_runtime(&|_expression: &str| None)
            .expect("the complete main-project OAuth policy is proxyable");
        assert_eq!(
            materialized.headers[&HeaderName::from_static("authorization")],
            "Bearer prefix-sk-ant-oat01-token-suffix"
        );
        assert!(materialized.headers.get("x-api-key").is_none());
        assert_eq!(
            materialized.headers[&HeaderName::from_static("anthropic-beta")],
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"
        );
    }

    #[test]
    fn anthropic_command_credentials_are_single_direct_attempts_after_materialization() {
        for (resolved, expected_auth, oauth) in [
            ("ordinary-secret", "ordinary-secret", false),
            (
                "prefix-sk-ant-oat01-token-suffix",
                "Bearer prefix-sk-ant-oat01-token-suffix",
                true,
            ),
        ] {
            let composition = composed(json!({
                "api": "anthropic-messages",
                "baseUrl": "https://candidate.example",
                "apiKey": "!credential-command",
                "models": [{"id": "m"}]
            }));
            let mut assessment = assess_composition_for_runtime(&composition);
            assert_eq!(assessment.capability, PiGatewayCapability::Proxyable);
            let plan = assessment.plans.remove(0);
            assert!(
                !plan.protocol_identity_is_predictable(),
                "credential commands affecting Anthropic beta must not be pre-executed"
            );
            let materialized = plan
                .materialize_for_runtime(&|expression: &str| {
                    (expression == "!credential-command").then(|| resolved.to_string())
                })
                .expect("materialize command result once");
            let auth_name = if oauth { "authorization" } else { "x-api-key" };
            assert_eq!(
                materialized.headers[&HeaderName::from_static(auth_name)],
                expected_auth
            );
            assert_eq!(
                materialized.headers.get("x-api-key").is_none(),
                oauth,
                "OAuth must never be proxied through x-api-key"
            );
        }
    }

    #[test]
    fn deferred_values_replay_actual_pinned_pi_transport_results() {
        let oracle: Value =
            serde_json::from_str(TRANSPORT_ORACLE_SOURCE).expect("parse transport oracle");
        for case in oracle["cases"].as_array().expect("transport cases") {
            let input = case["input"].as_str().expect("transport input");
            let composition = composed(json!({
                "api": "openai-responses",
                "baseUrl": "https://candidate.example/v1",
                "apiKey": input,
                "models": [{"id": "m"}]
            }));
            let plan = assess_composition(&composition).plans.remove(0);
            match case.pointer("/execution/status").and_then(Value::as_str) {
                Some("success") => {
                    let expected = case["expected"].as_str().expect("actual Pi result");
                    let materialized = plan
                        .materialize(&|expression: &str| {
                            (expression == input).then(|| expected.to_string())
                        })
                        .expect("replay actual Pi resolver result");
                    let expected_bearer = format!("Bearer {expected}");
                    assert_eq!(
                        materialized.headers[&HeaderName::from_static("authorization")],
                        expected_bearer,
                        "transport case '{}'",
                        case["id"]
                    );
                }
                Some("error") => {
                    assert!(case["expectedError"].is_string());
                    assert_eq!(
                        plan.materialize(&|_expression: &str| None)
                            .expect_err("actual Pi resolver failure must discard candidate")
                            .code,
                        PiGatewayReasonCode::DeferredValueUnavailable,
                        "transport case '{}'",
                        case["id"]
                    );
                }
                status => panic!("unexpected transport status {status:?}"),
            }
        }

        let header_case = &oracle["headerCase"];
        let input_headers = header_case["input"]
            .as_object()
            .expect("transport header input");
        let expected_headers = header_case["expected"]
            .as_object()
            .expect("actual Pi header output");
        let composition = composed(json!({
            "api": "openai-responses",
            "baseUrl": "https://candidate.example/v1",
            "apiKey": "literal-key",
            "headers": input_headers,
            "models": [{"id": "m"}]
        }));
        let materialized = assess_composition(&composition)
            .plans
            .remove(0)
            .materialize(&|expression: &str| {
                input_headers.iter().find_map(|(name, configured)| {
                    (configured.as_str() == Some(expression))
                        .then(|| expected_headers[name].as_str().map(ToOwned::to_owned))
                        .flatten()
                })
            })
            .expect("replay actual Pi header resolver results");
        for (name, expected) in expected_headers {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("oracle header name");
            assert_eq!(
                materialized.headers[&name],
                expected.as_str().expect("oracle header value")
            );
        }
    }
}
