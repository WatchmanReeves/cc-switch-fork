#![cfg(test)]
//! 只读 native inspection 契约测试。
//!
//! ## 目标
//! **pinned Pi 决定什么是合法**。本仓库的 DTO 形状、网关支持范围、头部策略
//! 都不得成为"合法性"的来源:schema 接受的,managed 不得拒绝也不得丢值;
//! Pi 会发出的,网关不得降级;Pi 不接受的形态,我们也不假装支持。
//!
//! ## 六条裁决及其上游证据
//! C1【无损性】pinned schema 对 `thinkingLevelMap` 只约束 7 个标准键
//!    (string|null;oracle 实证 `low: 2` 非法),额外键无约束(oracle 实证
//!    `future: {nested:true}` 合法);`cost`/tier 同样接受未来键。managed 与
//!    **effective 边界**(`effective_pi_model` 是 projection/routing/failover
//!    的共同入口)都必须无损,**空容器与缺席必须保持可区分**(`{}` 之于
//!    thinkingLevelMap、`[]` 之于 cost.tiers 同理)。
//!    据此取代两个既有测试中把收窄固化为断言的部分:
//!    `managed_narrowing_rejects_duplicates_or_unknown_thinking_keys` 与
//!    `unknown_thinking_shape_is_lossless_for_composer_and_narrowed_separately`
//!    (授权改写、可改名;DuplicateModelId 与 composer 无损两个语义由本套件
//!    直接接管)。若 `InvalidThinkingLevel` 变体因此不再可构造,授权移除。
//! C2【认证头】authorization / x-api-key / x-goog-api-key 是候选认证头,
//!    不是 protected。取值次序据 pinned SDK 与 composer 源码:authHeader 未
//!    设时显式头优先于 apiKey 合成值(Anthropic/OpenAI SDK 按"合成 auth →
//!    显式 headers"合并,后项覆盖);authHeader:true 时合成 Bearer 反过来
//!    优先(pinned provider-composer 在自定义头之后写入,且只写
//!    Authorization、不动 x-api-key)。**header-only 凭证对四族都不是 Pi 原生
//!    可请求形态**:pinned `ModelRuntime.prepareRequest()` 先解析 auth,得不到
//!    AuthResult 即抛 "Provider is not configured",在合并 headers 之前返回,
//!    而 headers 本身永不产生 AuthResult(Google adapter 更是无条件要 apiKey)。
//!    故无 apiKey 时维持 MissingCredential 降级,但认证头本身仍不得被报为
//!    ProtectedHeader。
//! C3【传输层】放宽认证头不得连带放宽传输层:逐跳头完整覆盖并以 `proxy-`
//!    **前缀**拒绝;契约 header 六分类中的 Gateway/HTTP owned(proxy trace /
//!    CDN 客户端身份 / 分布式追踪)同样拒绝,清单与生产 forwarder 无条件
//!    剥离的集合对齐。
//! C4【deferred 值的校验时机】pinned `resolveConfigValueOrThrow()` 先执行
//!    `!command` / 展开 `${ENV}`,再使用结果;**从不按 HTTP 头规则校验原始
//!    表达式**(命令输出 trim,环境模板不 trim,解析结果亦不做头合法性校验)。
//!    因此原始表达式含头非法字符、而解析结果合法的配置必须被接受;头合法性
//!    校验只能发生在物化之后(这是网关自身的传输约束,保留)。**字面量值仍在
//!    判定期校验,且该规则对 credential 与 header 一视同仁**——判定期说
//!    "可代理"而每次物化必然失败,是判定层与执行层自相矛盾。
//!
//! C5【凭证种类,2026-08-02 新增,**已 request-capture 实证**】pinned
//!    Anthropic 传输层以 `apiKey.includes("sk-ant-oat")`(子串,非前缀)判定
//!    OAuth,命中则以 `Authorization: Bearer` 发送、**不发 x-api-key**,并附
//!    `anthropic-beta: claude-code-20250219,oauth-2025-04-20,...`;**models.json
//!    里的字面量 apiKey 同样会走该分支**;该判定**只在 Anthropic 族**,同形
//!    token 在 OpenAI 族仍按普通 Bearer 发送。因此网关不得把这类凭证当普通
//!    x-api-key 代理:字面量命中即判定期 DirectOnly 并给结构化理由(不得是
//!    MissingCredential);deferred 凭证判定期不可知,则**物化期解析出命中值
//!    时必须失败**,绝不发出错误的认证形态。
//!    **完整 OAuth 传输(Bearer + oauth beta 值)不在前置 C 范围**——按
//!    项目范围划分,gateway 数据面属主工程,且需要
//!    先补 request-capture oracle。本工程只保证判定诚实、不发错凭证。
//! C6【entry 隔离,2026-08-02 新增】pinned Pi 逐 entry 做 TypeBox 判定,
//!    单个 entry 的取值错误(如 `contextWindow: 1e400`)只令该 entry 非法;
//!    整文件解析失败会让合法的兄弟 entry 被连坐隐藏,违反四层判定"每个
//!    entry 独立"的核心设计。
//!
//! ## 实现方义务(不在本文件断言,交盲审核查)
//! O1 `compat` 需复现 JavaScript object-spread 对嵌套值(尤其数组)的语义;
//! O2 架构扫描器:cfg 布尔语义(`cfg(not(test))` 的生产代码必须被扫描)、
//!    不得按 `tests/` 路径整体跳过文件、嵌套模块须继承父层归属。
//!
//! ## 上游实证(request-capture,2026-08-02)
//! `scripts/pi-transport-capture.mjs` 以本地抓包端点作 baseUrl,用 pinned Pi
//! 的 adapter 真发请求,实测矩阵(据此 C2/C5 不再是"读源码推断"):
//! - anthropic 普通 key → `x-api-key: <key>`;
//! - anthropic `sk-ant-oat...` → `authorization: Bearer <token>` +
//!   `anthropic-beta: claude-code-20250219,oauth-2025-04-20,...`,**无 x-api-key**;
//! - anthropic apiKey + 显式 `x-api-key` → 发**显式值**(显式覆盖合成);
//! - anthropic apiKey + 显式 `authorization` → 两者**并存**
//!   (`authorization` 取显式值,`x-api-key` 取合成值);
//! - openai responses/completions + 显式 `authorization` → 发**显式值**;
//! - openai + `sk-ant-oat` 形状 token → 仍是普通 `Bearer`,无 OAuth 特殊处理;
//! - openai completions + 显式 `x-api-key` → 与合成 `authorization` **并存**。
//!
//! ## 残余
//! Google 族两值并存的优先级、头名大小写变体未实测;命令输出 trim 与环境模板
//! 不 trim 的差异属数据面语义,本只读面不断言;完整 OAuth 传输实现按范围表
//! 归主工程(harness 已就位,可直接扩为受冻结的 transport oracle);
//! 其余按盲审 finding 处理。

