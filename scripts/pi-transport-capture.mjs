#!/usr/bin/env node
/**
 * Pi transport request-capture harness.
 *
 * 现有的 native-oracle 只执行 pinned Pi 的 schema evaluator / composer /
 * config-value resolver,**不执行** adapter 与厂商 SDK 的头合并,因此
 * "Pi 实际发出什么认证头" 一直只能靠读源码推断。本脚本补上这一层:
 * 起一个本地 HTTP 抓包端点当 baseUrl,用 pinned Pi 的 adapter 真发一次
 * 请求,记录实际发出的 header。
 *
 * 用法:
 *   PI_CHECKOUT=/path/to/pinned/pi node scripts/pi-transport-capture.mjs
 *
 * 不含任何密钥:测试用的 apiKey 是本地抓包用的假值;若要打真实端点,
 * 通过环境变量传入(PI_CAPTURE_BASE_URL / PI_CAPTURE_API_KEY),不要写进文件。
 *
 * 输出为 JSON,可作为 transport 断言的出处依据。若要升级为受冻结的
 * oracle 夹具,请比照 scripts/generate-pi-native-oracle.mjs 补 provenance
 * (pinned commit、源码哈希、bundler 版本)。
 */

import { createServer } from "node:http";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const PI = process.env.PI_CHECKOUT;
const EXPECTED_PI_COMMIT = "ab366ebe94cacd419d986be454f12b1b9913aaca";
if (!PI) {
  console.error(
    "PI_CHECKOUT must point at a pinned Pi checkout (with node_modules).",
  );
  process.exit(2);
}
const piCommit = execFileSync("git", ["-C", PI, "rev-parse", "HEAD"], {
  encoding: "utf8",
}).trim();
if (piCommit !== EXPECTED_PI_COMMIT) {
  throw new Error(
    `Pi checkout pin mismatch: expected ${EXPECTED_PI_COMMIT}, got ${piCommit}`,
  );
}

const requireFromPi = createRequire(join(PI, "package.json"));
const { buildSync, version: esbuildVersion } = requireFromPi("esbuild");
const codingAgentPackagePath = join(PI, "packages/coding-agent/package.json");
const codingAgentPackageBytes = readFileSync(codingAgentPackagePath);
const codingAgentPackage = JSON.parse(codingAgentPackageBytes.toString("utf8"));
const distributionMetadata = {
  source: "packages/coding-agent/package.json",
  sha256: createHash("sha256").update(codingAgentPackageBytes).digest("hex"),
  name: codingAgentPackage.name,
  version: codingAgentPackage.version,
  bin: codingAgentPackage.bin,
  piConfig: codingAgentPackage.piConfig,
};

const ANTHROPIC_SSE =
  'event: message_start\ndata: {"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}\n\n' +
  'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}\n\n' +
  'event: message_stop\ndata: {"type":"message_stop"}\n\n';
const OPENAI_SSE =
  'data: {"type":"response.completed","response":{"id":"r","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}\n\n' +
  "data: [DONE]\n\n";

const captured = [];
const server = createServer((request, response) => {
  const chunks = [];
  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    captured.push({ url: request.url, headers: { ...request.headers } });
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.end(request.url.includes("messages") ? ANTHROPIC_SSE : OPENAI_SSE);
  });
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const baseUrl =
  process.env.PI_CAPTURE_BASE_URL ??
  `http://127.0.0.1:${server.address().port}`;

