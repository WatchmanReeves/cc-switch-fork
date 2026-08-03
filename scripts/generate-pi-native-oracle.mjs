#!/usr/bin/env node

/**
 * Generate the vendored Pi raw-schema and composer oracles.
 *
 * The composer expectations are not a local reimplementation. This harness
 * bundles and executes the pinned upstream `composeModelProvider` function.
 * Two inert transport shims satisfy imports that are unreachable while
 * composing credential-blind custom catalog models; the exact shim bytes and
 * harness bytes are recorded in provenance.
 */

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const [piRootArgument, outputArgument] = process.argv.slice(2);
if (!piRootArgument || !outputArgument) {
  throw new Error(
    "usage: generate-pi-native-oracle.mjs <pinned-pi-root> <output-directory>",
  );
}

const piRoot = resolve(piRootArgument);
const outputDirectory = resolve(outputArgument);
const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const generatorRelativePath = "scripts/generate-pi-native-oracle.mjs";
const generatorPath = join(repositoryRoot, generatorRelativePath);
const requireFromPi = createRequire(join(piRoot, "package.json"));
const { Type } = requireFromPi("typebox");
const { Check } = requireFromPi("typebox/value");
const { buildSync, version: esbuildVersion } = requireFromPi("esbuild");
const typeboxVersion = JSON.parse(
  readFileSync(
    join(dirname(dirname(requireFromPi.resolve("typebox"))), "package.json"),
    "utf8",
  ),
).version;

const expectedPiCommit = "ab366ebe94cacd419d986be454f12b1b9913aaca";
const piRepository = "https://github.com/earendil-works/pi.git";
const modelConfigRelativePath =
  "packages/coding-agent/src/core/model-config.ts";
const composerRelativePath =
  "packages/coding-agent/src/core/provider-composer.ts";
const resolverRelativePath =
  "packages/coding-agent/src/core/resolve-config-value.ts";

const modelConfigPath = join(piRoot, modelConfigRelativePath);
const composerPath = join(piRoot, composerRelativePath);
const resolverPath = join(piRoot, resolverRelativePath);
const modelConfigSource = readFileSync(modelConfigPath, "utf8");
const composerSource = readFileSync(composerPath, "utf8");
const resolverSource = readFileSync(resolverPath, "utf8");
const generatorSource = readFileSync(generatorPath, "utf8");
const actualPiCommit = execFileSync(
  "git",
  ["-C", piRoot, "rev-parse", "HEAD"],
  { encoding: "utf8" },
).trim();

if (actualPiCommit !== expectedPiCommit) {
  throw new Error(
    `Pi checkout is ${actualPiCommit}, expected ${expectedPiCommit}`,
  );
}

function sha256(content) {
  return createHash("sha256").update(content).digest("hex");
}

function writeJson(filename, value) {
  const content = `${JSON.stringify(value, null, 2)}\n`;
  writeFileSync(join(outputDirectory, filename), content);
  return { content, sha256: sha256(content) };
}

function extractModelsSchema(source) {
  const start = source.indexOf("const PercentileCutoffsSchema");
  const end = source.indexOf("const validateModelsConfig");
  if (start < 0 || end <= start) {
    throw new Error("pinned model-config.ts schema block was not found");
  }
  const schemaProgram = source.slice(start, end);
  return Function(
    "Type",
    `${schemaProgram}; return ModelsConfigSchema;`,
  )(Type);
}

const modelsSchema = extractModelsSchema(modelConfigSource);
const providerSchema =
  modelsSchema.properties.providers.patternProperties["^.*$"];

const allThinkingLevels = {
  off: null,
  minimal: "minimal-effort",
  low: "low-effort",
  medium: "medium-effort",
  high: "high-effort",
  xhigh: "xhigh-effort",
  max: "max-effort",
};

const allCostFields = {
  input: 0.11,
  output: 0.22,
  cacheRead: 0.033,
  cacheWrite: 0.044,
  tiers: [
    {
      inputTokensAbove: 1000.5,
      input: 0.55,
      output: 0.66,
      cacheRead: 0.077,
      cacheWrite: 0.088,
    },
  ],
};

