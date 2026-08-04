import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Download,
  KeyRound,
  Loader2,
  RefreshCw,
  Terminal,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { piApi, type PiNativeDiagnostic } from "@/lib/api";
import { useAutoFailoverEnabled } from "@/lib/query/failover";
import { invalidatePiControlPlaneCaches } from "@/lib/query/mutations";
import type { Provider } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";

interface PiNativeCatalogPanelProps {
  providers: Record<string, Provider>;
  onCreate: () => void;
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

export function PiNativeCatalogPanel({
  providers,
  onCreate,
}: PiNativeCatalogPanelProps) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [loginGuideOpen, setLoginGuideOpen] = useState(false);
  const [modelPickerOpen, setModelPickerOpen] = useState(false);
  const [diagnosticsOpen, setDiagnosticsOpen] = useState(false);

  const catalog = useQuery({
    queryKey: ["pi", "nativeCatalog"],
    queryFn: () => piApi.getNativeCatalog(),
  });
  const defaults = useQuery({
    queryKey: ["pi", "nativeDefaults"],
    queryFn: () => piApi.getNativeDefaults(),
  });
  const currentState = useQuery({
    queryKey: ["pi", "currentState"],
    queryFn: () => piApi.getCurrentState(),
  });
  const { data: autoFailoverEnabled = false } = useAutoFailoverEnabled("pi");

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
    setSelectedProvider((current) => {
      if (nativeDefaultProviderId) return nativeDefaultProviderId;
      return current && providers[current] ? current : (providerIds[0] ?? "");
    });
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
    setSelectedModel((current) => {
      if (nativeModel && models.some((model) => model.id === nativeModel)) {
        return nativeModel;
      }
      return current && models.some((model) => model.id === current)
        ? current
        : (models[0]?.id ?? "");
    });
  }, [
    defaults.data?.defaultModel,
    nativeDefaultProviderId,
    models,
    selectedProvider,
  ]);

  const refreshAll = async () => {
    await invalidatePiControlPlaneCaches(queryClient);
  };

  const importMutation = useMutation({
    mutationFn: (entry: PiNativeDiagnostic) =>
      piApi.importNativeProvider(entry.providerKey, entry.fingerprint),
    onSuccess: async () => {
      await invalidatePiControlPlaneCaches(queryClient);
      toast.success(t("pi.native.imported"));
    },
    onError: (error) => {
      toast.error(extractErrorMessage(error) || t("pi.native.importFailed"));
      void refreshAll();
    },
  });

  const defaultMutation = useMutation({
    mutationFn: () => piApi.setDefaultModel(selectedProvider, selectedModel),
    onSuccess: async () => {
      await invalidatePiControlPlaneCaches(queryClient);
      toast.success(t("pi.native.defaultSaved"));
    },
    onError: (error) => {
      toast.error(
        extractErrorMessage(error) || t("pi.native.defaultSaveFailed"),
      );
      void refreshAll();
    },
  });

  const current = currentState.data;
  const currentName =
    (current?.managedProviderId
      ? providers[current.managedProviderId]?.name
      : undefined) ??
    current?.providerKey ??
    t("common.notSet");
  const entries = catalog.data ?? [];
  const loading =
    catalog.isLoading || defaults.isLoading || currentState.isLoading;
  const fetching =
    catalog.isFetching || defaults.isFetching || currentState.isFetching;
  const defaultIsCurrent =
    managedNativeKeys.get(selectedProvider) ===
      defaults.data?.defaultProvider &&
    defaults.data?.defaultModel === selectedModel;

  return (
    <section className="space-y-3 rounded-xl border border-border bg-muted/20 p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold">{t("pi.current.title")}</h2>
          <p className="mt-1 text-xs text-muted-foreground">
            {t("pi.current.description")}
          </p>
        </div>
        <Button
          type="button"
          size="sm"
          variant="outline"
          onClick={() => void refreshAll()}
          disabled={fetching}
        >
          <RefreshCw
            className={`mr-1.5 h-3.5 w-3.5 ${fetching ? "animate-spin" : ""}`}
          />
          {t("common.refresh")}
        </Button>
      </div>

      {currentState.isLoading ? (
        <div className="flex items-center gap-2 rounded-lg border bg-background/70 p-3 text-xs text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" />
          {t("common.loading")}
        </div>
      ) : currentState.isError ? (
        <div className="flex items-start gap-2 rounded-md border border-red-500/30 bg-red-500/5 p-3 text-xs text-red-700 dark:text-red-300">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
          {extractErrorMessage(currentState.error)}
        </div>
      ) : (
        <div className="rounded-lg border bg-background/70 p-3">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="min-w-0">
              <div className="truncate text-sm font-medium">{currentName}</div>
              <div className="mt-1 truncate text-xs text-muted-foreground">
                {current?.modelId ?? t("common.notSet")}
              </div>
            </div>
            {current && (
              <div className="flex flex-wrap gap-2 text-[11px]">
                <span className="rounded-full bg-muted px-2 py-1">
                  {t(`pi.current.ownership.${current.ownership}`)}
                </span>
                <span
                  className={`rounded-full px-2 py-1 ${
                    current.activeRoute === "gateway"
                      ? "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
                      : current.activeRoute === "direct"
                        ? "bg-blue-500/10 text-blue-700 dark:text-blue-300"
                        : "bg-amber-500/10 text-amber-700 dark:text-amber-300"
                  }`}
                >
                  {t(`pi.current.route.${current.activeRoute}`)}
                </span>
              </div>
            )}
          </div>
          {(current?.activeRoute === "unavailable" ||
            current?.routeReason === "native_catalog_unavailable") && (
            <p className="mt-2 text-xs text-amber-700 dark:text-amber-300">
              {t(`pi.current.reason.${current.routeReason}`)}
            </p>
          )}
          {autoFailoverEnabled && current?.activeRoute === "direct" && (
            <p className="mt-2 text-xs text-amber-700 dark:text-amber-300">
              {t("pi.current.failoverDirectOverride")}
            </p>
          )}
        </div>
      )}

      <div className="flex flex-wrap gap-2">
        <Button type="button" size="sm" onClick={onCreate}>
          <KeyRound className="mr-1.5 h-4 w-4" />
          {t("pi.current.addManaged")}
        </Button>
        <Button
          type="button"
          size="sm"
          variant="outline"
          onClick={() => setLoginGuideOpen((open) => !open)}
        >
          <Terminal className="mr-1.5 h-4 w-4" />
          {t("pi.current.nativeLogin")}
        </Button>
      </div>

      {loginGuideOpen && (
        <div className="rounded-lg border border-dashed bg-background/60 p-3 text-xs">
          <div className="font-medium">{t("pi.current.nativeLoginTitle")}</div>
          <ol className="mt-2 list-decimal space-y-1 pl-4 text-muted-foreground">
            <li>{t("pi.current.nativeLoginStep1")}</li>
            <li>{t("pi.current.nativeLoginStep2")}</li>
            <li>{t("pi.current.nativeLoginStep3")}</li>
          </ol>
          <p className="mt-2 text-muted-foreground">
            {t("pi.current.nativeDirectNote")}
          </p>
        </div>
      )}

      {providerIds.length > 0 && (
        <Collapsible open={modelPickerOpen} onOpenChange={setModelPickerOpen}>
          <CollapsibleTrigger asChild>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="gap-1 px-0 text-xs"
            >
              {modelPickerOpen ? (
                <ChevronDown className="h-4 w-4" />
              ) : (
                <ChevronRight className="h-4 w-4" />
              )}
              {t("pi.current.changeManagedDefault")}
            </Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="pt-2">
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
                  autoFailoverEnabled ||
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
            {autoFailoverEnabled && (
              <p className="mt-2 text-xs text-muted-foreground">
                {t("pi.current.failoverOwnsDefault")}
              </p>
            )}
          </CollapsibleContent>
        </Collapsible>
      )}

      <Collapsible open={diagnosticsOpen} onOpenChange={setDiagnosticsOpen}>
        <CollapsibleTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="gap-1 px-0 text-xs"
          >
            {diagnosticsOpen ? (
              <ChevronDown className="h-4 w-4" />
            ) : (
              <ChevronRight className="h-4 w-4" />
            )}
            {t("pi.current.detected", { count: entries.length })}
          </Button>
        </CollapsibleTrigger>
        <CollapsibleContent className="space-y-2 pt-2">
          <p className="text-xs text-muted-foreground">
            {t("pi.current.diagnosticsHint")}
          </p>
          {loading ? (
            <div className="flex items-center gap-2 py-3 text-xs text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              {t("common.loading")}
            </div>
          ) : catalog.isError ? (
            <div className="text-xs text-red-600">
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
                      {t(
                        `pi.native.management.${entry.managementStatus.status}`,
                      )}
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
        </CollapsibleContent>
      </Collapsible>
    </section>
  );
}