const harnessDirectory = mkdtempSync(join(PI, ".cc-switch-transport-capture-"));
process.on("exit", () =>
  rmSync(harnessDirectory, { recursive: true, force: true }),
);
const aiShimPath = join(harnessDirectory, "pi-ai-shim.mjs");
const compatShimPath = join(harnessDirectory, "pi-ai-compat-shim.mjs");
writeFileSync(
  aiShimPath,
  [
    'export function lazyStream() { throw new Error("transport shim must not execute during compat capture"); }',
    "let uuidSequence = 0;",
    'export function uuidv7() { uuidSequence += 1; return `00000000-0000-7000-8000-${String(uuidSequence).padStart(12, "0")}`; }',
    "export class EventStream {}",
    "export class ModelsError extends Error {}",
    "export function validateToolArguments() { return undefined; }",
    'export function contentText(value) { return typeof value === "string" ? value : ""; }',
    'export function retryAssistantCall() { throw new Error("resource capture must not call AI"); }',
    "export function parseStreamingJson() { return undefined; }",
    "export function modelsAreEqual(left, right) { return left === right; }",
    "export function createModels() { return {}; }",
    "export function getBuiltinModelDataGeneratedAt() { return undefined; }",
    "export function builtinProviders() { return []; }",
    "export function radiusProvider() { return undefined; }",
    "",
  ].join("\n"),
);
writeFileSync(
  compatShimPath,
  [
    'export function getApiProvider() { throw new Error("transport shim must not execute during compat capture"); }',
    "export function clampThinkingLevel(value) { return value; }",
    "export async function cleanupSessionResources() {}",
    "export function getSupportedThinkingLevels() { return []; }",
    "export function isContextOverflow() { return false; }",
    "export function isRetryableAssistantError() { return false; }",
    "export function modelsAreEqual(left, right) { return left === right; }",
    "export function resetApiProviders() {}",
    'export function streamSimple() { throw new Error("resource capture must not call AI"); }',
    'export function stream() { throw new Error("resource capture must not call AI"); }',
    'export function completeSimple() { throw new Error("resource capture must not call AI"); }',
    "",
  ].join("\n"),
);
const entryPoint = join(harnessDirectory, "entry.mjs");
writeFileSync(
  entryPoint,
  [
    `export { streamSimple as anthropicMessages } from "${PI}/packages/ai/src/api/anthropic-messages.ts";`,
    `export { streamSimple as openaiResponses } from "${PI}/packages/ai/src/api/openai-responses.ts";`,
    `export { streamSimple as openaiCompletions } from "${PI}/packages/ai/src/api/openai-completions.ts";`,
    `export { streamSimple as googleGenerativeAi } from "${PI}/packages/ai/src/api/google-generative-ai.ts";`,
    `export { composeModelProvider } from "${PI}/packages/coding-agent/src/core/provider-composer.ts";`,
    `export { resolveConfigValueOrThrow } from "${PI}/packages/coding-agent/src/core/resolve-config-value.ts";`,
    `export { loadSkills } from "${PI}/packages/coding-agent/src/core/skills.ts";`,
    `export { loadPromptTemplates, expandPromptTemplate } from "${PI}/packages/coding-agent/src/core/prompt-templates.ts";`,
    `export { SessionManager } from "${PI}/packages/coding-agent/src/core/session-manager.ts";`,
    `export { parseArgs } from "${PI}/packages/coding-agent/src/cli/args.ts";`,
    `export { BUILTIN_SLASH_COMMANDS } from "${PI}/packages/coding-agent/src/core/slash-commands.ts";`,
    `export { createAllToolDefinitions } from "${PI}/packages/coding-agent/src/core/tools/index.ts";`,
  ].join("\n"),
);
const bundlePath = join(harnessDirectory, "bundle.mjs");
buildSync({
  entryPoints: [entryPoint],
  bundle: true,
  platform: "node",
  format: "esm",
  outfile: bundlePath,
  external: ["node:*"],
  packages: "external",
  alias: {
    "@earendil-works/pi-ai": aiShimPath,
    "@earendil-works/pi-ai/compat": compatShimPath,
  },
  logLevel: "silent",
});
const adapters = await import(pathToFileURL(bundlePath).href);