const allCompatFields = {
  supportsStore: true,
  supportsDeveloperRole: true,
  supportsReasoningEffort: true,
  supportsUsageInStreaming: true,
  maxTokensField: "max_completion_tokens",
  requiresToolResultName: true,
  requiresAssistantAfterToolResult: true,
  requiresThinkingAsText: true,
  requiresReasoningContentOnAssistantMessages: true,
  thinkingFormat: "chat-template",
  chatTemplateKwargs: {
    stringValue: "literal",
    numberValue: 1.25,
    booleanValue: true,
    nullValue: null,
    variableValue: {
      $var: "thinking.effort",
      omitWhenOff: true,
    },
  },
  cacheControlFormat: "anthropic",
  openRouterRouting: {
    allow_fallbacks: true,
    require_parameters: true,
    data_collection: "deny",
    zdr: true,
    enforce_distillable_text: true,
    order: ["provider-a", "provider-b"],
    only: ["provider-a"],
    ignore: ["provider-z"],
    quantizations: ["fp8"],
    sort: {
      by: "price",
      partition: null,
    },
    max_price: {
      prompt: 1.1,
      completion: "2.2",
      image: 3.3,
      audio: "4.4",
      request: 5.5,
    },
    preferred_min_throughput: {
      p50: 10.5,
      p75: 9.5,
      p90: 8.5,
      p99: 7.5,
    },
    preferred_max_latency: {
      p50: 100.5,
      p75: 200.5,
      p90: 300.5,
      p99: 400.5,
    },
  },
  vercelGatewayRouting: {
    only: ["provider-a"],
    order: ["provider-a", "provider-b"],
  },
  supportsOpenAIGrammarTools: true,
  supportsStrictMode: true,
  sendSessionAffinityHeaders: true,
  deferredToolsMode: "kimi",
  sessionAffinityFormat: "openrouter",
  supportsLongCacheRetention: true,
  supportsToolSearch: true,
  supportsEagerToolInputStreaming: true,
  supportsCacheControlOnTools: true,
  supportsTemperature: true,
  forceAdaptiveThinking: true,
  allowEmptySignature: true,
  supportsStrictTools: true,
  supportsToolReferences: true,
};

const rawInputs = [
  {
    id: "all-schema-fields-valid",
    input: {
      name: "All Fields Provider",
      baseUrl: "https://all-fields.example/v1",
      apiKey: "literal-all-fields-key",
      api: "openai-responses",
      oauth: "radius",
      headers: {
        "x-provider-field": "provider-value",
      },
      compat: allCompatFields,
      authHeader: true,
      models: [
        {
          id: "all-fields-model",
          name: "All Fields Model",
          api: "anthropic-messages",
          baseUrl: "https://all-fields-model.example/v1",
          reasoning: true,
          thinkingLevelMap: allThinkingLevels,
          input: ["text", "image"],
          cost: allCostFields,
          contextWindow: 128000.5,
          maxTokens: 16384.25,
          headers: {
            "x-model-field": "model-value",
          },
          compat: allCompatFields,
        },
      ],
      modelOverrides: {
        "all-fields-model": {
          name: "All Fields Override",
          reasoning: false,
          thinkingLevelMap: allThinkingLevels,
          input: ["image", "text"],
          cost: allCostFields,
          contextWindow: 256000.75,
          maxTokens: 32768.5,
          headers: {
            "x-override-field": "override-value",
          },
          compat: allCompatFields,
        },
      },
    },
  },
  { id: "empty-provider-object", input: {} },
  { id: "null-provider", input: null },
  {
    id: "additional-provider-property",
    input: { futureProviderField: { nested: true } },
  },
  { id: "empty-present-base-url", input: { baseUrl: "" } },
  { id: "non-url-base-url-is-raw-string", input: { baseUrl: "not a URL" } },
  { id: "radius-oauth", input: { oauth: "radius" } },
  { id: "unknown-oauth-literal", input: { oauth: "other" } },
  { id: "models-null", input: { models: null } },
  {
    id: "model-missing-id",
    input: { models: [{ api: "openai-responses" }] },
  },
  { id: "model-empty-id", input: { models: [{ id: "" }] } },
  {
    id: "integer-model-numbers",
    input: {
      models: [{ id: "integer", contextWindow: 128000, maxTokens: 16384 }],
    },
  },
  {
    id: "fractional-model-numbers",
    input: {
      models: [
        {
          id: "fractional",
          contextWindow: 128000.5,
          maxTokens: 16384.25,
        },
      ],
    },
  },
  {
    id: "negative-number-is-still-typebox-number",
    input: { models: [{ id: "negative", maxTokens: -1.5 }] },
  },
  {
    id: "string-is-not-number",
    input: { models: [{ id: "string-limit", maxTokens: "16384" }] },
  },
  {
    id: "complete-cost-with-fractions",
    input: {
      models: [
        {
          id: "priced",
          cost: {
            input: 0.1,
            output: 0.2,
            cacheRead: 0.03,
            cacheWrite: 0.04,
            tiers: [
              {
                inputTokensAbove: 1000.5,
                input: 0.5,
                output: 0.6,
                cacheRead: 0.07,
                cacheWrite: 0.08,
              },
            ],
          },
        },
      ],
    },
  },
  {
    id: "incomplete-model-cost",
    input: {
      models: [
        {
          id: "priced",
          cost: { input: 0.1, output: 0.2, cacheRead: 0.03 },
        },
      ],
    },
  },
  {
    id: "compat-openrouter-recursive-valid",
    input: {
      compat: {
        thinkingFormat: "chat-template",
        chatTemplateKwargs: {
          temperature: 0.25,
          thinking: {
            $var: "thinking.effort",
            omitWhenOff: true,
          },
          nullable: null,
        },
        openRouterRouting: {
          data_collection: "deny",
          sort: { by: "price", partition: null },
          max_price: { prompt: 1.5, completion: "2.0" },
          preferred_min_throughput: { p50: 10.5, p99: 2 },
        },
      },
    },
  },
  {
    id: "compat-nested-union-invalid-in-every-branch",
    input: {
      compat: {
        openRouterRouting: {
          preferred_min_throughput: { p50: "fast" },
        },
        supportsToolSearch: "yes",
        supportsTemperature: 1,
      },
    },
  },
  {
    id: "compat-union-additional-field",
    input: { compat: { futureCompatField: [1, 2, 3] } },
  },
  { id: "compat-null", input: { compat: null } },
  {
    id: "thinking-map-unknown-key-and-string-value",
    input: {
      models: [
        {
          id: "thinking",
          thinkingLevelMap: {
            low: null,
            max: "future-effort",
            future: { nested: true },
          },
        },
      ],
    },
  },
  {
    id: "thinking-map-known-key-invalid-value",
    input: {
      models: [{ id: "thinking", thinkingLevelMap: { low: 2 } }],
    },
  },
  {
    id: "compat-sort-string-union-branch",
    input: { compat: { openRouterRouting: { sort: "price" } } },
  },
  {
    id: "compat-sort-object-union-branch",
    input: {
      compat: {
        openRouterRouting: { sort: { by: "price", partition: null } },
      },
    },
  },
  {
    id: "compat-sort-invalid-union-boundary",
    input: { compat: { openRouterRouting: { sort: false } } },
  },
  {
    id: "headers-record-valid",
    input: { headers: { Authorization: "Bearer literal", "x-count": "2" } },
  },
  {
    id: "headers-record-invalid-value",
    input: { headers: { "x-count": 2 } },
  },
  {
    id: "model-override-fractional-and-extra",
    input: {
      modelOverrides: {
        "model/a": {
          contextWindow: 10.5,
          maxTokens: 2.25,
          futureOverrideField: true,
        },
      },
    },
  },
  {
    id: "input-union-invalid",
    input: { models: [{ id: "input", input: ["text", "audio"] }] },
  },
];

