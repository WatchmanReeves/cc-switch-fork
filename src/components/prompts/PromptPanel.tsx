import React, { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, FileText, RefreshCw } from "lucide-react";
import { type AppId } from "@/lib/api";
import { usePromptActions } from "@/hooks/usePromptActions";
import { useTauriEvent } from "@/hooks/useTauriEvent";
import PromptListItem from "./PromptListItem";
import PromptFormPanel from "./PromptFormPanel";
import { PiNativePromptResources } from "./PiNativePromptResources";
import { ConfirmDialog } from "../ConfirmDialog";
import { Button } from "@/components/ui/button";

interface PromptPanelProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: AppId;
}

export interface PromptPanelHandle {
  openAdd: () => void;
}

const PromptPanel = React.forwardRef<PromptPanelHandle, PromptPanelProps>(
  ({ open, appId }, ref) => {
    const { t } = useTranslation();
    const [isFormOpen, setIsFormOpen] = useState(false);
    const [editingId, setEditingId] = useState<string | null>(null);
    const [confirmDialog, setConfirmDialog] = useState<{
      isOpen: boolean;
      titleKey: string;
      messageKey: string;
      messageParams?: Record<string, unknown>;
      onConfirm: () => void;
    } | null>(null);

    const {
      prompts,
      loading,
      piLibraryStatus,
      reload,
      savePrompt,
      deletePrompt,
      toggleEnabled,
      reconcilePiLibrary,
    } = usePromptActions(appId);

    useEffect(() => {
      if (open) reload();
    }, [open, reload]);

    // Listen for prompt import events from deep link
    useEffect(() => {
      const handlePromptImported = (event: Event) => {
        const customEvent = event as CustomEvent;
        // Reload if the import is for this app
        if (customEvent.detail?.app === appId) {
          reload();
        }
      };

      window.addEventListener("prompt-imported", handlePromptImported);
      return () => {
        window.removeEventListener("prompt-imported", handlePromptImported);
      };
    }, [appId, reload]);

    // 应用项目 Profile 会切换激活的 prompt（prompts 非 react-query，需主动 reload）
    useTauriEvent("profile-applied", reload);

    const handleAdd = () => {
      setEditingId(null);
      setIsFormOpen(true);
    };

    React.useImperativeHandle(ref, () => ({
      openAdd: handleAdd,
    }));

    const handleEdit = (id: string) => {
      setEditingId(id);
      setIsFormOpen(true);
    };

    const handleDelete = (id: string) => {
      const prompt = prompts[id];
      setConfirmDialog({
        isOpen: true,
        titleKey: "prompts.confirm.deleteTitle",
        messageKey: "prompts.confirm.deleteMessage",
        messageParams: { name: prompt?.name },
        onConfirm: async () => {
          try {
            await deletePrompt(id);
            setConfirmDialog(null);
          } catch (e) {
            // Error handled by hook
          }
        },
      });
    };

    const promptEntries = useMemo(() => Object.entries(prompts), [prompts]);

    const enabledPrompt = promptEntries.find(([_, p]) => p.enabled);

    return (
      <div className="flex flex-col flex-1 min-h-0 px-6">
        <div className="flex-shrink-0 py-4 glass rounded-xl border border-white/10 mb-4 px-6">
          <div className="text-sm text-muted-foreground">
            {t("prompts.count", { count: promptEntries.length })} ·{" "}
            {enabledPrompt
              ? t("prompts.enabledName", { name: enabledPrompt[1].name })
              : t("prompts.noneEnabled")}
          </div>
        </div>

        <div className="flex-1 overflow-y-auto pb-16">
          {appId === "pi" && <PiNativePromptResources />}
          {appId === "pi" && (
            <div className="mb-3">
              <h3 className="text-sm font-semibold">
                {t("pi.prompts.agentsLibrary")}
              </h3>
              <p className="mt-1 text-xs text-muted-foreground">
                {t("pi.prompts.agentsLibraryDescription")}
              </p>
            </div>
          )}
          {appId === "pi" && piLibraryStatus?.needsReconciliation && (
            <div
              className="mb-4 flex items-start justify-between gap-3 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3"
              role="status"
            >
              <div className="flex min-w-0 gap-2">
                <AlertTriangle
                  className="mt-0.5 h-4 w-4 shrink-0 text-amber-600"
                  aria-hidden="true"
                />
                <div>
                  <p className="text-sm font-medium">
                    {t("pi.prompts.libraryDriftTitle")}
                  </p>
                  <p className="mt-1 text-xs text-muted-foreground">
                    {t(
                      piLibraryStatus.nativeExists
                        ? "pi.prompts.libraryDriftNative"
                        : "pi.prompts.libraryDriftMissing",
                    )}
                  </p>
                </div>
              </div>
              <Button
                type="button"
                variant="outline"
                size="sm"
                className="shrink-0"
                onClick={() => {
                  void reconcilePiLibrary().catch(() => undefined);
                }}
              >
                <RefreshCw className="mr-2 h-3.5 w-3.5" aria-hidden="true" />
                {t("pi.prompts.reconcileLibrary")}
              </Button>
            </div>
          )}
          {loading ? (
            <div className="text-center py-12 text-muted-foreground">
              {t("prompts.loading")}
            </div>
          ) : promptEntries.length === 0 ? (
            <div className="text-center py-12">
              <div className="w-16 h-16 mx-auto mb-4 bg-muted rounded-full flex items-center justify-center">
                <FileText size={24} className="text-muted-foreground" />
              </div>
              <h3 className="text-lg font-medium text-foreground mb-2">
                {t("prompts.empty")}
              </h3>
              <p className="text-muted-foreground text-sm">
                {t("prompts.emptyDescription")}
              </p>
            </div>
          ) : (
            <div className="space-y-3">
              {promptEntries.map(([id, prompt]) => (
                <PromptListItem
                  key={id}
                  id={id}
                  prompt={prompt}
                  onToggle={toggleEnabled}
                  onEdit={handleEdit}
                  onDelete={handleDelete}
                />
              ))}
            </div>
          )}
        </div>

        {isFormOpen && (
          <PromptFormPanel
            appId={appId}
            editingId={editingId || undefined}
            initialData={editingId ? prompts[editingId] : undefined}
            onSave={savePrompt}
            onClose={() => setIsFormOpen(false)}
          />
        )}

        {confirmDialog && (
          <ConfirmDialog
            isOpen={confirmDialog.isOpen}
            title={t(confirmDialog.titleKey)}
            message={t(confirmDialog.messageKey, confirmDialog.messageParams)}
            onConfirm={confirmDialog.onConfirm}
            onCancel={() => setConfirmDialog(null)}
          />
        )}
      </div>
    );
  },
);

PromptPanel.displayName = "PromptPanel";

export default PromptPanel;