// ResourceLoader pulls the complete coding-agent resource graph. Bundle the
// real pinned ResourceLoader separately with broad AI stubs; the captured
// instruction behavior remains real while unrelated generated model data is
// kept outside this resource-only probe.
const resourceEntryPoint = join(harnessDirectory, "resource-entry.mjs");
const resourceBundlePath = join(harnessDirectory, "resource-bundle.mjs");
writeFileSync(
  resourceEntryPoint,
  `export { DefaultResourceLoader } from "${PI}/packages/coding-agent/src/core/resource-loader.ts";\n`,
);
buildSync({
  entryPoints: [resourceEntryPoint],
  bundle: true,
  platform: "node",
  format: "esm",
  outfile: resourceBundlePath,
  external: ["node:*"],
  packages: "external",
  alias: {
    "@earendil-works/pi-ai/providers/all": aiShimPath,
    "@earendil-works/pi-ai/oauth": aiShimPath,
    "@earendil-works/pi-ai": aiShimPath,
    "@earendil-works/pi-ai/compat": compatShimPath,
  },
  logLevel: "silent",
});
const resourceAdapters = await import(pathToFileURL(resourceBundlePath).href);

const API_BY_ADAPTER = {
  anthropicMessages: "anthropic-messages",
  openaiResponses: "openai-responses",
  openaiCompletions: "openai-completions",
  googleGenerativeAi: "google-generative-ai",
};

/** 每个用例只改变凭证与显式 header,其余保持最小合法模型。 */
const CASES = [
  ["anthropicMessages", "plain-key", "sk-ant-api03-plain", {}],
  ["anthropicMessages", "oauth-token", "sk-ant-oat01-token", {}],
  [
    "anthropicMessages",
    "oauth-with-explicit-x-api-key",
    "sk-ant-oat01-token",
    { "x-api-key": "explicit-secret" },
  ],
  [
    "anthropicMessages",
    "oauth-with-explicit-authorization",
    "sk-ant-oat01-token",
    { authorization: "Bearer configured" },
  ],
  [
    "anthropicMessages",
    "explicit-x-api-key",
    "synthesized-secret",
    { "x-api-key": "explicit-secret" },
  ],
  [
    "anthropicMessages",
    "explicit-authorization",
    "synthesized-secret",
    { authorization: "Bearer configured" },
  ],
  ["openaiResponses", "plain-key", "sk-plain", {}],
  ["openaiResponses", "oauth-shaped-token", "sk-ant-oat01-not-anthropic", {}],
  [
    "openaiResponses",
    "explicit-authorization",
    "synthesized-secret",
    { authorization: "Bearer configured" },
  ],
  [
    "openaiCompletions",
    "explicit-authorization",
    "synthesized-secret",
    { authorization: "Bearer configured" },
  ],
  [
    "openaiCompletions",
    "explicit-x-api-key",
    "synthesized-secret",
    { "x-api-key": "explicit-secret" },
  ],
  ["googleGenerativeAi", "plain-key", "google-plain", {}],
  [
    "googleGenerativeAi",
    "explicit-x-goog-api-key",
    "google-synthesized",
    { "x-goog-api-key": "google-explicit" },
  ],
];

const results = [];
for (const [adapter, label, apiKey, headers] of CASES) {
  const model = {
    id: "m",
    name: "m",
    api: API_BY_ADAPTER[adapter],
    provider: "candidate",
    baseUrl,
    reasoning: false,
    input: ["text"],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: 1000,
    maxTokens: 100,
  };
  const before = captured.length;
  let error;
  try {
    const stream = adapters[adapter](
      model,
      { messages: [{ role: "user", content: "hi" }] },
      {
        apiKey: process.env.PI_CAPTURE_API_KEY ?? apiKey,
        headers,
        maxTokens: 16,
      },
    );
    for await (const _event of stream) {
      // drain
    }
  } catch (caught) {
    error = String(caught);
  }
  const request =
    captured.length > before ? captured[captured.length - 1] : undefined;
  results.push({
    adapter: API_BY_ADAPTER[adapter],
    case: label,
    requestSent: Boolean(request),
    requestUrl: request?.url,
    error: request ? undefined : error,
    authHeaders: request
      ? Object.fromEntries(
          Object.entries(request.headers).filter(([name]) =>
            [
              "authorization",
              "x-api-key",
              "x-goog-api-key",
              "anthropic-beta",
              "anthropic-version",
              "openai-beta",
            ].includes(name),
          ),
        )
      : undefined,
  });
}