const rawOracle = {
  version: 1,
  execution: {
    engine: "typebox Value.Check",
    typeboxVersion,
    schemaTarget:
      "ModelsConfigSchema.properties.providers.patternProperties['^.*$']",
  },
  cases: rawInputs.map(({ id, input }) => ({
    id,
    input,
    expectedValid: Check(providerSchema, input),
  })),
};

const composerCases = [
  {
    id: "combined-all-fields-precedence",
    providerId: "custom-all-fields",
    input: {
      name: "All Fields Provider",
      baseUrl: "https://all-fields.example/v1",
      apiKey: "literal-all-fields-key",
      api: "openai-responses",
      oauth: "radius",
      headers: {
        "x-provider-field": "provider-value",
      },
      compat: allCompatFields,
      authHeader: true,
      models: [
        {
          id: "all-fields-model",
          name: "All Fields Model",
          api: "anthropic-messages",
          baseUrl: "https://all-fields-model.example/v1",
          reasoning: true,
          thinkingLevelMap: allThinkingLevels,
          input: ["text", "image"],
          cost: allCostFields,
          contextWindow: 128000.5,
          maxTokens: 16384.25,
          headers: {
            "x-model-field": "model-value",
          },
          compat: allCompatFields,
        },
      ],
      modelOverrides: {
        "all-fields-model": {
          name: "All Fields Override",
          reasoning: false,
          thinkingLevelMap: allThinkingLevels,
          input: ["image", "text"],
          cost: allCostFields,
          contextWindow: 256000.75,
          maxTokens: 32768.5,
          headers: {
            "x-override-field": "override-value",
          },
          compat: allCompatFields,
        },
      },
    },
  },
  {
    id: "provider-fields-inherited",
    providerId: "custom-provider-fields",
    input: {
      name: "Provider Layer Name",
      baseUrl: "https://provider-fields.example/v1",
      apiKey: "provider-layer-key",
      api: "openai-responses",
      oauth: "radius",
      headers: {
        "x-provider-field": "provider-layer-value",
      },
      compat: allCompatFields,
      authHeader: true,
      models: [{ id: "provider-inherited-model" }],
    },
  },
  {
    id: "model-fields-executed",
    providerId: "custom-model-fields",
    input: {
      apiKey: "model-layer-key",
      models: [
        {
          id: "model-layer-id",
          name: "Model Layer Name",
          api: "anthropic-messages",
          baseUrl: "https://model-fields.example/v1",
          reasoning: true,
          thinkingLevelMap: allThinkingLevels,
          input: ["text", "image"],
          cost: allCostFields,
          contextWindow: 64000.5,
          maxTokens: 8192.25,
          headers: {
            "x-model-field": "model-layer-value",
          },
          compat: allCompatFields,
        },
      ],
    },
  },
  {
    id: "override-fields-executed",
    providerId: "custom-override-fields",
    input: {
      apiKey: "override-layer-key",
      api: "openai-completions",
      baseUrl: "https://override-provider.example/v1",
      models: [
        {
          id: "override-target",
          name: "Definition Before Override",
          reasoning: true,
        },
      ],
      modelOverrides: {
        "override-target": {
          name: "Override Layer Name",
          reasoning: false,
          thinkingLevelMap: allThinkingLevels,
          input: ["image", "text"],
          cost: allCostFields,
          contextWindow: 96000.75,
          maxTokens: 12288.5,
          headers: {
            "x-override-field": "override-layer-value",
          },
          compat: allCompatFields,
        },
      },
    },
  },
  {
    id: "radius-oauth-requires-provider-base-url",
    providerId: "custom-radius-missing-base",
    input: {
      apiKey: "radius-key",
      oauth: "radius",
      models: [
        {
          id: "radius-model",
          api: "openai-responses",
          baseUrl: "https://model-only.example/v1",
        },
      ],
    },
  },
  {
    id: "official-defaults",
    providerId: "custom-defaults",
    input: {
      api: "openai-responses",
      baseUrl: "https://default.example/v1",
      apiKey: "literal-secret",
      models: [{ id: "default-model" }],
    },
  },
  {
    id: "later-model-first-model-fallback",
    providerId: "custom-fallback",
    input: {
      apiKey: "literal-secret",
      models: [
        {
          id: "first",
          api: "anthropic-messages",
          baseUrl: "https://first.example/v1",
        },
        { id: "second" },
      ],
    },
  },
  {
    id: "duplicate-model-replaces-existing-slot",
    providerId: "custom-duplicate",
    input: {
      apiKey: "literal-secret",
      models: [
        {
          id: "same",
          name: "First",
          api: "openai-completions",
          baseUrl: "https://first.example/v1",
        },
        {
          id: "same",
          name: "Second",
          api: "openai-responses",
          baseUrl: "https://second.example/v1",
        },
      ],
    },
  },
  {
    id: "provider-model-override-precedence",
    providerId: "custom-precedence",
    input: {
      api: "openai-completions",
      baseUrl: "https://provider.example/v1",
      apiKey: "literal-secret",
      headers: { layer: "provider", providerOnly: "yes" },
      compat: {
        supportsDeveloperRole: true,
        openRouterRouting: { zdr: true },
      },
      models: [
        {
          id: "precedence",
          name: "Definition",
          api: "openai-responses",
          baseUrl: "https://model.example/v1",
          reasoning: false,
          thinkingLevelMap: { high: "model-high", future: "model-future" },
          contextWindow: 1000.5,
          maxTokens: 100.25,
          headers: { layer: "definition", definitionOnly: "yes" },
          compat: {
            supportsStore: true,
            openRouterRouting: { only: ["definition"], zdr: false },
          },
          cost: {
            input: 1,
            output: 2,
            cacheRead: 0.1,
            cacheWrite: 0.2,
          },
        },
      ],
      modelOverrides: {
        precedence: {
          name: "Override",
          reasoning: true,
          thinkingLevelMap: {
            high: "override-high",
            futureOverride: "preserve",
          },
          contextWindow: 2000.75,
          headers: { layer: "override", overrideOnly: "yes" },
          compat: {
            supportsStore: false,
            openRouterRouting: { order: ["override"] },
          },
          cost: { output: 3.5 },
        },
      },
    },
  },
  {
    id: "fractional-cost-and-limits",
    providerId: "custom-fractional",
    input: {
      api: "google-generative-ai",
      baseUrl: "https://fractional.example/v1",
      apiKey: "literal-secret",
      models: [
        {
          id: "fractional",
          contextWindow: 128000.5,
          maxTokens: 16384.25,
          cost: {
            input: 0.125,
            output: 0.375,
            cacheRead: 0.0625,
            cacheWrite: 0.1875,
          },
        },
      ],
    },
  },
  {
    id: "unmatched-override-is-ignored-by-composer",
    providerId: "custom-unmatched",
    input: {
      api: "google-generative-ai",
      baseUrl: "https://google.example/v1",
      apiKey: "literal-secret",
      models: [{ id: "known" }],
      modelOverrides: {
        missing: {
          maxTokens: 7.5,
          headers: { ignored: "yes" },
        },
      },
    },
  },
  {
    id: "invalid-nonempty-url-is-composed",
    providerId: "custom-invalid-url",
    input: {
      api: "openai-responses",
      baseUrl: "not a URL",
      apiKey: "literal-secret",
      models: [{ id: "model" }],
    },
  },
  {
    id: "unknown-api-is-composed",
    providerId: "custom-future-api",
    input: {
      api: "future-wire-v9",
      baseUrl: "https://future.example/v9",
      apiKey: "literal-secret",
      models: [{ id: "future-model" }],
    },
  },
  {
    id: "unknown-thinking-fields-are-preserved",
    providerId: "custom-thinking",
    input: {
      api: "anthropic-messages",
      baseUrl: "https://thinking.example/v1",
      apiKey: "literal-secret",
      models: [
        {
          id: "thinking",
          thinkingLevelMap: {
            high: "future-high",
            future: { nested: ["opaque"] },
          },
        },
      ],
    },
  },
  {
    id: "empty-url-fails-upstream-composition",
    providerId: "custom-empty-url",
    input: {
      api: "openai-responses",
      baseUrl: "",
      apiKey: "literal-secret",
      models: [{ id: "model" }],
    },
  },
];

