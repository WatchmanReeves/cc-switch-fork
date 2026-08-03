import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  Loader2,
  RefreshCw,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { piApi, type PiNativeDiagnostic } from "@/lib/api";
import type { Provider } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";

interface PiNativeCatalogPanelProps {
  providers: Record<string, Provider>;
}

interface PiModelChoice {
  id: string;
  name: string;
}

function providerModels(provider: Provider | undefined): PiModelChoice[] {
  const models = provider?.settingsConfig?.models;
  if (!Array.isArray(models)) return [];
  return models.flatMap((value) => {
    if (!value || typeof value !== "object" || Array.isArray(value)) return [];
    const model = value as Record<string, unknown>;
    if (typeof model.id !== "string" || !model.id) return [];
    return [
      {
        id: model.id,
        name:
          typeof model.name === "string" && model.name ? model.name : model.id,
      },
    ];
  });
}

function diagnosticTone(entry: PiNativeDiagnostic): string {
  if (entry.rawValidity === "invalid") {
    return "border-red-500/30 bg-red-500/5";
  }
  if (
    entry.rawValidity === "unknown" ||
    entry.gatewayStatus === "unknown" ||
    entry.managementStatus.status === "unsupported"
  ) {
    return "border-amber-500/30 bg-amber-500/5";
  }
  return "border-border bg-background/50";
}