const compatInput = {
  api: "openai-responses",
  baseUrl: "https://compat.example/v1",
  apiKey: "literal",
  compat: {
    openRouterRouting: ["first", "second"],
    chatTemplateKwargs: "ab",
    baseOnly: true,
  },
  models: [{ id: "m", compat: { supportsStore: true } }],
  modelOverrides: {
    m: {
      compat: {
        openRouterRouting: null,
        chatTemplateKwargs: { named: true },
        overlayOnly: true,
      },
    },
  },
};
const compatProvider = adapters.composeModelProvider(
  "compat-spread",
  undefined,
  {
    getProvider(providerId) {
      return providerId === "compat-spread" ? compatInput : undefined;
    },
  },
  undefined,
);
const compatSpread = compatProvider.getModels()[0].compat;

const minimalProvider = adapters.composeModelProvider(
  "minimal-provider",
  undefined,
  {
    getProvider(providerId) {
      return providerId === "minimal-provider"
        ? {
            name: "Minimal provider",
            api: "openai-responses",
            baseUrl: "https://minimal.example/v1",
            apiKey: "literal",
            models: [{ id: "minimal-model", name: "Minimal model" }],
          }
        : undefined;
    },
  },
  undefined,
);
const minimalModel = minimalProvider.getModels()[0];
const minimalProviderComposition = {
  id: minimalModel.id,
  name: minimalModel.name,
  provider: minimalModel.provider,
  api: minimalModel.api,
  baseUrl: minimalModel.baseUrl,
  reasoning: minimalModel.reasoning,
  input: minimalModel.input,
  cost: minimalModel.cost,
  contextWindow: minimalModel.contextWindow,
  maxTokens: minimalModel.maxTokens,
};

function jsonSafeJavaScriptValue(value) {
  if (typeof value === "string") {
    const codeUnits = Array.from({ length: value.length }, (_, index) =>
      value.charCodeAt(index),
    );
    const hasLoneSurrogate = codeUnits.some((unit, index) => {
      if (unit >= 0xd800 && unit <= 0xdbff) {
        return !(
          index + 1 < codeUnits.length &&
          codeUnits[index + 1] >= 0xdc00 &&
          codeUnits[index + 1] <= 0xdfff
        );
      }
      if (unit >= 0xdc00 && unit <= 0xdfff) {
        return !(
          index > 0 &&
          codeUnits[index - 1] >= 0xd800 &&
          codeUnits[index - 1] <= 0xdbff
        );
      }
      return false;
    });
    return hasLoneSurrogate
      ? {
          $javascriptStringUtf16: codeUnits.map((unit) =>
            unit.toString(16).padStart(4, "0"),
          ),
        }
      : value;
  }
  if (Array.isArray(value)) {
    return value.map(jsonSafeJavaScriptValue);
  }
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, child]) => [
        key,
        jsonSafeJavaScriptValue(child),
      ]),
    );
  }
  return value;
}

function captureCompatSpread(label, baseValue, overlayValue) {
  const providerId = `compat-${label}`;
  const provider = adapters.composeModelProvider(
    providerId,
    undefined,
    {
      getProvider(candidate) {
        if (candidate !== providerId) return undefined;
        return {
          api: "openai-responses",
          baseUrl: "https://compat.example/v1",
          apiKey: "literal",
          compat: { chatTemplateKwargs: baseValue },
          models: [{ id: "m" }],
          modelOverrides: {
            m: { compat: { chatTemplateKwargs: overlayValue } },
          },
        };
      },
    },
    undefined,
  );
  return {
    label,
    baseValue,
    overlayValue,
    result: jsonSafeJavaScriptValue(
      provider.getModels()[0].compat.chatTemplateKwargs,
    ),
  };
}