function joinFieldPath(parent, token) {
  const escaped = token.replaceAll("~", "~0").replaceAll("/", "~1");
  return `${parent}/${escaped}`;
}

function collectSchemaFieldPaths(schema, pointer = "", output = new Set()) {
  if (!schema || typeof schema !== "object" || Array.isArray(schema)) {
    return output;
  }
  for (const branch of schema.anyOf ?? []) {
    collectSchemaFieldPaths(branch, pointer, output);
  }
  for (const [name, child] of Object.entries(schema.properties ?? {})) {
    const childPointer = joinFieldPath(pointer, name);
    output.add(childPointer);
    collectSchemaFieldPaths(child, childPointer, output);
  }
  for (const child of Object.values(schema.patternProperties ?? {})) {
    const childPointer = joinFieldPath(pointer, "*");
    output.add(childPointer);
    collectSchemaFieldPaths(child, childPointer, output);
  }
  if (schema.items) {
    collectSchemaFieldPaths(
      schema.items,
      joinFieldPath(pointer, "*"),
      output,
    );
  }
  return output;
}

function isPatternContainer(pointer) {
  return (
    pointer.endsWith("/headers") ||
    pointer.endsWith("/modelOverrides") ||
    pointer.endsWith("/chatTemplateKwargs")
  );
}