use super::composer::compose_explicit_custom_catalog;
use super::gateway::{assess_composition, PiGatewayCapability, PiGatewayReasonCode};
use super::model::{
    effective_pi_model, validate_pi_managed_provider, PiConfigError, PiManagedAssessment,
    PiManagedProviderConfig, PiManagementStatus, PiRawNativeValidity,
};
use super::native::{inspect_pi_native_catalog, inspect_pi_native_entry};
use super::raw_schema::evaluate_provider_value;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn write_catalog(value: &Value) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("models.json");
    fs::write(&path, serde_json::to_string_pretty(value).expect("encode")).expect("write");
    (temp, path)
}

fn composed_catalog(value: Value) -> super::composer::PiNativeComposition {
    let raw = evaluate_provider_value(&value);
    compose_explicit_custom_catalog(
        "candidate",
        raw.valid_provider.as_ref().expect("raw-valid input"),
    )
}

fn has_gateway_reason(
    gateway: &super::gateway::PiGatewayAssessment,
    code: PiGatewayReasonCode,
) -> bool {
    gateway.reasons.iter().any(|reason| reason.code == code)
}

// ---------------------------------------------------------------------------
// pinned 夹具冻结——oracle 是上游出处工件,不得为过测试再生成
// ---------------------------------------------------------------------------