export function PiNativeCatalogPanel({ providers }: PiNativeCatalogPanelProps) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const catalog = useQuery({
    queryKey: ["pi", "nativeCatalog"],
    queryFn: () => piApi.getNativeCatalog(),
  });
  const defaults = useQuery({
    queryKey: ["pi", "nativeDefaults"],
    queryFn: () => piApi.getNativeDefaults(),
  });
  const providerIds = useMemo(() => Object.keys(providers), [providers]);
  const managedNativeKeys = useMemo(() => {
    const keys = new Map<string, string>();
    for (const entry of catalog.data ?? []) {
      if (entry.managementStatus.status === "managed") {
        keys.set(entry.managementStatus.providerId, entry.providerKey);
      }
    }
    return keys;
  }, [catalog.data]);
  const nativeDefaultProviderId = useMemo(() => {
    const providerKey = defaults.data?.defaultProvider;
    if (!providerKey) return undefined;
    for (const [providerId, managedKey] of managedNativeKeys) {
      if (managedKey === providerKey && providers[providerId]) {
        return providerId;
      }
    }
    return undefined;
  }, [defaults.data?.defaultProvider, managedNativeKeys, providers]);
  const [selectedProvider, setSelectedProvider] = useState("");
  const [selectedModel, setSelectedModel] = useState("");

  useEffect(() => {
    const next = nativeDefaultProviderId ?? providerIds[0] ?? "";
    setSelectedProvider((current) =>
      current && providers[current] ? current : next,
    );
  }, [nativeDefaultProviderId, providerIds, providers]);

  const models = useMemo(
    () => providerModels(providers[selectedProvider]),
    [providers, selectedProvider],
  );

  useEffect(() => {
    const nativeModel =
      nativeDefaultProviderId === selectedProvider
        ? defaults.data?.defaultModel
        : undefined;
    const next =
      (nativeModel && models.some((model) => model.id === nativeModel)
        ? nativeModel
        : undefined) ??
      models[0]?.id ??
      "";
    setSelectedModel((current) =>
      current && models.some((model) => model.id === current) ? current : next,
    );
  }, [
    defaults.data?.defaultModel,
    nativeDefaultProviderId,
    models,
    selectedProvider,
  ]);

  const importMutation = useMutation({
    mutationFn: (entry: PiNativeDiagnostic) =>
      piApi.importNativeProvider(entry.providerKey, entry.fingerprint),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["pi", "nativeCatalog"] }),
        queryClient.invalidateQueries({ queryKey: ["providers", "pi"] }),
      ]);
      toast.success(t("pi.native.imported"));
    },
    onError: (error) => {
      toast.error(extractErrorMessage(error) || t("pi.native.importFailed"));
      void catalog.refetch();
    },
  });

  const defaultMutation = useMutation({
    mutationFn: () => piApi.setDefaultModel(selectedProvider, selectedModel),
    onSuccess: async () => {
      await Promise.all([
        defaults.refetch(),
        queryClient.invalidateQueries({ queryKey: ["providers", "pi"] }),
      ]);
      toast.success(t("pi.native.defaultSaved"));
    },
    onError: (error) =>
      toast.error(
        extractErrorMessage(error) || t("pi.native.defaultSaveFailed"),
      ),
  });

  const entries = catalog.data ?? [];
  const loading = catalog.isLoading || defaults.isLoading;
  const defaultIsCurrent =
    managedNativeKeys.get(selectedProvider) ===
      defaults.data?.defaultProvider &&
    defaults.data?.defaultModel === selectedModel;

  return (
    <section className="rounded-xl border border-border bg-muted/20 p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold">{t("pi.native.title")}</h2>
          <p className="mt-1 text-xs text-muted-foreground">
            {t("pi.native.description")}
          </p>
        </div>
        <Button
          type="button"
          size="sm"
          variant="outline"
          onClick={() =>
            void Promise.all([catalog.refetch(), defaults.refetch()])
          }
          disabled={catalog.isFetching || defaults.isFetching}
        >
          <RefreshCw
            className={`mr-1.5 h-3.5 w-3.5 ${
              catalog.isFetching || defaults.isFetching ? "animate-spin" : ""
            }`}
          />
          {t("common.refresh")}
        </Button>
      </div>

      {providerIds.length > 0 && (
        <div className="mt-4 rounded-lg border bg-background/70 p-3">
          <div className="mb-2 text-xs font-medium">
            {t("pi.native.defaultModel")}
          </div>
          <div className="grid gap-2 sm:grid-cols-[1fr_1fr_auto]">
            <Select
              value={selectedProvider}
              onValueChange={setSelectedProvider}
            >
              <SelectTrigger aria-label={t("pi.native.defaultProvider")}>
                <SelectValue placeholder={t("pi.native.defaultProvider")} />
              </SelectTrigger>
              <SelectContent>
                {providerIds.map((providerId) => (
                  <SelectItem key={providerId} value={providerId}>
                    {providers[providerId].name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Select value={selectedModel} onValueChange={setSelectedModel}>
              <SelectTrigger aria-label={t("pi.native.defaultModel")}>
                <SelectValue placeholder={t("pi.native.defaultModel")} />
              </SelectTrigger>
              <SelectContent>
                {models.map((model) => (
                  <SelectItem key={model.id} value={model.id}>
                    {model.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button
              type="button"
              onClick={() => defaultMutation.mutate()}
              disabled={
                !selectedProvider ||
                !selectedModel ||
                defaultIsCurrent ||
                defaultMutation.isPending
              }
            >
              {defaultMutation.isPending && (
                <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
              )}
              {defaultIsCurrent
                ? t("pi.native.currentDefault")
                : t("pi.native.setDefault")}
            </Button>
          </div>
          {defaults.data?.defaultProvider && !nativeDefaultProviderId && (
            <p className="mt-2 text-xs text-amber-700 dark:text-amber-300">
              {t("pi.native.unmanagedDefault", {
                provider: defaults.data.defaultProvider,
                model: defaults.data.defaultModel ?? t("common.notSet"),
              })}
            </p>
          )}
        </div>
      )}

      <div className="mt-4 space-y-2">
        {loading ? (
          <div className="flex items-center gap-2 py-3 text-xs text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            {t("common.loading")}
          </div>
        ) : catalog.isError ? (
          <div className="flex items-start gap-2 rounded-md border border-red-500/30 bg-red-500/5 p-3 text-xs text-red-700 dark:text-red-300">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
            {extractErrorMessage(catalog.error)}
          </div>
        ) : entries.length === 0 ? (
          <p className="py-2 text-xs text-muted-foreground">
            {t("pi.native.empty")}
          </p>
        ) : (
          entries.map((entry) => (
            <div
              key={entry.providerKey}
              className={`flex flex-wrap items-center justify-between gap-3 rounded-lg border p-3 ${diagnosticTone(
                entry,
              )}`}
            >
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="truncate text-sm font-medium">
                    {entry.displayName || entry.providerKey}
                  </span>
                  <code className="rounded bg-muted px-1.5 py-0.5 text-[10px]">
                    {entry.providerKey}
                  </code>
                  <span className="text-[10px] text-muted-foreground">
                    {t(`pi.native.management.${entry.managementStatus.status}`)}
                    {" · "}
                    {t(`pi.native.gateway.${entry.gatewayStatus}`)}
                  </span>
                </div>
                {entry.reasons.length > 0 && (
                  <p className="mt-1 break-all text-[10px] text-muted-foreground">
                    {entry.reasons
                      .map(
                        (reason) =>
                          `${reason.layer}:${reason.code}${
                            reason.jsonPointer ? `@${reason.jsonPointer}` : ""
                          }`,
                      )
                      .join(", ")}
                  </p>
                )}
              </div>
              {entry.managementStatus.status === "importable" ? (
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  onClick={() => importMutation.mutate(entry)}
                  disabled={importMutation.isPending}
                >
                  {importMutation.isPending ? (
                    <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
                  ) : (
                    <Download className="mr-1.5 h-4 w-4" />
                  )}
                  {t("common.import")}
                </Button>
              ) : entry.managementStatus.status === "managed" ? (
                <span className="flex items-center gap-1 text-xs text-emerald-700 dark:text-emerald-300">
                  <CheckCircle2 className="h-4 w-4" />
                  {t("pi.native.managed")}
                </span>
              ) : null}
            </div>
          ))
        )}
      </div>
    </section>
  );
}