function collectInputFieldPaths(value, pointer = "", output = new Set()) {
  if (Array.isArray(value)) {
    for (const child of value) {
      collectInputFieldPaths(child, joinFieldPath(pointer, "*"), output);
    }
    return output;
  }
  if (!value || typeof value !== "object") {
    return output;
  }
  for (const [name, child] of Object.entries(value)) {
    const childPointer = joinFieldPath(
      pointer,
      isPatternContainer(pointer) ? "*" : name,
    );
    output.add(childPointer);
    collectInputFieldPaths(child, childPointer, output);
  }
  return output;
}

const schemaFieldPaths = [...collectSchemaFieldPaths(providerSchema)].sort();
const rawCaseFieldPaths = new Map(
  rawInputs.map((entry) => [entry.id, collectInputFieldPaths(entry.input)]),
);
const composerCaseFieldPaths = new Map(
  composerCases.map((entry) => [entry.id, collectInputFieldPaths(entry.input)]),
);
function requiredComposerBehaviorCase(fieldPath) {
  if (fieldPath === "/models" || fieldPath.startsWith("/models/")) {
    return "model-fields-executed";
  }
  if (
    fieldPath === "/modelOverrides" ||
    fieldPath.startsWith("/modelOverrides/")
  ) {
    return "override-fields-executed";
  }
  return "provider-fields-inherited";
}