const PINNED_FIXTURES: &[(&str, &str)] = &[
    (
        "tests/fixtures/pi/native-oracle/composer-oracle-v1.json",
        "f7e54bb84e5fd6d50e5762dc304834410fa73ef608c2f9c42475c5983f8e0cf5",
    ),
    (
        "tests/fixtures/pi/native-oracle/field-coverage-v1.json",
        "b8b85e611cf1dbef86c611df185ba8ac2d64160087d0c6e47747f838a0fafe42",
    ),
    (
        "tests/fixtures/pi/native-oracle/provenance-v1.json",
        "6b2f9570ecc58d54ebe3da094530fee1c8c0d4a8265fa9c3199218582cb8dbcb",
    ),
    (
        "tests/fixtures/pi/native-oracle/provider-schema.snapshot.json",
        "e498c9f1b344eee1bd3c3ba74d1b648dcb835378cfad92800ec80078b825745c",
    ),
    (
        "tests/fixtures/pi/native-oracle/raw-oracle-v1.json",
        "5aaa37160f96a0fe50867d900ca38c73f13aba769e156a883324368d9dbeeb9a",
    ),
    (
        "tests/fixtures/pi/native-oracle/transport-oracle-v1.json",
        "b2c816e53b60da5cd6352d2c23934939e9f6dd0077971488fe9dd36fa723e855",
    ),
    (
        "tests/fixtures/pi/module-boundaries-v1.json",
        "a69ab84fc0db323d5eb8ddc63555a9c69613dda865962f077cd8691639951b4d",
    ),
];

#[test]
fn certify_pinned_fixtures_are_frozen() {
    for (relative, expected) in PINNED_FIXTURES {
        let bytes = fs::read(repo_root().join(relative))
            .unwrap_or_else(|e| panic!("read fixture {relative}: {e}"));
        assert_eq!(
            &format!("{:x}", Sha256::digest(bytes)),
            expected,
            "pinned fixture '{relative}' drifted; fixtures are upstream provenance \
             artifacts and may only change under adjudication"
        );
    }
}

// ---------------------------------------------------------------------------
// 被取代测试中必须保留的语义,由本套件直接接管
// ---------------------------------------------------------------------------

#[test]
fn certify_duplicate_model_id_rejection_is_preserved() {
    let config: PiManagedProviderConfig = serde_json::from_value(json!({
        "api": "anthropic-messages",
        "baseUrl": "https://dup.example",
        "apiKey": "literal",
        "models": [{"id": "same"}, {"id": "same"}]
    }))
    .expect("deserialize managed provider");
    assert_eq!(
        validate_pi_managed_provider(&config),
        Err(PiConfigError::DuplicateModelId("same".into())),
        "duplicate model ids must keep being rejected"
    );
}

#[test]
fn certify_composer_thinking_losslessness_guard() {
    let odd_map = json!({"high": "h", "future": {"opaque": true}});
    let composition = composed_catalog(json!({
        "api": "anthropic-messages",
        "baseUrl": "https://thinking.example",
        "apiKey": "literal",
        "models": [{"id": "m", "thinkingLevelMap": odd_map}]
    }));
    assert_eq!(
        composition.models[0].thinking_level_map.as_ref(),
        Some(&odd_map),
        "composer keeps the raw thinkingLevelMap value verbatim"
    );
}

// ---------------------------------------------------------------------------
// C1:schema 合法值必须无损直到 effective 边界
// ---------------------------------------------------------------------------