const compatEdgeCases = [
  captureCompatSpread("ascii-string-to-string", "ab", "cd"),
  captureCompatSpread("astral-string-to-object", "😀", { named: true }),
  captureCompatSpread("astral-string-fully-overridden", "😀", {
    0: "repaired-high",
    1: "repaired-low",
    named: true,
  }),
  captureCompatSpread("string-to-array", "ab", ["first", "second"]),
];

const resolverInputs = [
  "literal-secret",
  "cash$money",
  "café$literal",
  "$$literal-$!bang",
  "prefix-${PI_CAPTURE_MISSING}-suffix",
];
const resolverCases = resolverInputs.map((input) => {
  try {
    return {
      input,
      status: "success",
      result: adapters.resolveConfigValueOrThrow(
        input,
        "transport capture",
        {},
      ),
    };
  } catch (error) {
    return {
      input,
      status: "error",
      error: error instanceof Error ? error.message : String(error),
    };
  }
});

// Execute Pi's real discovery entry point. The directories are deliberately
// created in reverse order: a deterministic winner must come from Pi's
// discovery ordering, not filesystem insertion order.
const skillProbeRoot = join(harnessDirectory, "skill-discovery");
const skillAgentDir = join(skillProbeRoot, "agent");
const skillProjectDir = join(skillProbeRoot, "project");
for (const directory of [
  skillProjectDir,
  join(skillAgentDir, "skills", "b-second"),
  join(skillAgentDir, "skills", "a-first"),
]) {
  mkdirSync(directory, { recursive: true });
}
writeFileSync(
  join(skillAgentDir, "skills", "b-second", "SKILL.md"),
  "---\nname: duplicate\ndescription: second\n---\nsecond\n",
);
writeFileSync(
  join(skillAgentDir, "skills", "a-first", "SKILL.md"),
  "---\nname: duplicate\ndescription: first\n---\nfirst\n",
);
const skillDiscovery = jsonSafeJavaScriptValue(
  await adapters.loadSkills({
    cwd: skillProjectDir,
    agentDir: skillAgentDir,
    skillPaths: [],
    includeDefaults: true,
  }),
);

const promptAgentDir = join(harnessDirectory, "prompt-agent");
const promptProjectDir = join(harnessDirectory, "prompt-project");
mkdirSync(join(promptAgentDir, "prompts", "nested"), { recursive: true });
mkdirSync(promptProjectDir, { recursive: true });
writeFileSync(
  join(promptAgentDir, "prompts", "review.md"),
  "---\ndescription: Review captured changes\nargument-hint: <range>\n---\nReview $1\n",
);
writeFileSync(
  join(promptAgentDir, "prompts", "release notes.md"),
  "This spaced filename cannot be addressed as one slash-command token.\n",
);
writeFileSync(join(promptAgentDir, "prompts", "release.v2.md"), "Release $1\n");
writeFileSync(join(promptAgentDir, "prompts", "评审.md"), "评审 $1\n");
writeFileSync(join(promptAgentDir, "prompts", "empty.md"), "");
writeFileSync(
  join(promptAgentDir, "prompts", "nested", "ignored.md"),
  "nested",
);
const loadedPromptTemplates = adapters.loadPromptTemplates({
  cwd: promptProjectDir,
  agentDir: promptAgentDir,
  promptPaths: [],
  includeDefaults: true,
});
const promptTemplateDiscovery = jsonSafeJavaScriptValue(
  loadedPromptTemplates.map((template) => ({
    name: template.name,
    description: template.description,
    argumentHint: template.argumentHint,
    content: template.content,
    source: template.sourceInfo?.source,
    scope: template.sourceInfo?.scope,
    relativeFile:
      template.filePath ===
      join(promptAgentDir, "prompts", `${template.name}.md`)
        ? `prompts/${template.name}.md`
        : template.filePath,
  })),
);
const promptTemplateExpansion = [
  "/review captured-range",
  "/release notes",
  "/release.v2 captured-range",
  "/评审 变更",
].map((input) => ({
  input,
  result: adapters.expandPromptTemplate(input, loadedPromptTemplates),
}));