const fieldCoverageEntries = schemaFieldPaths.map((fieldPath) => {
  const rawOracleCases = [...rawCaseFieldPaths]
    .filter(([, paths]) => paths.has(fieldPath))
    .map(([id]) => id);
  const composerOracleCases = [...composerCaseFieldPaths]
    .filter(([, paths]) => paths.has(fieldPath))
    .map(([id]) => id);
  const composerBehaviorCase = requiredComposerBehaviorCase(fieldPath);
  if (!composerOracleCases.includes(composerBehaviorCase)) {
    throw new Error(
      `Pi field ${fieldPath} is not exercised at its own composition layer by ${composerBehaviorCase}`,
    );
  }
  return {
    fieldPath,
    rawOracleCases,
    composerOracleCases,
    composerBehaviorCase,
  };
});
const uncoveredRawFields = fieldCoverageEntries
  .filter((entry) => entry.rawOracleCases.length === 0)
  .map((entry) => entry.fieldPath);
const uncoveredComposerFields = fieldCoverageEntries
  .filter((entry) => entry.composerOracleCases.length === 0)
  .map((entry) => entry.fieldPath);
if (uncoveredRawFields.length > 0 || uncoveredComposerFields.length > 0) {
  throw new Error(
    `Pi field coverage is incomplete; raw=${JSON.stringify(
      uncoveredRawFields,
    )}, composer=${JSON.stringify(uncoveredComposerFields)}`,
  );
}

const aiShimSource = `
export function lazyStream() {
  throw new Error("transport shim must not execute during composer oracle generation");
}
`;
const compatShimSource = `
export function getApiProvider() {
  throw new Error("transport shim must not execute during composer oracle generation");
}
`;

function projectUpstreamModel(
  model,
  providerConfig,
  resolveCompatibilityRequestConfig,
) {
  const requestConfig = resolveCompatibilityRequestConfig(
    model,
    providerConfig,
    undefined,
  );
  return {
    id: model.id,
    name: model.name,
    api: model.api,
    provider: model.provider,
    baseUrl: model.baseUrl,
    reasoning: model.reasoning,
    ...(model.thinkingLevelMap === undefined
      ? {}
      : { thinkingLevelMap: model.thinkingLevelMap }),
    input: model.input,
    cost: model.cost,
    contextWindow: model.contextWindow,
    maxTokens: model.maxTokens,
    ...(model.compat === undefined ? {} : { compat: model.compat }),
    ...(requestConfig.headers === undefined
      ? {}
      : { headers: requestConfig.headers }),
    authHeader: requestConfig.authHeader,
  };
}

async function runPinnedComposer() {
  const harnessDirectory = mkdtempSync(
    join(piRoot, ".cc-switch-composer-oracle-"),
  );
  try {
    const aiShimPath = join(harnessDirectory, "pi-ai-shim.mjs");
    const compatShimPath = join(harnessDirectory, "pi-ai-compat-shim.mjs");
    const bundlePath = join(harnessDirectory, "provider-composer.mjs");
    writeFileSync(aiShimPath, aiShimSource);
    writeFileSync(compatShimPath, compatShimSource);
    buildSync({
      entryPoints: [composerPath],
      bundle: true,
      platform: "node",
      format: "esm",
      target: "node22",
      outfile: bundlePath,
      packages: "external",
      alias: {
        "@earendil-works/pi-ai": aiShimPath,
        "@earendil-works/pi-ai/compat": compatShimPath,
      },
      logLevel: "silent",
    });
    const bundledComposer = await import(pathToFileURL(bundlePath).href);
    const {
      composeModelProvider,
      resolveCompatibilityRequestConfig,
    } = bundledComposer;
    const cases = await Promise.all(composerCases.map(async ({ id, providerId, input }) => {
      try {
        const modelConfig = {
          getProvider(candidateId) {
            return candidateId === providerId ? input : undefined;
          },
        };
        const provider = composeModelProvider(
          providerId,
          undefined,
          modelConfig,
          undefined,
        );
        const models = provider.getModels();
        const modelIds = new Set(models.map((model) => model.id));
        let authExecution;
        if (provider.auth.apiKey) {
          try {
            const result = await provider.auth.apiKey.resolve({
              ctx: {
                env: async () => undefined,
              },
            });
            authExecution = {
              status: "success",
              entryFunction: "Provider.auth.apiKey.resolve",
              result: result ?? null,
            };
          } catch (error) {
            authExecution = {
              status: "error",
              entryFunction: "Provider.auth.apiKey.resolve",
              error: error instanceof Error ? error.message : String(error),
            };
          }
        } else {
          authExecution = {
            status: "unavailable",
            entryFunction: "Provider.auth.apiKey.resolve",
            reason: "pinned composer exposed no API-key auth method",
          };
        }
        return {
          id,
          providerId,
          input,
          execution: {
            status: "success",
            entryFunctions: [
              "composeModelProvider",
              "Provider.getModels",
              "resolveCompatibilityRequestConfig",
              "Provider.auth.apiKey.resolve",
            ],
          },
          authExecution,
          expected: {
            provider: {
              id: provider.id,
              name: provider.name,
              ...(provider.baseUrl === undefined
                ? {}
                : { baseUrl: provider.baseUrl }),
            },
            models: models.map((model) =>
              projectUpstreamModel(
                model,
                input,
                resolveCompatibilityRequestConfig,
              ),
            ),
            ignoredOverrideKeys: Object.keys(input.modelOverrides ?? {}).filter(
              (modelId) => !modelIds.has(modelId),
            ),
          },
        };
      } catch (error) {
        return {
          id,
          providerId,
          input,
          execution: {
            status: "error",
            entryFunctions: ["composeModelProvider"],
          },
          expectedError:
            error instanceof Error ? error.message : String(error),
        };
      }
    }));
    return {
      cases,
      harness: {
        bundler: `esbuild@${esbuildVersion}`,
        upstreamEntry: composerRelativePath,
        entryFunctions: [
          "composeModelProvider",
          "Provider.getModels",
          "resolveCompatibilityRequestConfig",
          "Provider.auth.apiKey.resolve",
        ],
        transportShims: {
          "pi-ai": sha256(aiShimSource),
          "pi-ai/compat": sha256(compatShimSource),
        },
      },
    };
  } finally {
    rmSync(harnessDirectory, { recursive: true, force: true });
  }
}