#[test]
fn certify_managed_losslessness_through_effective_boundary() {
    // 标准键只取 schema 允许的 string|null;额外键覆盖全部 JSON 类型。
    let model_map = json!({
        "high": "native-high",
        "medium": null,
        "future-level": "textual",
        "vendor": {"opaque": {"nested": true}},
        "budget": 42,
        "enabled": true
    });
    // 与 model_map 共有 "high",用于绑定 override 的覆盖方向。
    let override_map = json!({
        "high": "override-high",
        "low": "override-low",
        "another-future": [1, "two", null]
    });
    let cost = json!({
        "input": 1.5,
        "output": 2.5,
        "cacheRead": 0.5,
        "cacheWrite": 0.25,
        "futureRate": 9.0,
        "tiers": [{
            "inputTokensAbove": 100.0,
            "input": 1.0,
            "output": 2.0,
            "cacheRead": 0.5,
            "cacheWrite": 0.25,
            "futureTierField": "opaque"
        }]
    });
    let catalog = json!({
        "providers": {
            "thinking": {
                "api": "anthropic-messages",
                "baseUrl": "https://thinking.example",
                "apiKey": "literal",
                "models": [
                    {"id": "m", "thinkingLevelMap": model_map.clone(), "cost": cost.clone()},
                    {"id": "empty-map", "thinkingLevelMap": {}},
                    {"id": "absent-map"}
                ],
                "modelOverrides": {"m": {"thinkingLevelMap": override_map.clone()}}
            }
        }
    });
    let (_temp, path) = write_catalog(&catalog);
    let inspection = inspect_pi_native_entry(&path, "thinking", &BTreeMap::new())
        .expect("inspect")
        .expect("entry present");

    assert_eq!(
        inspection.diagnostic.raw_validity,
        PiRawNativeValidity::Valid,
        "the pinned schema accepts additional thinkingLevelMap and cost members"
    );
    assert_eq!(
        inspection.diagnostic.managed_assessment,
        PiManagedAssessment::Manageable,
        "managed must not reject what the executed pin accepts"
    );
    assert_eq!(
        inspection.diagnostic.management_status,
        PiManagementStatus::Importable
    );
    // 以序列化后的字符串码断言,便于 InvalidThinkingLevel 变体被整体移除。
    let reasons = serde_json::to_value(&inspection.diagnostic.reasons).expect("serialize reasons");
    assert!(
        !reasons
            .as_array()
            .expect("reasons array")
            .iter()
            .any(|reason| reason["code"] == "invalid_thinking_level"),
        "no invalid_thinking_level reason may fire for schema-valid input"
    );

    let managed = inspection.managed_config.expect("managed config");
    let round_trip = serde_json::to_value(&managed).expect("serialize managed config");
    assert_eq!(
        round_trip.pointer("/models/0/thinkingLevelMap"),
        Some(&model_map),
        "model thinkingLevelMap must round-trip losslessly"
    );
    assert_eq!(
        round_trip.pointer("/modelOverrides/m/thinkingLevelMap"),
        Some(&override_map),
        "override thinkingLevelMap must round-trip losslessly"
    );
    assert_eq!(
        round_trip.pointer("/models/0/cost"),
        Some(&cost),
        "cost and tier members must round-trip losslessly, including future keys"
    );
    // 空对象与缺席是两种原生形态,序列化必须保持可区分。
    assert_eq!(
        round_trip.pointer("/models/1/thinkingLevelMap"),
        Some(&json!({})),
        "an explicitly empty thinkingLevelMap must survive as an empty object"
    );
    assert_eq!(
        round_trip.pointer("/models/2/thinkingLevelMap"),
        None,
        "an absent thinkingLevelMap must stay absent"
    );

    // effective 是 projection / runtime / routing / failover 的共同入口:
    // DTO 修好后在这里二次收窄同样是丢值。
    let effective = effective_pi_model(&managed, "m").expect("effective model");
    let effective_value = serde_json::to_value(&effective).expect("serialize effective model");
    let mut merged = model_map.as_object().expect("model map").clone();
    for (key, value) in override_map.as_object().expect("override map") {
        merged.insert(key.clone(), value.clone());
    }
    assert_eq!(
        effective_value.pointer("/thinkingLevelMap"),
        Some(&Value::Object(merged)),
        "the effective model must carry the merged map losslessly, with override \
         entries winning on shared keys"
    );
    assert_eq!(
        effective_value.pointer("/cost"),
        Some(&cost),
        "the effective model must not drop cost members either"
    );
}

// ---------------------------------------------------------------------------
// C2:候选认证头不是 protected
// ---------------------------------------------------------------------------

