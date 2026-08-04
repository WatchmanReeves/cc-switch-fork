import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronRight, Plus, Trash2 } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent } from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import type { ProviderFormProps, ProviderFormValues } from "./ProviderForm";
import EndpointSpeedTest from "./EndpointSpeedTest";
import { ProviderPresetSelector } from "./ProviderPresetSelector";
import { ApiKeySection } from "./shared";
import {
  piProviderPresets,
  type PiProviderPreset,
} from "@/config/piProviderPresets";
import type {
  CustomEndpoint,
  EndpointCandidate,
  ProviderCategory,
  ProviderMeta,
} from "@/types";

const PI_API_EXAMPLES = [
  "anthropic-messages",
  "openai-responses",
  "openai-completions",
  "google-generative-ai",
] as const;

const ROOT_CONTROLLED_KEYS = new Set([
  "name",
  "baseUrl",
  "api",
  "apiKey",
  "headers",
  "authHeader",
  "models",
]);
const MODEL_CONTROLLED_KEYS = new Set(["id", "name", "baseUrl", "api"]);

interface PiModelDraft {
  key: string;
  id: string;
  name: string;
  api: string;
  baseUrl: string;
  additionalJson: string;
}

function objectWithout(
  value: Record<string, unknown>,
  denied: Set<string>,
): Record<string, unknown> {
  return Object.fromEntries(
    Object.entries(value).filter(([key]) => !denied.has(key)),
  );
}

function asObject(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function optionalText(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function containsNonFiniteNumber(value: unknown): boolean {
  if (typeof value === "number") return !Number.isFinite(value);
  if (Array.isArray(value)) return value.some(containsNonFiniteNumber);
  if (value && typeof value === "object") {
    return Object.values(value).some(containsNonFiniteNumber);
  }
  return false;
}

function parseObject(
  text: string,
  objectError: string,
  numberError: string,
): Record<string, unknown> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text || "{}") as unknown;
  } catch {
    throw new Error(objectError);
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error(objectError);
  }
  if (containsNonFiniteNumber(parsed)) {
    throw new Error(numberError);
  }
  return parsed as Record<string, unknown>;
}

function validateAbsoluteHttpUrl(value: string, errorMessage: string): void {
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    throw new Error(errorMessage);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error(errorMessage);
  }
}

function modelDraft(value: unknown): PiModelDraft {
  const model = asObject(value);
  return {
    key: crypto.randomUUID(),
    id: optionalText(model.id),
    name: optionalText(model.name),
    api: optionalText(model.api),
    baseUrl: optionalText(model.baseUrl),
    additionalJson: JSON.stringify(
      objectWithout(model, MODEL_CONTROLLED_KEYS),
      null,
      2,
    ),
  };
}

function newModel(): PiModelDraft {
  return {
    key: crypto.randomUUID(),
    id: "",
    name: "",
    api: "",
    baseUrl: "",
    // Pinned Pi accepts an id-only model override and supplies its own
    // composition defaults. Do not invent model capability or pricing values.
    additionalJson: "{}",
  };
}

