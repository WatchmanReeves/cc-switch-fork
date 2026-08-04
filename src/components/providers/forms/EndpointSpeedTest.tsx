import React, { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Zap, Loader2, Plus, X, AlertCircle, Save } from "lucide-react";
import type { AppId } from "@/lib/api";
import { vscodeApi } from "@/lib/api/vscode";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FullScreenPanel } from "@/components/common/FullScreenPanel";
import type { CustomEndpoint, EndpointCandidate } from "@/types";

// 端点测速超时配置（秒）
const ENDPOINT_TIMEOUT_SECS: Record<AppId, number> = {
  codex: 12,
  claude: 8,
  "claude-desktop": 8,
  gemini: 8,
  grokbuild: 12,
  opencode: 8,
  openclaw: 8,
  hermes: 8,
  pi: 12,
};

interface TestResult {
  url: string;
  latency: number | null;
  status?: number;
  error?: string | null;
}

interface EndpointSpeedTestProps {
  appId: AppId;
  providerId?: string;
  value: string;
  onChange: (url: string) => void;
  initialEndpoints: EndpointCandidate[];
  visible?: boolean;
  onClose: () => void;
  autoSelect: boolean;
  onAutoSelectChange: (checked: boolean) => void;
  // Reports the effective custom endpoint set. In immediate mode entries only
  // change after the backend mutation succeeds.
  onCustomEndpointsChange?: (urls: string[]) => void;
  /** Pi edit mode owns endpoint membership as explicit immediate mutations. */
  persistenceMode?: "batched" | "immediate";
}

interface EndpointEntry extends EndpointCandidate {
  id: string;
  latency: number | null;
  status?: number;
  error?: string | null;
}

const randomId = () => `ep_${Math.random().toString(36).slice(2, 9)}`;

const normalizeEndpointUrl = (url: string): string =>
  url.trim().replace(/\/+$/, "");

const buildInitialEntries = (
  candidates: EndpointCandidate[],
  selected: string,
): EndpointEntry[] => {
  const map = new Map<string, EndpointEntry>();
  const addCandidate = (candidate: EndpointCandidate) => {
    const sanitized = candidate.url ? normalizeEndpointUrl(candidate.url) : "";
    if (!sanitized) return;
    if (map.has(sanitized)) return;

    map.set(sanitized, {
      id: candidate.id ?? randomId(),
      url: sanitized,
      isCustom: candidate.isCustom ?? false,
      latency: null,
      status: undefined,
      error: null,
    });
  };

  candidates.forEach(addCandidate);

  const selectedUrl = normalizeEndpointUrl(selected);
  if (selectedUrl && !map.has(selectedUrl)) {
    addCandidate({ url: selectedUrl, isCustom: true });
  }

  return Array.from(map.values());
};