// File presence, including a zero-byte file, is the native activation state
// for Pi's global instruction resources. Execute the real resource loader so
// cc-switch does not infer that rule from a parser implementation.
const instructionAgentDir = join(harnessDirectory, "instruction-agent");
const instructionProjectDir = join(harnessDirectory, "instruction-project");
mkdirSync(instructionAgentDir, { recursive: true });
mkdirSync(instructionProjectDir, { recursive: true });
for (const filename of ["AGENTS.md", "SYSTEM.md", "APPEND_SYSTEM.md"]) {
  writeFileSync(join(instructionAgentDir, filename), "");
}
const instructionLoader = new resourceAdapters.DefaultResourceLoader({
  cwd: instructionProjectDir,
  agentDir: instructionAgentDir,
  noExtensions: true,
  noSkills: true,
  noPromptTemplates: true,
  noThemes: true,
});
await instructionLoader.reload();
const emptyInstructionFiles = {
  agentsFiles: instructionLoader.getAgentsFiles().agentsFiles.map((entry) => ({
    relativeFile: entry.path.startsWith(instructionAgentDir)
      ? entry.path.slice(instructionAgentDir.length + 1)
      : entry.path,
    content: entry.content,
  })),
  systemPrompt: instructionLoader.getSystemPrompt(),
  systemPromptSource: instructionLoader.getSystemPromptSource()?.path,
  appendSystemPrompt: instructionLoader.getAppendSystemPrompt(),
  appendSystemPromptSources: instructionLoader
    .getAppendSystemPromptSources()
    .map((entry) => entry.path),
};

// Exercise Pi's real SessionManager instead of inferring sessionDir or JSONL
// shape from its TypeScript source. Relative sessionDir is resolved against the
// launching process cwd, while the header keeps the explicit project cwd.
const sessionProjectDir = join(harnessDirectory, "session project");
mkdirSync(sessionProjectDir, { recursive: true });
const originalCwd = process.cwd();
process.chdir(sessionProjectDir);
const capturedSession = adapters.SessionManager.create(
  sessionProjectDir,
  ".pi/sessions",
  { id: "cc-switch-capture-session" },
);
capturedSession.appendSessionInfo("Captured session");
capturedSession.appendMessage({
  role: "user",
  content: [{ type: "text", text: "captured question" }],
  timestamp: 1_700_000_000_000,
});
capturedSession.appendMessage({
  role: "assistant",
  content: [{ type: "text", text: "captured answer" }],
  api: "openai-responses",
  provider: "capture",
  model: "capture-model",
  usage: {
    input: 1,
    output: 1,
    cacheRead: 0,
    cacheWrite: 0,
    totalTokens: 2,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
  },
  stopReason: "stop",
  timestamp: 1_700_000_001_000,
});
const capturedSessionFile = capturedSession.getSessionFile();
if (!capturedSessionFile) {
  throw new Error("pinned SessionManager did not persist the capture session");
}
const capturedSessionPath = resolve(capturedSessionFile);
const parsedSessionArgs = adapters.parseArgs([
  "--session",
  capturedSessionPath,
]);
const parsedVersionArgs = adapters.parseArgs(["--version"]);
const sessionCliSemantics = {
  argv: ["--session", capturedSessionPath],
  parsedSession: parsedSessionArgs.session,
  diagnostics: parsedSessionArgs.diagnostics,
  versionArgv: ["--version"],
  parsedVersion: parsedVersionArgs.version,
  versionDiagnostics: parsedVersionArgs.diagnostics,
};
const capturedSessionLines = capturedSessionFile
  ? readFileSync(capturedSessionFile, "utf8")
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line))
  : [];