async function runPinnedTransportResolver() {
  const harnessDirectory = mkdtempSync(
    join(piRoot, ".cc-switch-transport-oracle-"),
  );
  try {
    const bundlePath = join(harnessDirectory, "resolve-config-value.mjs");
    buildSync({
      entryPoints: [resolverPath],
      bundle: true,
      platform: "node",
      format: "esm",
      target: "node22",
      outfile: bundlePath,
      packages: "external",
      logLevel: "silent",
    });
    const resolver = await import(pathToFileURL(bundlePath).href);
    const cases = [
      {
        id: "literal-value",
        input: "literal-secret",
        environment: {},
      },
      {
        id: "environment-template",
        input: "prefix-${PI_ORACLE_VALUE}-suffix",
        environment: { PI_ORACLE_VALUE: "environment-secret" },
      },
      {
        id: "escaped-dollar-and-bang",
        input: "$$literal-$!bang",
        environment: {},
      },
      {
        id: "shell-command",
        input: "!printf pi-command-value",
        environment: {},
      },
      {
        id: "missing-environment",
        input: "${PI_ORACLE_MISSING}",
        environment: {},
      },
    ].map((entry) => {
      try {
        const value = resolver.resolveConfigValueOrThrow(
          entry.input,
          `oracle ${entry.id}`,
          entry.environment,
        );
        return {
          ...entry,
          execution: {
            status: "success",
            entryFunction: "resolveConfigValueOrThrow",
          },
          expected: value,
        };
      } catch (error) {
        return {
          ...entry,
          execution: {
            status: "error",
            entryFunction: "resolveConfigValueOrThrow",
          },
          expectedError:
            error instanceof Error ? error.message : String(error),
        };
      }
    });
    const headerInput = {
      "x-literal": "literal-header",
      "x-environment": "${PI_ORACLE_HEADER}",
      "x-command": "!printf pi-command-header",
    };
    const headerEnvironment = { PI_ORACLE_HEADER: "environment-header" };
    const expectedHeaders = resolver.resolveHeadersOrThrow(
      headerInput,
      "oracle headers",
      headerEnvironment,
    );
    return {
      engine: "pinned upstream TypeScript",
      bundler: `esbuild@${esbuildVersion}`,
      upstreamEntry: resolverRelativePath,
      platform: process.platform,
      cases,
      headerCase: {
        id: "provider-header-materialization",
        input: headerInput,
        environment: headerEnvironment,
        execution: {
          status: "success",
          entryFunction: "resolveHeadersOrThrow",
        },
        expected: expectedHeaders,
      },
    };
  } finally {
    rmSync(harnessDirectory, { recursive: true, force: true });
  }
}

