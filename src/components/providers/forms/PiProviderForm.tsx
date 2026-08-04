import { useCallback, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ChevronDown,
  ChevronRight,
  Download,
  Loader2,
  Plus,
  Trash2,
} from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Form } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import type { ProviderFormProps, ProviderFormValues } from "./ProviderForm";
import { BasicFormFields } from "./BasicFormFields";
import EndpointSpeedTest from "./EndpointSpeedTest";
import { ProviderPresetSelector } from "./ProviderPresetSelector";
import { ApiKeySection, EndpointField, ModelDropdown } from "./shared";
import {
  piProviderPresets,
  type PiProviderPreset,
} from "@/config/piProviderPresets";
import {
  fetchModelsForConfig,
  showFetchModelsError,
  type FetchedModel,
} from "@/lib/api/model-fetch";
import { providerSchema, type ProviderFormData } from "@/lib/schemas/provider";
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
  const [fetchedModels, setFetchedModels] = useState<FetchedModel[]>([]);
  const [isFetchingModels, setIsFetchingModels] = useState(false);
  const [models, setModels] = useState<PiModelDraft[]>(() => {
    const configured = Array.isArray(initialConfig.models)
      ? initialConfig.models
      : [];
    return configured.length > 0 ? configured.map(modelDraft) : [newModel()];
  });
  const [selectedModelKey, setSelectedModelKey] = useState(
    () => models[0]?.key ?? "",
  );
  const identityDefaults = useMemo<ProviderFormData>(
    () => ({
      name: initialData?.name ?? optionalText(initialConfig.name),
      websiteUrl: initialData?.websiteUrl ?? "",
      notes: initialData?.notes ?? "",
      settingsConfig: "{}",
      icon: initialData?.icon ?? "",
      iconColor: initialData?.iconColor ?? "",
    }),
    [initialConfig, initialData],
  );
  const form = useForm<ProviderFormData>({
    resolver: zodResolver(providerSchema),
    defaultValues: identityDefaults,
    mode: "onSubmit",
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
      setProviderKey("");
      form.reset(identityDefaults);
      setBaseUrl("");
      setApi("");
      setApiKey("");
      setAuthHeader(false);
      setAuthHeaderExplicit(false);
      setHeadersJson("{}");
      setAdditionalJson("{}");
      setDraftCustomEndpoints([]);
      setFetchedModels([]);
      const model = newModel();
      setModels([model]);
      setSelectedModelKey(model.key);
      return;
    }
    const entry = presetEntries.find((candidate) => candidate.id === id);
    if (!entry) return;
    const preset = entry.preset;
    setAdvancedOpen(false);
    setSelectedPreset(preset);
    setCategory(preset.category ?? "custom");
    setProviderKey(preset.providerKey);
    form.reset({
      name: preset.settingsConfig.name,
      websiteUrl: preset.websiteUrl,
      notes: "",
      settingsConfig: "{}",
      icon: preset.icon ?? "",
      iconColor: preset.iconColor ?? "",
    });
    setBaseUrl(preset.settingsConfig.baseUrl);
    setApi(preset.settingsConfig.api);
    setApiKey("");
    setAuthHeader(false);
    setAuthHeaderExplicit(false);
    setHeadersJson("{}");
    setAdditionalJson("{}");
    setDraftCustomEndpoints([]);
    setFetchedModels([]);
    const nextModels = preset.settingsConfig.models.map((model) =>
      modelDraft(model),
    );
    setModels(nextModels);
    setSelectedModelKey(nextModels[0]?.key ?? "");
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

  const addModel = () => {
    const model = newModel();
    setModels((current) => [...current, model]);
    if (!selectedModelKey) {
      setSelectedModelKey(model.key);
    }
  };

  const removeModel = (key: string) => {
    const nextModels = models.filter((model) => model.key !== key);
    setModels(nextModels);
    if (selectedModelKey === key) {
      setSelectedModelKey(nextModels[0]?.key ?? "");
    }
  };

  const handleFetchModels = useCallback(() => {
    const endpoint = baseUrl.trim();
    if (!endpoint || !apiKey) {
      showFetchModelsError(null, t, {
        hasApiKey: Boolean(apiKey),
        hasBaseUrl: Boolean(endpoint),
      });
      return;
    }

    setIsFetchingModels(true);
    fetchModelsForConfig(endpoint, apiKey)
      .then((result) => {
        setFetchedModels(result);
        if (result.length === 0) {
          toast.info(t("providerForm.fetchModelsEmpty"));
        } else {
          toast.success(
            t("providerForm.fetchModelsSuccess", { count: result.length }),
          );
        }
      })
      .catch((error) => {
        console.warn("[ModelFetch] Failed:", error);
        showFetchModelsError(error, t);
      })
      .finally(() => setIsFetchingModels(false));
  }, [apiKey, baseUrl, t]);

  const submit = async (identity: ProviderFormData) => {
    onSubmittingChange?.(true);
    try {
      const trimmedName = identity.name.trim();
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
      // Pi and the existing switch command treat the first model as the
      // provider default. Keep that wire format stable while exposing an
      // explicit model choice in the form.
      const orderedModels = selectedModelKey
        ? [
            ...models.filter((model) => model.key === selectedModelKey),
            ...models.filter((model) => model.key !== selectedModelKey),
          ]
        : models;
      const seen = new Set<string>();
      const normalizedModels = orderedModels.map((model, index) => {
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
        websiteUrl: identity.websiteUrl?.trim() ?? "",
        notes: identity.notes?.trim() ?? "",
        settingsConfig: JSON.stringify(settingsConfig),
        icon: identity.icon || selectedPreset?.icon || "pi",
        iconColor: identity.iconColor || selectedPreset?.iconColor || "",
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
  const showDefaultModelSelect =
    models.length > 0 && (isEdit || selectedPreset !== null);

  const defaultModelSelect = (
    <div className="space-y-2">
      <Label htmlFor="pi-default-model">{t("pi.form.defaultModel")}</Label>
      <Select value={selectedModelKey} onValueChange={setSelectedModelKey}>
        <SelectTrigger id="pi-default-model" className="w-full">
          <SelectValue placeholder={t("pi.form.defaultModelPlaceholder")} />
        </SelectTrigger>
        <SelectContent>
          {models.map((model, index) => (
            <SelectItem key={model.key} value={model.key}>
              {model.name ||
                model.id ||
                t("pi.form.modelNumber", { index: index + 1 })}
              {model.name && model.id ? ` · ${model.id}` : ""}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <p className="text-xs text-muted-foreground">{t("pi.form.modelsHint")}</p>
    </div>
  );

  return (
    <Form {...form}>
      <form
        id="provider-form"
        onSubmit={form.handleSubmit(submit)}
        className="space-y-6 glass rounded-xl p-6 border border-white/10"
      >
        {!isEdit && (
          <ProviderPresetSelector
            selectedPresetId={selectedPresetId}
            presetEntries={presetEntries}
            presetCategoryLabels={presetCategoryLabels}
            onPresetChange={selectPreset}
            category={category}
          />
        )}

        <BasicFormFields
          form={form}
          beforeNameSlot={
            isEdit || selectedPresetId === "custom" ? (
              <div className="space-y-2">
                <Label htmlFor="pi-provider-key">
                  {t("pi.form.providerKey")}
                  <span className="text-destructive ml-1">*</span>
                </Label>
                <Input
                  id="pi-provider-key"
                  value={providerKey}
                  onChange={(event) =>
                    setProviderKey(
                      event.target.value
                        .toLowerCase()
                        .replace(/[^a-z0-9-]/g, ""),
                    )
                  }
                  disabled={isEdit}
                  placeholder="my-provider"
                  autoComplete="off"
                />
                <p className="text-xs text-muted-foreground">
                  {t("pi.form.providerKeyHint")}
                </p>
              </div>
            ) : undefined
          }
        />

        <ApiKeySection
          id="pi-api-key"
          label={t("pi.form.credential")}
          value={apiKey}
          onChange={setApiKey}
          category={category}
          shouldShowLink={Boolean(selectedPreset?.apiKeyUrl)}
          websiteUrl={selectedPreset?.apiKeyUrl ?? ""}
          isPartner={selectedPreset?.isPartner}
          partnerPromotionKey={selectedPreset?.partnerPromotionKey}
          placeholder={{
            official: t("pi.form.apiKeyPlaceholder"),
            thirdParty: t("pi.form.apiKeyPlaceholder"),
          }}
        />

        {showDefaultModelSelect && defaultModelSelect}

        <Collapsible
          open={advancedOpen}
          onOpenChange={setAdvancedOpen}
          className="rounded-lg border border-border-default p-4"
        >
          <CollapsibleTrigger asChild>
            <Button
              type="button"
              variant={null}
              size="sm"
              className="h-8 w-full justify-start gap-1.5 px-0 text-sm font-medium text-foreground hover:opacity-70"
              aria-expanded={advancedOpen}
            >
              {advancedOpen ? (
                <ChevronDown className="h-4 w-4" />
              ) : (
                <ChevronRight className="h-4 w-4" />
              )}
              {t("providerForm.advancedOptionsToggle")}
            </Button>
          </CollapsibleTrigger>
          {!advancedOpen && (
            <p className="mt-1 ml-1 text-xs text-muted-foreground">
              {t("pi.form.advancedHint")}
            </p>
          )}
          <CollapsibleContent className="space-y-4 pt-3">
            <Field label={t("pi.form.providerApi")} htmlFor="pi-provider-api">
              <Input
                id="pi-provider-api"
                value={api}
                onChange={(event) => setApi(event.target.value)}
                list="pi-api-examples"
                placeholder="openai-responses"
              />
              <p className="text-xs text-muted-foreground">
                {t("pi.form.inheritanceHint")}
              </p>
            </Field>

            <EndpointField
              id="pi-provider-base-url"
              label={t("pi.form.providerBaseUrl")}
              value={baseUrl}
              onChange={setBaseUrl}
              placeholder="https://api.example.com/v1"
              showManageButton
              onManageClick={() => setIsEndpointModalOpen(true)}
              manageButtonLabel={t("pi.form.manageEndpoints")}
            />

            {!showDefaultModelSelect && defaultModelSelect}

            <div className="space-y-3">
              <div className="flex items-center justify-between gap-3">
                <div>
                  <Label>{t("pi.form.models")}</Label>
                  <p className="text-xs text-muted-foreground">
                    {t("pi.form.modelEditorHint")}
                  </p>
                </div>
                <div className="flex gap-1">
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={handleFetchModels}
                    disabled={isFetchingModels}
                    className="h-7 gap-1"
                  >
                    {isFetchingModels ? (
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    ) : (
                      <Download className="h-3.5 w-3.5" />
                    )}
                    {t("providerForm.fetchModels")}
                  </Button>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={addModel}
                    className="h-7 gap-1"
                  >
                    <Plus className="h-3.5 w-3.5" />
                    {t("pi.form.addModel")}
                  </Button>
                </div>
              </div>

              {models.map((model, index) => (
                <div
                  key={model.key}
                  className="space-y-3 rounded-lg border border-border-default p-4"
                >
                  <div className="flex items-center justify-between">
                    <span className="text-sm font-medium">
                      {model.name ||
                        model.id ||
                        t("pi.form.modelNumber", { index: index + 1 })}
                    </span>
                    {models.length > 1 && (
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        onClick={() => removeModel(model.key)}
                        aria-label={t("pi.form.removeModel")}
                        className="h-8 w-8"
                      >
                        <Trash2 className="h-4 w-4 text-destructive" />
                      </Button>
                    )}
                  </div>
                  <div className="grid gap-3 sm:grid-cols-2">
                    <Field
                      label={t("pi.form.modelId")}
                      htmlFor={`pi-model-id-${model.key}`}
                    >
                      <div className="flex gap-1">
                        <Input
                          id={`pi-model-id-${model.key}`}
                          value={model.id}
                          onChange={(event) =>
                            updateModel(model.key, { id: event.target.value })
                          }
                          placeholder="model-id"
                          className="flex-1"
                        />
                        {fetchedModels.length > 0 && (
                          <ModelDropdown
                            models={fetchedModels}
                            onSelect={(id) => updateModel(model.key, { id })}
                          />
                        )}
                      </div>
                    </Field>
                    <Field
                      label={t("pi.form.modelName")}
                      htmlFor={`pi-model-name-${model.key}`}
                    >
                      <Input
                        id={`pi-model-name-${model.key}`}
                        value={model.name}
                        onChange={(event) =>
                          updateModel(model.key, { name: event.target.value })
                        }
                        placeholder={t("pi.form.modelNamePlaceholder")}
                      />
                    </Field>
                    <Field
                      label={t("pi.form.modelApi")}
                      htmlFor={`pi-model-api-${model.key}`}
                    >
                      <Input
                        id={`pi-model-api-${model.key}`}
                        value={model.api}
                        onChange={(event) =>
                          updateModel(model.key, { api: event.target.value })
                        }
                        list="pi-api-examples"
                        placeholder={t("pi.form.inherit")}
                      />
                    </Field>
                    <Field
                      label={t("pi.form.modelBaseUrl")}
                      htmlFor={`pi-model-base-url-${model.key}`}
                    >
                      <Input
                        id={`pi-model-base-url-${model.key}`}
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
                    htmlFor={`pi-model-additional-${model.key}`}
                  >
                    <Textarea
                      id={`pi-model-additional-${model.key}`}
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
              ))}
            </div>

            <label
              htmlFor="pi-auth-header"
              className="flex items-center justify-between gap-4 rounded-lg border border-border-default p-3"
            >
              <span>
                <span className="block text-sm font-medium">
                  {t("pi.form.authHeader")}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t("pi.form.authHeaderHint")}
                </span>
              </span>
              <Switch
                id="pi-auth-header"
                checked={authHeader}
                onCheckedChange={(checked) => {
                  setAuthHeader(checked);
                  setAuthHeaderExplicit(true);
                }}
              />
            </label>

            <Field label={t("pi.form.headers")} htmlFor="pi-provider-headers">
              <Textarea
                id="pi-provider-headers"
                value={headersJson}
                onChange={(event) => setHeadersJson(event.target.value)}
                className="min-h-24 font-mono text-xs"
                spellCheck={false}
              />
            </Field>

            <Field
              label={t("pi.form.additionalConfig")}
              htmlFor="pi-provider-additional"
            >
              <Textarea
                id="pi-provider-additional"
                value={additionalJson}
                onChange={(event) => setAdditionalJson(event.target.value)}
                className="min-h-28 font-mono text-xs"
                spellCheck={false}
              />
              <p className="text-xs text-muted-foreground">
                {t("pi.form.additionalConfigHint")}
              </p>
            </Field>
          </CollapsibleContent>
        </Collapsible>

        <datalist id="pi-api-examples">
          {PI_API_EXAMPLES.map((value) => (
            <option key={value} value={value} />
          ))}
        </datalist>

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
    </Form>
  );
}

function Field({
  label,
  htmlFor,
  children,
}: {
  label: string;
  htmlFor: string;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <Label htmlFor={htmlFor}>{label}</Label>
      {children}
    </div>
  );
}