#[test]
fn certify_auth_candidate_headers_are_not_protected() {
    // (a) Anthropic:显式 x-api-key 不得被拒,取值优先于 apiKey 合成值。
    let explicit = composed_catalog(json!({
        "api": "anthropic-messages",
        "baseUrl": "https://anthropic.example",
        "apiKey": "synthesized-secret",
        "headers": {"x-api-key": "explicit-secret"},
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&explicit);
    assert!(
        !has_gateway_reason(&gateway, PiGatewayReasonCode::ProtectedHeader),
        "x-api-key is candidate-auth, not protected"
    );
    assert_eq!(gateway.capability, PiGatewayCapability::Proxyable);
    let materialized = gateway.plans[0]
        .materialize(&|_: &str| None)
        .expect("materialize literal candidate");
    assert_eq!(
        materialized.headers[&http::HeaderName::from_static("x-api-key")],
        http::HeaderValue::from_static("explicit-secret"),
        "explicit config header value takes precedence over synthesized family auth"
    );
    // 认证头永远不进 failover 协议身份。
    if let Some((_, protocol_headers)) = materialized.failover_protocol_identity() {
        assert!(
            !protocol_headers.contains_key(http::HeaderName::from_static("x-api-key")),
            "auth headers must stay out of the failover protocol identity"
        );
    }

    // (b) OpenAI-Responses:显式 authorization 同理。
    let bearer = composed_catalog(json!({
        "api": "openai-responses",
        "baseUrl": "https://openai.example/v1",
        "apiKey": "synthesized-secret",
        "headers": {"authorization": "Bearer configured-token"},
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&bearer);
    assert!(
        !has_gateway_reason(&gateway, PiGatewayReasonCode::ProtectedHeader),
        "authorization is candidate-auth, not protected"
    );
    assert_eq!(gateway.capability, PiGatewayCapability::Proxyable);
    assert_eq!(
        gateway.plans[0]
            .materialize(&|_: &str| None)
            .expect("materialize")
            .headers[&http::HeaderName::from_static("authorization")],
        http::HeaderValue::from_static("Bearer configured-token")
    );

    // (c) Google:显式认证头与 apiKey 并存,不得拒绝、不得降级
    //     (取值优先级不断言——Google SDK 顺序无上游证据)。
    let google = composed_catalog(json!({
        "api": "google-generative-ai",
        "baseUrl": "https://gemini.example",
        "apiKey": "literal",
        "headers": {"x-goog-api-key": "explicit-secret"},
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&google);
    assert!(
        !has_gateway_reason(&gateway, PiGatewayReasonCode::ProtectedHeader),
        "x-goog-api-key is candidate-auth, not protected"
    );
    assert_eq!(gateway.capability, PiGatewayCapability::Proxyable);
}

// ---------------------------------------------------------------------------
// C2:authHeader:true 时合成 Bearer 覆盖显式 Authorization
// ---------------------------------------------------------------------------

#[test]
fn certify_auth_header_bearer_overrides_explicit_authorization() {
    let composition = composed_catalog(json!({
        "api": "anthropic-messages",
        "baseUrl": "https://anthropic.example",
        "apiKey": "synthesized-secret",
        "authHeader": true,
        "headers": {"authorization": "Bearer explicit-token"},
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&composition);
    assert_eq!(
        gateway.capability,
        PiGatewayCapability::Proxyable,
        "an explicit authorization header must not downgrade an authHeader model"
    );
    let materialized = gateway.plans[0]
        .materialize(&|_: &str| None)
        .expect("materialize literal candidate");
    assert_eq!(
        materialized.headers[&http::HeaderName::from_static("authorization")],
        http::HeaderValue::from_static("Bearer synthesized-secret"),
        "with authHeader:true the synthesized Bearer wins (pinned composer writes it \
         after the explicit headers)"
    );
    assert_eq!(
        materialized.headers[&http::HeaderName::from_static("x-api-key")],
        http::HeaderValue::from_static("synthesized-secret"),
        "the Bearer step only rewrites Authorization; family auth stays synthesized"
    );
}

// ---------------------------------------------------------------------------
// C3:传输层与网关自有身份头
// ---------------------------------------------------------------------------

/// 逐跳/传输头。末四项是合成名字:精确枚举无法覆盖,必须按 `proxy-` 前缀拒绝。
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "proxy-future-extension",
    "proxy-tenant-routing",
    "proxy-x9",
];

/// Gateway/HTTP owned:proxy trace / CDN 客户端身份 / 分布式追踪。
/// 与生产 forwarder 无条件剥离的集合对齐,两侧同进退。
const GATEWAY_OWNED_HEADERS: &[&str] = &[
    "forwarded",
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

#[test]
fn certify_transport_owned_headers_stay_protected() {
    let cases = HOP_BY_HOP_HEADERS
        .iter()
        .map(|header| ("hop-by-hop", *header))
        .chain(
            GATEWAY_OWNED_HEADERS
                .iter()
                .map(|header| ("gateway-owned", *header)),
        );
    for (class, header) in cases {
        let composition = composed_catalog(json!({
            "api": "openai-responses",
            "baseUrl": "https://openai.example/v1",
            "apiKey": "literal",
            "headers": {header: "value"},
            "models": [{"id": "m"}]
        }));
        let gateway = assess_composition(&composition);
        assert!(
            has_gateway_reason(&gateway, PiGatewayReasonCode::ProtectedHeader),
            "{class} header '{header}' must be reported as ProtectedHeader"
        );
        assert_eq!(
            gateway.capability,
            PiGatewayCapability::DirectOnly,
            "{class} header '{header}' must keep the model DirectOnly"
        );
    }
}

// ---------------------------------------------------------------------------
// C2:header-only 凭证四族皆非 Pi 原生可请求形态
// ---------------------------------------------------------------------------

#[test]
fn certify_header_only_credentials_stay_direct_only() {
    // pinned ModelRuntime.prepareRequest() 先解析 auth,得不到 AuthResult 即抛
    // "Provider is not configured",在合并 headers 之前返回;headers 永不产生
    // AuthResult。因此"只有认证头、无 apiKey"必须降级——但认证头本身依然是
    // candidate-auth,不得被报为 ProtectedHeader。
    for (api, header) in [
        ("anthropic-messages", "x-api-key"),
        ("openai-completions", "authorization"),
        ("openai-responses", "authorization"),
        ("google-generative-ai", "x-goog-api-key"),
    ] {
        let composition = composed_catalog(json!({
            "api": api,
            "baseUrl": "https://example.test/v1",
            "headers": {header: "header-secret"},
            "models": [{"id": "m"}]
        }));
        let gateway = assess_composition(&composition);
        assert_eq!(
            gateway.capability,
            PiGatewayCapability::DirectOnly,
            "{api}: header-only credentials are not a requestable pinned Pi form"
        );
        assert!(
            has_gateway_reason(&gateway, PiGatewayReasonCode::MissingCredential),
            "{api}: a missing apiKey must be reported as MissingCredential"
        );
        assert!(
            !has_gateway_reason(&gateway, PiGatewayReasonCode::ProtectedHeader),
            "{api}: the auth header itself must not be reported as protected"
        );
    }
}

// ---------------------------------------------------------------------------
// C4:deferred 值只能在物化之后校验
// ---------------------------------------------------------------------------

#[test]
fn certify_deferred_header_values_are_validated_after_resolution() {
    // 原始表达式含头非法字符(非可见 ASCII),解析结果合法。pinned Pi 先执行
    // 再用结果,从不校验原始表达式,故这类配置必须被接受。
    let expression = "!echo café";
    let deferred = composed_catalog(json!({
        "api": "openai-responses",
        "baseUrl": "https://openai.example/v1",
        "apiKey": "literal",
        "headers": {"x-tenant": expression},
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&deferred);
    assert!(
        !has_gateway_reason(&gateway, PiGatewayReasonCode::InvalidHeaderValue),
        "a deferred expression must not be validated as an HTTP header value before \
         it is resolved"
    );
    assert_eq!(gateway.capability, PiGatewayCapability::Proxyable);
    let materialized = gateway.plans[0]
        .materialize(&|value: &str| (value == expression).then(|| "resolved-secret".to_string()))
        .expect("materialize resolved candidate");
    assert_eq!(
        materialized.headers[&http::HeaderName::from_static("x-tenant")],
        http::HeaderValue::from_static("resolved-secret"),
        "the resolved value is what reaches the candidate"
    );

    // 防过度放宽:字面量(非 deferred)含头非法字符仍必须当场拒绝。
    let literal = composed_catalog(json!({
        "api": "openai-responses",
        "baseUrl": "https://openai.example/v1",
        "apiKey": "literal",
        "headers": {"x-tenant": "café"},
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&literal);
    assert!(
        has_gateway_reason(&gateway, PiGatewayReasonCode::InvalidHeaderValue),
        "a literal header value outside visible ASCII must still be rejected"
    );
}

// ---------------------------------------------------------------------------
// C5:OAuth 凭证绝不能按 x-api-key 代理
// ---------------------------------------------------------------------------

#[test]
fn certify_oauth_credentials_are_never_proxied_as_api_key() {
    // 字面量命中:判定期即可知,必须 DirectOnly 并给出结构化理由——
    // 而不是宣称可代理再发出错误的认证形态。
    let literal = composed_catalog(json!({
        "api": "anthropic-messages",
        "baseUrl": "https://anthropic.example",
        "apiKey": "sk-ant-oat01-example-token",
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&literal);
    assert_eq!(
        gateway.capability,
        PiGatewayCapability::DirectOnly,
        "pinned Pi sends an sk-ant-oat credential as an OAuth Bearer with oauth beta \
         headers; proxying it as x-api-key would send the wrong auth form"
    );
    assert!(
        !gateway.reasons.is_empty(),
        "the downgrade must carry a structured reason"
    );
    assert!(
        !has_gateway_reason(&gateway, PiGatewayReasonCode::MissingCredential),
        "the credential is present; MissingCredential would misreport the cause"
    );

    // deferred 凭证:判定期不可知,允许 Proxyable;但物化解析出命中值时必须
    // 失败,绝不发出错误的认证形态。
    let deferred = composed_catalog(json!({
        "api": "anthropic-messages",
        "baseUrl": "https://anthropic.example",
        "apiKey": "!load-token",
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&deferred);
    assert_eq!(
        gateway.capability,
        PiGatewayCapability::Proxyable,
        "a deferred credential's kind is unknowable at plan time"
    );
    assert!(
        gateway.plans[0]
            .materialize(&|_: &str| Some("sk-ant-oat01-resolved".to_string()))
            .is_err(),
        "materialising a resolved OAuth credential must fail rather than send it as \
         a plain api key"
    );

    // 防过度收窄:普通 Anthropic key 不受影响;非 Anthropic 族不适用该判定
    // (pinned 的 includes 检查只在 Anthropic 传输层)。
    for (api, key) in [
        ("anthropic-messages", "sk-ant-api03-plain"),
        ("openai-responses", "sk-ant-oat01-not-anthropic"),
    ] {
        let plain = composed_catalog(json!({
            "api": api,
            "baseUrl": "https://plain.example/v1",
            "apiKey": key,
            "models": [{"id": "m"}]
        }));
        assert_eq!(
            assess_composition(&plain).capability,
            PiGatewayCapability::Proxyable,
            "{api}: the OAuth rule must not over-reach"
        );
    }
}

// ---------------------------------------------------------------------------
// C4 扩展:字面量凭证必须在判定期校验
// ---------------------------------------------------------------------------

#[test]
fn certify_literal_credentials_are_validated_at_plan_time() {
    // 判定期宣称"可代理"、而每次物化必然失败,是判定层与执行层自相矛盾。
    let illegal = composed_catalog(json!({
        "api": "openai-responses",
        "baseUrl": "https://openai.example/v1",
        "apiKey": "café",
        "models": [{"id": "m"}]
    }));
    let gateway = assess_composition(&illegal);
    assert!(
        gateway.plans.is_empty() || gateway.plans[0].materialize(&|_: &str| None).is_err(),
        "sanity: this literal credential can never materialise"
    );
    assert_eq!(
        gateway.capability,
        PiGatewayCapability::DirectOnly,
        "a literal credential that can never materialise must not be judged proxyable"
    );
    assert!(
        !gateway.reasons.is_empty(),
        "the downgrade must carry a structured reason"
    );

    // 对称约束:deferred 凭证仍不得因原始表达式在判定期被拒(C4)。
    let deferred = composed_catalog(json!({
        "api": "openai-responses",
        "baseUrl": "https://openai.example/v1",
        "apiKey": "!echo café",
        "models": [{"id": "m"}]
    }));
    assert_eq!(
        assess_composition(&deferred).capability,
        PiGatewayCapability::Proxyable,
        "a deferred credential must not be validated as a header value before it is \
         resolved"
    );
}

// ---------------------------------------------------------------------------
// C1 扩展:空容器与缺席必须可区分
// ---------------------------------------------------------------------------

#[test]
fn certify_empty_containers_stay_distinct_from_absent() {
    let base_rates = json!({
        "input": 1.0, "output": 2.0, "cacheRead": 0.5, "cacheWrite": 0.25
    });
    let mut with_empty = base_rates.as_object().expect("rates").clone();
    with_empty.insert("tiers".into(), json!([]));
    let catalog = json!({
        "providers": {
            "tiers": {
                "api": "anthropic-messages",
                "baseUrl": "https://tiers.example",
                "apiKey": "literal",
                "models": [
                    {"id": "empty-tiers", "cost": Value::Object(with_empty)},
                    {"id": "absent-tiers", "cost": base_rates.clone()}
                ]
            }
        }
    });
    let (_temp, path) = write_catalog(&catalog);
    let managed = inspect_pi_native_entry(&path, "tiers", &BTreeMap::new())
        .expect("inspect")
        .expect("entry present")
        .managed_config
        .expect("managed config");
    let round_trip = serde_json::to_value(&managed).expect("serialize managed config");
    assert_eq!(
        round_trip.pointer("/models/0/cost/tiers"),
        Some(&json!([])),
        "an explicitly empty tiers list must survive as an empty list"
    );
    assert_eq!(
        round_trip.pointer("/models/1/cost/tiers"),
        None,
        "an absent tiers list must stay absent"
    );
}

// ---------------------------------------------------------------------------
// C6:单个 entry 的错误不得连坐兄弟 entry
// ---------------------------------------------------------------------------

#[test]
fn certify_one_bad_entry_does_not_hide_its_siblings() {
    // pinned Pi 逐 entry 判定:`contextWindow: 1e400` 只令该 entry 非法。
    // 整文件解析失败会让合法条目一并消失,破坏"每个 entry 独立"的判定设计。
    let source = r#"{
  "providers": {
    "healthy": {
      "api": "anthropic-messages",
      "baseUrl": "https://healthy.example",
      "apiKey": "literal",
      "models": [{"id": "m"}]
    },
    "overflow": {
      "api": "anthropic-messages",
      "baseUrl": "https://overflow.example",
      "apiKey": "literal",
      "models": [{"id": "m", "contextWindow": 1e400}]
    }
  }
}"#;
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("models.json");
    fs::write(&path, source).expect("write");

    let diagnostics = inspect_pi_native_catalog(&path, &BTreeMap::new())
        .expect("one malformed entry must not fail the whole catalog");
    assert_eq!(diagnostics.len(), 2, "both entries must still be reported");
    let healthy = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.provider_key == "healthy")
        .expect("healthy entry present");
    assert_eq!(
        healthy.raw_validity,
        PiRawNativeValidity::Valid,
        "a legal sibling must not be hidden by a malformed entry"
    );
    assert_eq!(healthy.management_status, PiManagementStatus::Importable);
    let overflow = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.provider_key == "overflow")
        .expect("overflow entry present");
    assert_ne!(
        overflow.raw_validity,
        PiRawNativeValidity::Valid,
        "the out-of-range contextWindow entry itself must not be judged valid"
    );
}