export function PiProviderForm({
  providerId,
  submitLabel,
  onSubmit,
  onCancel,
  onSubmittingChange,
  initialData,
  showButtons = true,
}: ProviderFormProps) {
  const { t } = useTranslation();
  const initialConfig = useMemo(
    () => asObject(initialData?.settingsConfig),
    [initialData?.settingsConfig],
  );
  const isEdit = Boolean(initialData);
  const [selectedPresetId, setSelectedPresetId] = useState<string | null>(null);
  const [selectedPreset, setSelectedPreset] = useState<PiProviderPreset | null>(
    null,
  );
  const [category, setCategory] = useState<ProviderCategory>(
    initialData?.category ?? "custom",
  );
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [providerKey, setProviderKey] = useState(providerId ?? "");
  const [name, setName] = useState(
    initialData?.name ?? optionalText(initialConfig.name),
  );
  const [websiteUrl, setWebsiteUrl] = useState(initialData?.websiteUrl ?? "");
  const [notes, setNotes] = useState(initialData?.notes ?? "");
  const [baseUrl, setBaseUrl] = useState(optionalText(initialConfig.baseUrl));
  const [api, setApi] = useState(optionalText(initialConfig.api));
  const [apiKey, setApiKey] = useState(optionalText(initialConfig.apiKey));
  const [authHeader, setAuthHeader] = useState(
    typeof initialConfig.authHeader === "boolean"
      ? initialConfig.authHeader
      : false,
  );
  const [authHeaderExplicit, setAuthHeaderExplicit] = useState(
    typeof initialConfig.authHeader === "boolean",
  );
  const [headersJson, setHeadersJson] = useState(
    JSON.stringify(asObject(initialConfig.headers), null, 2),
  );
  const [additionalJson, setAdditionalJson] = useState(
    JSON.stringify(objectWithout(initialConfig, ROOT_CONTROLLED_KEYS), null, 2),
  );
  const [isEndpointModalOpen, setIsEndpointModalOpen] = useState(false);
  const [endpointAutoSelect, setEndpointAutoSelect] = useState(
    initialData?.meta?.endpointAutoSelect ?? true,
  );
  const [draftCustomEndpoints, setDraftCustomEndpoints] = useState<string[]>(
    () => Object.keys(initialData?.meta?.custom_endpoints ?? {}),
  );
  const [models, setModels] = useState<PiModelDraft[]>(() => {
    const configured = Array.isArray(initialConfig.models)
      ? initialConfig.models
      : [];
    return configured.length > 0 ? configured.map(modelDraft) : [newModel()];
  });

  const presetEntries = useMemo(
    () =>
      piProviderPresets.map((preset, index) => ({
        id: `pi-${index}`,
        preset,
      })),
    [],
  );

  const selectPreset = (id: string) => {
    setSelectedPresetId(id);
    if (id === "custom") {
      setSelectedPreset(null);
      setCategory("custom");
      setAdvancedOpen(true);
      return;
    }
    const entry = presetEntries.find((candidate) => candidate.id === id);
    if (!entry) return;
    const preset = entry.preset;
    setAdvancedOpen(false);
    setSelectedPreset(preset);
    setCategory(preset.category ?? "custom");
    setProviderKey(preset.providerKey);
    setName(preset.settingsConfig.name);
    setWebsiteUrl(preset.websiteUrl);
    setBaseUrl(preset.settingsConfig.baseUrl);
    setApi(preset.settingsConfig.api);
    setApiKey("");
    setAuthHeader(false);
    setAuthHeaderExplicit(false);
    setHeadersJson("{}");
    setAdditionalJson("{}");
    setDraftCustomEndpoints([]);
    setModels(preset.settingsConfig.models.map((model) => modelDraft(model)));
  };

  const updateModel = (
    key: string,
    update: Partial<Omit<PiModelDraft, "key">>,
  ) => {
    setModels((current) =>
      current.map((model) =>
        model.key === key ? { ...model, ...update } : model,
      ),
    );
  };

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    onSubmittingChange?.(true);
    try {
      const trimmedName = name.trim();
      const trimmedKey = providerKey.trim();
      if (!trimmedName) throw new Error(t("pi.form.nameRequired"));
      if (!isEdit && !trimmedKey) {
        throw new Error(t("pi.form.providerKeyRequired"));
      }
      if (selectedPreset && apiKey.length === 0) {
        throw new Error(t("pi.form.credentialRequired"));
      }
      if (models.length === 0) throw new Error(t("pi.form.modelRequired"));

      const headersLabel = t("pi.form.headers");
      const headers = parseObject(
        headersJson,
        t("pi.form.jsonObjectRequired", { label: headersLabel }),
        t("pi.form.nonFiniteNumber", { label: headersLabel }),
      );
      if (Object.values(headers).some((value) => typeof value !== "string")) {
        throw new Error(t("pi.form.headersStringValues"));
      }
      const providerAdditionalLabel = t("pi.form.additionalConfig");
      const rootAdditional = parseObject(
        additionalJson,
        t("pi.form.jsonObjectRequired", {
          label: providerAdditionalLabel,
        }),
        t("pi.form.nonFiniteNumber", { label: providerAdditionalLabel }),
      );
      const seen = new Set<string>();
      const normalizedModels = models.map((model, index) => {
        // Pinned Pi treats model IDs as opaque, exact strings. In particular,
        // its schema accepts whitespace-only and edge-whitespace IDs; trimming
        // here would silently rename an imported model.
        const id = model.id;
        const modelApi = model.api.trim();
        const modelBaseUrl = model.baseUrl.trim();
        if (id.length === 0) {
          throw new Error(t("pi.form.modelIdRequired", { index: index + 1 }));
        }
        if (seen.has(id)) {
          throw new Error(t("pi.form.duplicateModel", { id }));
        }
        seen.add(id);
        if (!modelApi && !api.trim()) {
          throw new Error(t("pi.form.effectiveApiRequired", { id }));
        }
        const effectiveUrl = modelBaseUrl || baseUrl.trim();
        if (!effectiveUrl) {
          throw new Error(t("pi.form.effectiveBaseUrlRequired", { id }));
        }
        validateAbsoluteHttpUrl(
          effectiveUrl,
          t("pi.form.absoluteHttpUrlRequired", {
            label: t("pi.form.modelBaseUrlFor", { id }),
          }),
        );
        const modelAdditionalLabel = t("pi.form.modelAdditionalConfig", {
          id,
        });
        const additional = parseObject(
          model.additionalJson,
          t("pi.form.jsonObjectRequired", { label: modelAdditionalLabel }),
          t("pi.form.nonFiniteNumber", { label: modelAdditionalLabel }),
        );
        return {
          ...additional,
          id,
          ...(model.name.trim() ? { name: model.name.trim() } : {}),
          ...(modelApi ? { api: modelApi } : {}),
          ...(modelBaseUrl ? { baseUrl: modelBaseUrl } : {}),
        };
      });
      if (baseUrl.trim()) {
        validateAbsoluteHttpUrl(
          baseUrl.trim(),
          t("pi.form.absoluteHttpUrlRequired", {
            label: t("pi.form.providerBaseUrl"),
          }),
        );
      }

      const settingsConfig: Record<string, unknown> = {
        ...rootAdditional,
        name: trimmedName,
        ...(baseUrl.trim() ? { baseUrl: baseUrl.trim() } : {}),
        ...(api.trim() ? { api: api.trim() } : {}),
        ...(apiKey ? { apiKey } : {}),
        ...(Object.keys(headers).length > 0 ? { headers } : {}),
        ...(authHeaderExplicit ? { authHeader } : {}),
        models: normalizedModels,
      };
      const meta: ProviderMeta = {
        ...(initialData?.meta ?? {}),
        endpointAutoSelect,
      };
      // Existing-provider endpoint membership is owned by the dedicated
      // add/remove commands in EndpointSpeedTest. Provider update DTOs reject
      // hydrated endpoint snapshots by design.
      delete meta.custom_endpoints;
      if (!isEdit && draftCustomEndpoints.length > 0) {
        const now = Date.now();
        meta.custom_endpoints = Object.fromEntries(
          draftCustomEndpoints.map((url) => [
            url,
            {
              url,
              addedAt: now,
              lastUsed: undefined,
            } satisfies CustomEndpoint,
          ]),
        );
      }
      const values: ProviderFormValues = {
        name: trimmedName,
        websiteUrl: websiteUrl.trim(),
        notes: notes.trim(),
        settingsConfig: JSON.stringify(settingsConfig),
        icon: initialData?.icon ?? selectedPreset?.icon ?? "pi",
        iconColor: initialData?.iconColor ?? selectedPreset?.iconColor ?? "",
        providerKey: isEdit ? providerId : trimmedKey,
        presetId: selectedPresetId ?? undefined,
        presetCategory: category,
        meta,
      };
      await onSubmit(values);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      onSubmittingChange?.(false);
    }
  };

  const endpointCandidates = useMemo<EndpointCandidate[]>(() => {
    const candidates: EndpointCandidate[] = [];
    if (baseUrl.trim()) {
      candidates.push({ url: baseUrl.trim(), isCustom: false });
    }
    for (const url of draftCustomEndpoints) {
      if (url !== baseUrl.trim()) {
        candidates.push({ url, isCustom: true });
      }
    }
    return candidates;
  }, [baseUrl, draftCustomEndpoints]);
  const presetCategoryLabels = useMemo<Record<string, string>>(
    () => ({
      official: t("providerForm.categoryOfficial"),
      cn_official: t("providerForm.categoryCnOfficial"),
      aggregator: t("providerForm.categoryAggregation"),
      third_party: t("providerForm.categoryThirdParty"),
      custom: t("providerPreset.custom"),
    }),
    [t],
  );

  return (
    <form id="provider-form" onSubmit={submit} className="space-y-6">
      {!isEdit && (
        <section className="space-y-2">
          <h3 className="text-sm font-medium">{t("pi.form.stepPreset")}</h3>
          <ProviderPresetSelector
            selectedPresetId={selectedPresetId}
            presetEntries={presetEntries}
            presetCategoryLabels={presetCategoryLabels}
            onPresetChange={selectPreset}
            category={category}
            categoryHint={t("pi.form.presetHint")}
          />
        </section>
      )}

      <section className="space-y-3">
        <div>
          <h3 className="text-sm font-medium">{t("pi.form.stepAuth")}</h3>
          <p className="text-xs text-muted-foreground">
            {t("pi.form.managedCredentialHint")}
          </p>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label={t("pi.form.displayName")}>
            <Input
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="My Pi provider"
            />
          </Field>
          <div>
            <ApiKeySection
              label={t("pi.form.credential")}
              value={apiKey}
              onChange={setApiKey}
              category={category}
              shouldShowLink={Boolean(selectedPreset?.apiKeyUrl)}
              websiteUrl={selectedPreset?.apiKeyUrl ?? ""}
              isPartner={selectedPreset?.isPartner}
              partnerPromotionKey={selectedPreset?.partnerPromotionKey}
              placeholder={{
                official: "literal, $ENV, or !command",
                thirdParty: "literal, $ENV, or !command",
              }}
            />
            <p className="text-xs text-muted-foreground">
              {t("pi.form.credentialHint")}
            </p>
          </div>
        </div>
        <p className="rounded-md border border-dashed px-3 py-2 text-xs text-muted-foreground">
          {t("pi.form.nativeLoginAlternative")}
        </p>
      </section>

      <section className="space-y-3">
        <div className="flex items-center justify-between">
          <div>
            <h3 className="text-sm font-medium">{t("pi.form.stepModel")}</h3>
            <p className="text-xs text-muted-foreground">
              {t("pi.form.modelsHint")}
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={() => setAdvancedOpen((current) => !current)}
              className="gap-1"
            >
              {advancedOpen ? (
                <ChevronDown className="h-4 w-4" />
              ) : (
                <ChevronRight className="h-4 w-4" />
              )}
              {t("pi.form.advanced")}
            </Button>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => setModels((current) => [...current, newModel()])}
            >
              <Plus className="mr-1 h-4 w-4" />
              {t("pi.form.addModel")}
            </Button>
          </div>
        </div>
        {models.map((model, index) => (
          <div
            key={model.key}
            className="space-y-3 rounded-lg border bg-muted/20 p-4"
          >
            <div className="flex items-center justify-between">
              <h4 className="text-sm font-medium">
                {t("pi.form.modelNumber", { index: index + 1 })}
              </h4>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() =>
                  setModels((current) =>
                    current.filter((item) => item.key !== model.key),
                  )
                }
                aria-label={t("pi.form.removeModel")}
              >
                <Trash2 className="h-4 w-4 text-destructive" />
              </Button>
            </div>
            <div className="grid gap-3 sm:grid-cols-2">
              <Field label={t("pi.form.modelId")}>
                <Input
                  value={model.id}
                  onChange={(event) =>
                    updateModel(model.key, { id: event.target.value })
                  }
                  placeholder="model-id"
                />
              </Field>
              <Field label={t("pi.form.modelName")}>
                <Input
                  value={model.name}
                  onChange={(event) =>
                    updateModel(model.key, { name: event.target.value })
                  }
                  placeholder="Display name"
                />
              </Field>
            </div>
            <div className={advancedOpen ? "space-y-3" : "hidden"}>
              <div className="grid gap-3 sm:grid-cols-2">
                <Field label={t("pi.form.modelApi")}>
                  <Input
                    value={model.api}
                    onChange={(event) =>
                      updateModel(model.key, { api: event.target.value })
                    }
                    list="pi-api-examples"
                    placeholder={t("pi.form.inherit")}
                  />
                </Field>
                <Field label={t("pi.form.modelBaseUrl")}>
                  <Input
                    value={model.baseUrl}
                    onChange={(event) =>
                      updateModel(model.key, {
                        baseUrl: event.target.value,
                      })
                    }
                    placeholder={t("pi.form.inherit")}
                  />
                </Field>
              </div>
              <Field
                label={t("pi.form.modelAdditionalConfig", {
                  id: model.id,
                })}
              >
                <Textarea
                  value={model.additionalJson}
                  onChange={(event) =>
                    updateModel(model.key, {
                      additionalJson: event.target.value,
                    })
                  }
                  className="min-h-32 font-mono text-xs"
                  spellCheck={false}
                />
              </Field>
            </div>
          </div>
        ))}
      </section>

      <Collapsible open={advancedOpen} onOpenChange={setAdvancedOpen}>
        <CollapsibleContent
          forceMount
          className="space-y-4 pt-2 data-[state=closed]:hidden"
        >
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label={t("pi.form.providerKey")}>
              <Input
                value={providerKey}
                onChange={(event) => setProviderKey(event.target.value)}
                disabled={isEdit}
                placeholder="my-provider"
                autoComplete="off"
              />
              <p className="text-xs text-muted-foreground">
                {t("pi.form.providerKeyHint")}
              </p>
            </Field>
            <Field label={t("pi.form.providerApi")}>
              <Input
                value={api}
                onChange={(event) => setApi(event.target.value)}
                list="pi-api-examples"
                placeholder="openai-responses"
              />
              <datalist id="pi-api-examples">
                {PI_API_EXAMPLES.map((value) => (
                  <option key={value} value={value} />
                ))}
              </datalist>
              <p className="text-xs text-muted-foreground">
                {t("pi.form.inheritanceHint")}
              </p>
            </Field>
            <Field label={t("pi.form.providerBaseUrl")}>
              <Input
                value={baseUrl}
                onChange={(event) => setBaseUrl(event.target.value)}
                placeholder="https://api.example.com/v1"
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => setIsEndpointModalOpen(true)}
              >
                {t("pi.form.manageEndpoints")}
              </Button>
            </Field>
            <Field label={t("pi.form.website")}>
              <Input
                value={websiteUrl}
                onChange={(event) => setWebsiteUrl(event.target.value)}
                placeholder="https://example.com"
              />
            </Field>
          </div>
          <label className="flex items-start gap-2 rounded-md border p-3 text-sm">
            <input
              type="checkbox"
              checked={authHeader}
              onChange={(event) => {
                setAuthHeader(event.target.checked);
                setAuthHeaderExplicit(true);
              }}
              className="mt-0.5"
            />
            <span>
              <span className="font-medium">{t("pi.form.authHeader")}</span>
              <span className="block text-xs text-muted-foreground">
                {t("pi.form.authHeaderHint")}
              </span>
            </span>
          </label>
          <Field label={t("pi.form.headers")}>
            <Textarea
              value={headersJson}
              onChange={(event) => setHeadersJson(event.target.value)}
              className="min-h-24 font-mono text-xs"
              spellCheck={false}
            />
          </Field>
          <Field label={t("pi.form.additionalConfig")}>
            <Textarea
              value={additionalJson}
              onChange={(event) => setAdditionalJson(event.target.value)}
              className="min-h-28 font-mono text-xs"
              spellCheck={false}
            />
            <p className="text-xs text-muted-foreground">
              {t("pi.form.additionalConfigHint")}
            </p>
          </Field>
          <Field label={t("provider.notes")}>
            <Textarea
              value={notes}
              onChange={(event) => setNotes(event.target.value)}
              className="min-h-20"
            />
          </Field>
        </CollapsibleContent>
      </Collapsible>

      {showButtons && (
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" onClick={onCancel}>
            {t("common.cancel")}
          </Button>
          <Button type="submit">{submitLabel}</Button>
        </div>
      )}
      {isEndpointModalOpen && (
        <EndpointSpeedTest
          appId="pi"
          providerId={providerId}
          value={baseUrl}
          onChange={setBaseUrl}
          initialEndpoints={endpointCandidates}
          onClose={() => setIsEndpointModalOpen(false)}
          autoSelect={endpointAutoSelect}
          onAutoSelectChange={setEndpointAutoSelect}
          onCustomEndpointsChange={setDraftCustomEndpoints}
          persistenceMode={isEdit ? "immediate" : "batched"}
        />
      )}
    </form>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <Label>{label}</Label>
      {children}
    </div>
  );
}