const composerExecution = await runPinnedComposer();
const transportExecution = await runPinnedTransportResolver();
if (
  !rawOracle.cases.find((entry) => entry.id === "all-schema-fields-valid")
    ?.expectedValid
) {
  throw new Error("the all-fields raw vector was not accepted by pinned TypeBox");
}
if (
  composerExecution.cases.find(
    (entry) => entry.id === "combined-all-fields-precedence",
  )?.execution.status !== "success"
) {
  throw new Error("the all-fields vector did not execute in the pinned composer");
}
const composerOracle = {
  version: 1,
  execution: {
    engine: "pinned upstream TypeScript",
    piCommit: actualPiCommit,
    ...composerExecution.harness,
  },
  cases: composerExecution.cases,
  failClosedCases: [
    {
      id: "builtin-overlay-without-pinned-base-catalog",
      kind: "built_in_overlay",
      unavailableContext: "pinned built-in Provider instance and model catalog",
      rustExpectedStatus: "unknown",
      reasonCode: "catalog_required",
    },
    {
      id: "extension-overlay-without-extension-registration",
      kind: "extension_overlay",
      unavailableContext: "extension ProviderConfigInput and registered model catalog",
      rustExpectedStatus: "unknown",
      reasonCode: "catalog_required",
    },
  ],
};
const transportOracle = {
  version: 1,
  piCommit: actualPiCommit,
  ...transportExecution,
};

function collectSchemaOperators(value, operators = new Set()) {
  if (Array.isArray(value)) {
    for (const child of value) collectSchemaOperators(child, operators);
    return operators;
  }
  if (!value || typeof value !== "object") return operators;
  for (const [key, child] of Object.entries(value)) {
    if (
      [
        "additionalProperties",
        "anyOf",
        "const",
        "items",
        "minLength",
        "patternProperties",
        "properties",
        "required",
        "type",
      ].includes(key)
    ) {
      operators.add(key);
    }
    collectSchemaOperators(child, operators);
  }
  return operators;
}

mkdirSync(outputDirectory, { recursive: true });
const schemaArtifact = writeJson(
  "provider-schema.snapshot.json",
  providerSchema,
);
const rawArtifact = writeJson("raw-oracle-v1.json", rawOracle);
const composerArtifact = writeJson(
  "composer-oracle-v1.json",
  composerOracle,
);
const transportArtifact = writeJson(
  "transport-oracle-v1.json",
  transportOracle,
);
const fieldCoverageArtifact = writeJson("field-coverage-v1.json", {
  version: 1,
  piCommit: actualPiCommit,
  schemaTarget:
    "ModelsConfigSchema.properties.providers.patternProperties['^.*$']",
  assertion:
    "every schema field is present in an actual TypeBox Value.Check vector and in a pinned composer execution dedicated to its provider, model, or override layer",
  fields: fieldCoverageEntries,
});

const provenance = {
  version: 1,
  pi: {
    repository: piRepository,
    commit: actualPiCommit,
  },
  typeboxVersion,
  sources: {
    modelConfig: {
      path: modelConfigRelativePath,
      sha256: sha256(modelConfigSource),
    },
    providerComposer: {
      path: composerRelativePath,
      sha256: sha256(composerSource),
    },
    resolveConfigValue: {
      path: resolverRelativePath,
      sha256: sha256(resolverSource),
    },
  },
  artifacts: {
    "provider-schema.snapshot.json": schemaArtifact.sha256,
    "raw-oracle-v1.json": rawArtifact.sha256,
    "composer-oracle-v1.json": composerArtifact.sha256,
    "transport-oracle-v1.json": transportArtifact.sha256,
    "field-coverage-v1.json": fieldCoverageArtifact.sha256,
  },
  harness: {
    path: generatorRelativePath,
    sha256: sha256(generatorSource),
    upstreamEntry: composerRelativePath,
    entryFunctions: composerExecution.harness.entryFunctions,
    bundler: composerExecution.harness.bundler,
    transportShims: composerExecution.harness.transportShims,
    assertion:
      "expected composer outputs were captured by executing the pinned upstream entry functions; transport shims are unreachable during credential-blind model composition",
    transportResolver: {
      upstreamEntry: resolverRelativePath,
      entryFunctions: [
        "resolveConfigValueOrThrow",
        "resolveHeadersOrThrow",
      ],
      bundler: transportExecution.bundler,
      platform: transportExecution.platform,
    },
  },
  uncoveredSemantics: [
    ...composerOracle.failClosedCases.map(
      ({ id, unavailableContext, rustExpectedStatus, reasonCode }) => ({
        id,
        unavailableContext,
        rustExpectedStatus,
        reasonCode,
      }),
    ),
    {
      id: "radius-oauth-credential-lifecycle",
      unavailableContext:
        "interactive Radius login, refresh credentials, and network exchange",
      rustExpectedStatus: "direct_only",
      reasonCode: "missing_gateway_credential",
    },
  ],
  schemaOperatorInventory: [
    ...collectSchemaOperators(providerSchema),
  ].sort(),
  evaluatorOperatorAllowlist: [
    "additionalProperties",
    "anyOf",
    "const",
    "items",
    "minLength",
    "patternProperties",
    "properties",
    "required",
    "type",
  ],
};
writeJson("provenance-v1.json", provenance);