const EndpointSpeedTest: React.FC<EndpointSpeedTestProps> = ({
  appId,
  providerId,
  value,
  onChange,
  initialEndpoints,
  visible = true,
  onClose,
  autoSelect,
  onAutoSelectChange,
  onCustomEndpointsChange,
  persistenceMode = "batched",
}) => {
  const { t } = useTranslation();
  const [entries, setEntries] = useState<EndpointEntry[]>(() =>
    buildInitialEntries(initialEndpoints, value),
  );
  const [customUrl, setCustomUrl] = useState("");
  const [addError, setAddError] = useState<string | null>(null);
  const [isTesting, setIsTesting] = useState(false);
  const [lastError, setLastError] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);

  // 记录初始的自定义端点，用于对比变化
  const [initialCustomUrls, setInitialCustomUrls] = useState<Set<string>>(
    new Set(),
  );

  const normalizedSelected = normalizeEndpointUrl(value);

  const hasEndpoints = entries.length > 0;
  const isEditMode = Boolean(providerId); // 编辑模式有 providerId

  // 编辑模式：加载已保存的自定义端点
  useEffect(() => {
    let cancelled = false;

    const loadCustomEndpoints = async () => {
      try {
        if (!providerId) return; // 新建模式不加载

        const customEndpoints = await vscodeApi.getCustomEndpoints(
          appId,
          providerId,
        );

        if (cancelled) return;

        const candidates: EndpointCandidate[] = customEndpoints.map(
          (ep: CustomEndpoint) => ({
            url: ep.url,
            isCustom: true,
          }),
        );

        // 记录初始的自定义端点
        const customUrls = new Set(
          customEndpoints.map((ep) => normalizeEndpointUrl(ep.url)),
        );
        setInitialCustomUrls(customUrls);

        // In edit mode the endpoint service owns membership. Keep non-custom
        // candidates (such as the configured base URL), then rebuild custom
        // membership from the authoritative backend result.
        setEntries((prev) => {
          const map = new Map<string, EndpointEntry>();

          prev
            .filter((entry) => !entry.isCustom)
            .forEach((entry) => {
              map.set(entry.url, entry);
            });

          candidates.forEach((candidate) => {
            const sanitized = normalizeEndpointUrl(candidate.url);
            if (!sanitized) return;

            const existing = map.get(sanitized);
            if (existing) {
              map.set(sanitized, { ...existing, isCustom: true });
            } else {
              map.set(sanitized, {
                id: randomId(),
                url: sanitized,
                isCustom: true,
                latency: null,
                status: undefined,
                error: null,
              });
            }
          });

          return Array.from(map.values());
        });
      } catch (error) {
        if (!cancelled) {
          console.error(t("endpointTest.loadEndpointsFailed"), error);
        }
      }
    };

    // 只在编辑模式下加载
    if (providerId) {
      loadCustomEndpoints();
    }

    return () => {
      cancelled = true;
    };
  }, [appId, providerId, t]);

  // Keep the parent draft aligned with the effective endpoint membership.
  // Immediate-mode state changes happen only after persistence succeeds.
  useEffect(() => {
    if (!onCustomEndpointsChange) return;
    try {
      const customUrls = Array.from(
        new Set(
          entries
            .filter((e) => e.isCustom)
            .map((e) => (e.url ? normalizeEndpointUrl(e.url) : ""))
            .filter(Boolean),
        ),
      );
      onCustomEndpointsChange(customUrls);
    } catch (err) {
      // ignore
    }
  }, [entries, onCustomEndpointsChange]);

  const sortedEntries = useMemo(() => {
    return entries.slice().sort((a: TestResult, b: TestResult) => {
      const aLatency = a.latency ?? Number.POSITIVE_INFINITY;
      const bLatency = b.latency ?? Number.POSITIVE_INFINITY;
      if (aLatency === bLatency) {
        return a.url.localeCompare(b.url);
      }
      return aLatency - bLatency;
    });
  }, [entries]);

  const handleAddEndpoint = useCallback(async () => {
    if (isSaving) return;

    const candidate = customUrl.trim();
    let errorMsg: string | null = null;

    if (!candidate) {
      errorMsg = t("endpointTest.enterValidUrl");
    }

    let parsed: URL | null = null;
    if (!errorMsg) {
      try {
        parsed = new URL(candidate);
      } catch {
        errorMsg = t("endpointTest.invalidUrlFormat");
      }
    }

    // 明确只允许 http: 和 https:
    const allowedProtocols = ["http:", "https:"];
    if (!errorMsg && parsed && !allowedProtocols.includes(parsed.protocol)) {
      errorMsg = t("endpointTest.onlyHttps");
    }

    let sanitized = "";
    if (!errorMsg && parsed) {
      sanitized = normalizeEndpointUrl(parsed.toString());
      // 使用当前 entries 做去重校验
      const isDuplicate = entries.some((entry) => entry.url === sanitized);
      if (isDuplicate) {
        errorMsg = t("endpointTest.urlExists");
      }
    }

    if (errorMsg) {
      setAddError(errorMsg);
      return;
    }

    setAddError(null);
    setLastError(null);

    if (persistenceMode === "immediate" && providerId) {
      setIsSaving(true);
      try {
        await vscodeApi.addCustomEndpoint(appId, providerId, sanitized);
        setInitialCustomUrls((current) => new Set(current).add(sanitized));
      } catch (error) {
        setLastError(
          error instanceof Error ? error.message : t("endpointTest.saveFailed"),
        );
        setIsSaving(false);
        return;
      } finally {
        setIsSaving(false);
      }
    }

    setEntries((prev) => {
      if (prev.some((e) => e.url === sanitized)) return prev;
      return [
        ...prev,
        {
          id: randomId(),
          url: sanitized,
          isCustom: true,
          latency: null,
          status: undefined,
          error: null,
        },
      ];
    });

    if (!normalizedSelected) {
      onChange(sanitized);
    }

    setCustomUrl("");
  }, [
    appId,
    customUrl,
    entries,
    isSaving,
    normalizedSelected,
    onChange,
    persistenceMode,
    providerId,
    t,
  ]);

  const handleRemoveEndpoint = useCallback(
    async (entry: EndpointEntry) => {
      if (isSaving) return;

      // 清空之前的错误提示
      setLastError(null);

      if (persistenceMode === "immediate" && providerId && entry.isCustom) {
        setIsSaving(true);
        try {
          await vscodeApi.removeCustomEndpoint(appId, providerId, entry.url);
          setInitialCustomUrls((current) => {
            const next = new Set(current);
            next.delete(entry.url);
            return next;
          });
        } catch (error) {
          setLastError(
            error instanceof Error
              ? error.message
              : t("endpointTest.saveFailed"),
          );
          setIsSaving(false);
          return;
        } finally {
          setIsSaving(false);
        }
      }

      setEntries((prev) => {
        const next = prev.filter((item) => item.id !== entry.id);
        if (entry.url === normalizedSelected) {
          const fallback = next[0];
          onChange(fallback ? fallback.url : "");
        }
        return next;
      });
    },
    [
      appId,
      isSaving,
      normalizedSelected,
      onChange,
      persistenceMode,
      providerId,
      t,
    ],
  );

  const runSpeedTest = useCallback(async () => {
    const urls = entries.map((entry) => entry.url);
    if (urls.length === 0) {
      setLastError(t("endpointTest.pleaseAddEndpoint"));
      return;
    }

    setIsTesting(true);
    setLastError(null);

    // 清空所有延迟数据，显示 loading 状态
    setEntries((prev) =>
      prev.map((entry) => ({
        ...entry,
        latency: null,
        status: undefined,
        error: null,
      })),
    );

    try {
      const results = await vscodeApi.testApiEndpoints(urls, {
        timeoutSecs: ENDPOINT_TIMEOUT_SECS[appId],
      });

      const resultMap = new Map(
        results.map((item) => [normalizeEndpointUrl(item.url), item]),
      );

      setEntries((prev) =>
        prev.map((entry) => {
          const match = resultMap.get(entry.url);
          if (!match) {
            return {
              ...entry,
              latency: null,
              status: undefined,
              error: t("endpointTest.noResult"),
            };
          }
          return {
            ...entry,
            latency:
              typeof match.latency === "number"
                ? Math.round(match.latency)
                : null,
            status: match.status,
            error: match.error ?? null,
          };
        }),
      );

      if (autoSelect) {
        const successful = results
          .filter(
            (item) => typeof item.latency === "number" && item.latency !== null,
          )
          .sort((a, b) => (a.latency! || 0) - (b.latency! || 0));
        const best = successful[0];
        if (best && best.url && best.url !== normalizedSelected) {
          onChange(best.url);
        }
      }
    } catch (error) {
      const message =
        error instanceof Error
          ? error.message
          : `${t("endpointTest.testFailed", { error: String(error) })}`;
      setLastError(message);
    } finally {
      setIsTesting(false);
    }
  }, [entries, autoSelect, appId, normalizedSelected, onChange, t]);

  const handleSelect = useCallback(
    (url: string) => {
      if (!url || url === normalizedSelected) return;
      onChange(url);
    },
    [normalizedSelected, onChange],
  );

  // 保存端点变更
  const handleSave = useCallback(async () => {
    // 编辑模式：对比初始端点和当前端点，批量保存变更
    if (isEditMode && providerId && persistenceMode === "batched") {
      setIsSaving(true);
      setLastError(null);

      try {
        // 获取当前的自定义端点
        const currentCustomUrls = new Set(
          entries
            .filter((e) => e.isCustom)
            .map((e) => normalizeEndpointUrl(e.url)),
        );

        // 找出新增的端点
        const toAdd = Array.from(currentCustomUrls).filter(
          (url) => !initialCustomUrls.has(url),
        );

        // 找出删除的端点
        const toRemove = Array.from(initialCustomUrls).filter(
          (url) => !currentCustomUrls.has(url),
        );

        // 批量添加
        for (const url of toAdd) {
          await vscodeApi.addCustomEndpoint(appId, providerId, url);
        }

        // 批量删除
        for (const url of toRemove) {
          await vscodeApi.removeCustomEndpoint(appId, providerId, url);
        }

        // 更新初始端点列表
        setInitialCustomUrls(currentCustomUrls);
      } catch (error) {
        const message =
          error instanceof Error ? error.message : t("endpointTest.saveFailed");
        setLastError(message);
        setIsSaving(false);
        return;
      } finally {
        setIsSaving(false);
      }
    }

    // 关闭弹窗
    onClose();
  }, [
    isEditMode,
    providerId,
    persistenceMode,
    entries,
    initialCustomUrls,
    appId,
    onClose,
    t,
  ]);

  if (!visible) return null;

  const footer =
    persistenceMode === "immediate" && isEditMode ? (
      <Button type="button" onClick={onClose} disabled={isSaving}>
        {t("common.done", { defaultValue: "Done" })}
      </Button>
    ) : (
      <div className="flex items-center gap-2">
        <Button
          type="button"
          variant="outline"
          onClick={(event) => {
            event.preventDefault();
            onClose();
          }}
          disabled={isSaving}
        >
          {t("common.cancel")}
        </Button>
        <Button
          type="button"
          onClick={handleSave}
          disabled={isSaving}
          className="gap-2"
        >
          {isSaving ? (
            <>
              <Loader2 className="w-4 h-4 animate-spin" />
              {t("common.saving")}
            </>
          ) : (
            <>
              <Save className="w-4 h-4" />
              {t("common.save")}
            </>
          )}
        </Button>
      </div>
    );

  return (
    <FullScreenPanel
      isOpen={visible}
      title={t("endpointTest.title")}
      onClose={onClose}
      footer={footer}
    >
      <div className="glass rounded-xl p-6 border border-white/10 flex flex-col gap-6">
        {persistenceMode === "immediate" && isEditMode && (
          <p className="rounded-md border border-dashed px-3 py-2 text-xs text-muted-foreground">
            {t("pi.form.endpointChangesImmediate")}
          </p>
        )}
        {/* 测速控制栏 */}
        <div className="flex items-center justify-between">
          <div className="text-sm text-muted-foreground">
            {entries.length} {t("endpointTest.endpoints")}
          </div>
          <div className="flex items-center gap-3">
            <label className="flex items-center gap-1.5 text-xs text-gray-600 dark:text-gray-400">
              <input
                type="checkbox"
                checked={autoSelect}
                onChange={(event) => {
                  onAutoSelectChange(event.target.checked);
                }}
                className="h-3.5 w-3.5 rounded border-border-default bg-background text-primary focus:ring-2 focus:ring-primary/20"
              />
              {t("endpointTest.autoSelect")}
            </label>
            <Button
              type="button"
              onClick={runSpeedTest}
              disabled={isTesting || !hasEndpoints}
              size="sm"
              className="h-7 w-24 gap-1.5 text-xs bg-primary text-primary-foreground hover:bg-primary/90 disabled:opacity-60"
            >
              {isTesting ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  {t("endpointTest.testing")}
                </>
              ) : (
                <>
                  <Zap className="h-3.5 w-3.5" />
                  {t("endpointTest.testSpeed")}
                </>
              )}
            </Button>
          </div>
        </div>

        {/* 添加输入 */}
        <div className="space-y-1.5">
          <div className="flex gap-2">
            <Input
              type="text"
              value={customUrl}
              disabled={isSaving}
              placeholder={t("endpointTest.addEndpointPlaceholder")}
              onChange={(event) => setCustomUrl(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  handleAddEndpoint();
                }
              }}
              className="flex-1"
            />
            <Button
              type="button"
              onClick={handleAddEndpoint}
              disabled={isSaving}
              variant="outline"
              size="icon"
            >
              <Plus className="h-4 w-4" />
            </Button>
          </div>
          {addError && (
            <div className="flex items-center gap-1.5 text-xs text-red-600 dark:text-red-400">
              <AlertCircle className="h-3 w-3" />
              {addError}
            </div>
          )}
        </div>

        {/* 端点列表 */}
        {hasEndpoints ? (
          <div className="space-y-2">
            {sortedEntries.map((entry) => {
              const isSelected = normalizedSelected === entry.url;
              const latency = entry.latency;

              return (
                <div
                  key={entry.id}
                  onClick={() => handleSelect(entry.url)}
                  className={`group flex cursor-pointer items-center justify-between px-3 py-2.5 rounded-lg border transition text-foreground ${
                    isSelected
                      ? "border-primary/70 bg-primary/5 shadow-sm"
                      : "border-border-default bg-background hover:bg-muted"
                  }`}
                >
                  <div className="flex min-w-0 flex-1 items-center gap-3">
                    {/* 选择指示器 */}
                    <div
                      className={`h-1.5 w-1.5 flex-shrink-0 rounded-full transition ${
                        isSelected
                          ? "bg-blue-500 dark:bg-blue-400"
                          : "bg-gray-300 dark:bg-gray-700"
                      }`}
                    />

                    {/* 内容 */}
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm text-foreground">
                        {entry.url}
                      </div>
                    </div>
                  </div>

                  {/* 右侧信息 */}
                  <div className="flex items-center gap-2">
                    {latency !== null ? (
                      <div className="text-right">
                        <div
                          className={`font-mono text-sm font-medium ${
                            latency < 300
                              ? "text-emerald-600 dark:text-emerald-400"
                              : latency < 500
                                ? "text-yellow-600 dark:text-yellow-400"
                                : latency < 800
                                  ? "text-orange-600 dark:text-orange-400"
                                  : "text-red-600 dark:text-red-400"
                          }`}
                        >
                          {latency}ms
                        </div>
                      </div>
                    ) : isTesting ? (
                      <Loader2 className="h-4 w-4 animate-spin text-gray-400" />
                    ) : entry.error ? (
                      <div className="text-xs text-gray-400">
                        {t("endpointTest.failed")}
                      </div>
                    ) : (
                      <div className="text-xs text-gray-400">—</div>
                    )}

                    <button
                      type="button"
                      onClick={(event) => {
                        event.stopPropagation();
                        void handleRemoveEndpoint(entry);
                      }}
                      disabled={isSaving}
                      className="opacity-0 transition hover:text-red-600 group-hover:opacity-100 disabled:cursor-not-allowed disabled:opacity-30 dark:hover:text-red-400"
                    >
                      <X className="h-4 w-4" />
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        ) : (
          <div className="rounded-md border border-dashed border-border-default bg-muted px-4 py-8 text-center text-sm text-muted-foreground">
            {t("endpointTest.empty")}
          </div>
        )}

        {/* 错误提示 */}
        {lastError && (
          <div className="flex items-center gap-1.5 text-xs text-red-600 dark:text-red-400">
            <AlertCircle className="h-3 w-3" />
            {lastError}
          </div>
        )}
      </div>
    </FullScreenPanel>
  );
};

export default EndpointSpeedTest;