const sessionDirectorySemantics = {
  processCwd: sessionProjectDir,
  suppliedProjectCwd: sessionProjectDir,
  suppliedSessionDir: ".pi/sessions",
  resolvedSessionDir: capturedSession.getSessionDir(),
  headerKeys: Object.keys(capturedSession.getHeader() ?? {}).sort(),
  entryShapes: capturedSession.getEntries().map((entry) => ({
    type: entry.type,
    keys: Object.keys(entry).sort(),
    messageRole: entry.type === "message" ? entry.message.role : undefined,
    messageKeys:
      entry.type === "message" ? Object.keys(entry.message).sort() : undefined,
  })),
  persistedLineTypes: capturedSessionLines.map((entry) => entry.type),
  listAllCount: (
    await adapters.SessionManager.listAll(capturedSession.getSessionDir())
  ).length,
};

const branchedSession = adapters.SessionManager.create(
  sessionProjectDir,
  ".pi/sessions",
  { id: "cc-switch-capture-branch" },
);
const branchRootId = branchedSession.appendMessage({
  role: "user",
  content: [{ type: "text", text: "branch root" }],
  timestamp: 1_700_000_002_000,
});
branchedSession.appendSessionInfo("Abandoned branch name");
branchedSession.appendMessage({
  role: "assistant",
  content: [{ type: "text", text: "abandoned answer" }],
  api: "openai-responses",
  provider: "capture",
  model: "capture-model",
  usage: {
    input: 1,
    output: 1,
    cacheRead: 0,
    cacheWrite: 0,
    totalTokens: 2,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
  },
  stopReason: "stop",
  timestamp: 1_700_000_003_000,
});
branchedSession.branch(branchRootId);
branchedSession.appendMessage({
  role: "user",
  content: [{ type: "text", text: "active branch" }],
  timestamp: 1_700_000_004_000,
});
const sessionBranchSemantics = {
  sessionName: branchedSession.getSessionName(),
  activeEntryTypes: branchedSession.getBranch().map((entry) => entry.type),
};

const malformedSessionFile = join(
  capturedSession.getSessionDir(),
  "cc-switch-capture-malformed.jsonl",
);
writeFileSync(
  malformedSessionFile,
  [
    JSON.stringify(capturedSession.getHeader()),
    "{not valid json",
    ...capturedSession.getEntries().map((entry) => JSON.stringify(entry)),
    "",
  ].join("\n"),
);
let malformedSessionSemantics;
try {
  const malformedSession = adapters.SessionManager.open(malformedSessionFile);
  malformedSessionSemantics = {
    status: "accepted",
    entryTypes: malformedSession.getEntries().map((entry) => entry.type),
  };
} catch (error) {
  malformedSessionSemantics = {
    status: "rejected",
    error: error instanceof Error ? error.message : String(error),
  };
}
process.chdir(originalCwd);

// The executed built-in tool factory is the authoritative core inventory.
// Extension-provided tools remain possible, but Pi exposes no native MCP
// registry for cc-switch to mirror as an app toggle.
const nativeToolInventory = Object.values(
  adapters.createAllToolDefinitions(sessionProjectDir),
)
  .map((tool) => tool.name)
  .sort();
const nativeSlashCommandInventory = adapters.BUILTIN_SLASH_COMMANDS.map(
  ({ name, description, argumentHint }) => ({
    name,
    description,
    ...(argumentHint ? { argumentHint } : {}),
  }),
);

server.close();
console.log(
  JSON.stringify(
    {
      bundler: `esbuild@${esbuildVersion}`,
      piCheckout: PI,
      piCommit,
      distributionMetadata,
      baseUrl,
      results,
      minimalProviderComposition,
      compatSpread,
      compatEdgeCases,
      resolverCases,
      skillDiscovery,
      promptTemplateDiscovery,
      promptTemplateExpansion,
      emptyInstructionFiles,
      sessionDirectorySemantics,
      sessionCliSemantics,
      sessionBranchSemantics,
      malformedSessionSemantics,
      nativeToolInventory,
      nativeSlashCommandInventory,
    },
    null,
    2,
  ),
);
