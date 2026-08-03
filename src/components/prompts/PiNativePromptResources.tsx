import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FilePlus2, Loader2, RefreshCw, Trash2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  promptsApi,
  type PiPromptFileKind,
  type PiPromptFileSnapshot,
  type PiPromptTemplate,
} from "@/lib/api/prompts";
import { extractErrorMessage } from "@/utils/errorUtils";

const EDITABLE_FILES: Array<{
  kind: Exclude<PiPromptFileKind, "global_context">;
  filename: string;
  titleKey: string;
  descriptionKey: string;
}> = [
  {
    kind: "system_override",
    filename: "SYSTEM.md",
    titleKey: "pi.prompts.systemOverride",
    descriptionKey: "pi.prompts.systemOverrideDescription",
  },
  {
    kind: "system_append",
    filename: "APPEND_SYSTEM.md",
    titleKey: "pi.prompts.systemAppend",
    descriptionKey: "pi.prompts.systemAppendDescription",
  },
];

function mutationError(error: unknown, fallback: string) {
  toast.error(extractErrorMessage(error) || fallback);
}

function PiInstructionFileEditor({
  kind,
  filename,
  titleKey,
  descriptionKey,
}: (typeof EDITABLE_FILES)[number]) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState("");
  const [confirmCreate, setConfirmCreate] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const queryKey = ["pi", "promptFile", kind] as const;
  const query = useQuery({
    queryKey,
    queryFn: () => promptsApi.getPiPromptFile(kind),
  });

  useEffect(() => {
    if (query.data) setDraft(query.data.content);
  }, [query.data?.revision]);

  const save = useMutation({
    mutationFn: () => {
      const snapshot = query.data;
      if (!snapshot) throw new Error(t("pi.prompts.loadFirst"));
      return promptsApi.replacePiPromptFile(kind, snapshot.revision, draft);
    },
    onSuccess: (snapshot) => {
      queryClient.setQueryData<PiPromptFileSnapshot>(queryKey, snapshot);
      setConfirmCreate(false);
      toast.success(t("pi.prompts.fileSaved", { filename }));
    },
    onError: (error) => {
      mutationError(error, t("pi.prompts.saveFailed"));
      void query.refetch();
    },
  });
  const remove = useMutation({
    mutationFn: async () => {
      const snapshot = query.data;
      if (!snapshot) throw new Error(t("pi.prompts.loadFirst"));
      await promptsApi.deletePiPromptFile(kind, snapshot.revision);
      return promptsApi.getPiPromptFile(kind);
    },
    onSuccess: (snapshot) => {
      queryClient.setQueryData<PiPromptFileSnapshot>(queryKey, snapshot);
      setDraft("");
      setConfirmDelete(false);
      toast.success(t("pi.prompts.fileDeactivated", { filename }));
    },
    onError: (error) => {
      mutationError(error, t("pi.prompts.deleteFailed"));
      void query.refetch();
    },
  });

  const busy = save.isPending || remove.isPending;
  const changed = Boolean(query.data && draft !== query.data.content);
  const blank = !draft.trim();
  const requestSave = () => {
    if (kind === "system_override" && query.data && !query.data.exists) {
      setConfirmCreate(true);
      return;
    }
    save.mutate();
  };

  return (
    <div className="rounded-lg border border-border bg-background/60 p-4">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <div className="flex items-center gap-2">
            <h4 className="text-sm font-medium">{t(titleKey)}</h4>
            <Badge variant={query.data?.exists ? "default" : "outline"}>
              {query.data?.exists
                ? t("pi.prompts.active")
                : t("pi.prompts.inactive")}
            </Badge>
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            {t(descriptionKey)}
          </p>
          {query.data?.path && (
            <code className="mt-1 block break-all text-[10px] text-muted-foreground">
              {query.data.path}
            </code>
          )}
        </div>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          onClick={() => void query.refetch()}
          disabled={query.isFetching || busy}
          title={t("common.refresh")}
        >
          <RefreshCw
            className={`h-4 w-4 ${query.isFetching ? "animate-spin" : ""}`}
          />
        </Button>
      </div>

      {query.isLoading ? (
        <div className="flex items-center gap-2 py-6 text-xs text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" />
          {t("common.loading")}
        </div>
      ) : (
        <>
          <Textarea
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            className="mt-3 min-h-28 font-mono text-xs"
            placeholder={t("pi.prompts.instructionPlaceholder")}
            spellCheck={false}
          />
          {blank && (
            <p className="mt-1 text-xs text-destructive">
              {t("pi.prompts.blankInstruction")}
            </p>
          )}
          <div className="mt-3 flex justify-end gap-2">
            {query.data?.exists && (
              <Button
                type="button"
                variant="outline"
                onClick={() => setConfirmDelete(true)}
                disabled={busy}
              >
                {t("pi.prompts.deactivate")}
              </Button>
            )}
            <Button
              type="button"
              onClick={requestSave}
              disabled={!query.data || !changed || blank || busy}
            >
              {save.isPending && (
                <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
              )}
              {t("common.save")}
            </Button>
          </div>
        </>
      )}

      <ConfirmDialog
        isOpen={confirmCreate}
        title={t("pi.prompts.activateOverrideTitle", { filename })}
        message={t("pi.prompts.activateOverrideMessage", { filename })}
        confirmText={t("pi.prompts.activateOverride")}
        onConfirm={() => save.mutate()}
        onCancel={() => setConfirmCreate(false)}
      />
      <ConfirmDialog
        isOpen={confirmDelete}
        title={t("pi.prompts.deactivateTitle", { filename })}
        message={t("pi.prompts.deactivateMessage", { filename })}
        confirmText={t("pi.prompts.deactivate")}
        onConfirm={() => remove.mutate()}
        onCancel={() => setConfirmDelete(false)}
      />
    </div>
  );
}

function PiTemplateEditor({
  template,
  onChanged,
}: {
  template: PiPromptTemplate;
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState(template.content);
  const [confirmDelete, setConfirmDelete] = useState(false);

  useEffect(
    () => setDraft(template.content),
    [template.content, template.revision],
  );

  const save = useMutation({
    mutationFn: () =>
      promptsApi.upsertPiPromptTemplate(
        template.slug,
        template.revision,
        draft,
      ),
    onSuccess: () => {
      toast.success(t("pi.prompts.templateSaved", { slug: template.slug }));
      onChanged();
    },
    onError: (error) =>
      mutationError(error, t("pi.prompts.templateSaveFailed")),
  });
  const remove = useMutation({
    mutationFn: () =>
      promptsApi.deletePiPromptTemplate(template.slug, template.revision),
    onSuccess: () => {
      setConfirmDelete(false);
      toast.success(t("pi.prompts.templateDeleted", { slug: template.slug }));
      onChanged();
    },
    onError: (error) =>
      mutationError(error, t("pi.prompts.templateDeleteFailed")),
  });

  const busy = save.isPending || remove.isPending;
  return (
    <div className="rounded-lg border border-border bg-background/60 p-3">
      <div className="flex items-center justify-between gap-2">
        <code className="text-xs font-medium">/{template.slug}</code>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          onClick={() => setConfirmDelete(true)}
          disabled={busy}
          title={t("common.delete")}
        >
          <Trash2 className="h-4 w-4 text-destructive" />
        </Button>
      </div>
      <Textarea
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
        className="mt-2 min-h-24 font-mono text-xs"
        spellCheck={false}
      />
      <div className="mt-2 flex justify-end">
        <Button
          type="button"
          size="sm"
          onClick={() => save.mutate()}
          disabled={draft === template.content || busy}
        >
          {save.isPending && (
            <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
          )}
          {t("common.save")}
        </Button>
      </div>
      <ConfirmDialog
        isOpen={confirmDelete}
        title={t("pi.prompts.deleteTemplateTitle", {
          slug: template.slug,
        })}
        message={t("pi.prompts.deleteTemplateMessage", {
          slug: template.slug,
        })}
        confirmText={t("common.delete")}
        onConfirm={() => remove.mutate()}
        onCancel={() => setConfirmDelete(false)}
      />
    </div>
  );
}

export function PiNativePromptResources() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [slug, setSlug] = useState("");
  const [content, setContent] = useState("");
  const templates = useQuery({
    queryKey: ["pi", "promptTemplates"],
    queryFn: () => promptsApi.listPiPromptTemplates(),
  });
  const createTemplate = useMutation({
    mutationFn: () =>
      promptsApi.upsertPiPromptTemplate(slug.trim(), "missing", content),
    onSuccess: async () => {
      setSlug("");
      setContent("");
      await queryClient.invalidateQueries({
        queryKey: ["pi", "promptTemplates"],
      });
      toast.success(t("pi.prompts.templateCreated"));
    },
    onError: (error) =>
      mutationError(error, t("pi.prompts.templateSaveFailed")),
  });
  const refreshTemplates = () =>
    queryClient.invalidateQueries({ queryKey: ["pi", "promptTemplates"] });

  return (
    <section className="mb-5 space-y-4 rounded-xl border border-border bg-muted/20 p-4">
      <div>
        <h3 className="text-sm font-semibold">{t("pi.prompts.nativeTitle")}</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          {t("pi.prompts.nativeDescription")}
        </p>
      </div>

      <div className="grid gap-3 lg:grid-cols-2">
        {EDITABLE_FILES.map((file) => (
          <PiInstructionFileEditor key={file.kind} {...file} />
        ))}
      </div>

      <div className="rounded-lg border border-border bg-background/40 p-4">
        <div className="flex items-start justify-between gap-2">
          <div>
            <h4 className="text-sm font-medium">{t("pi.prompts.templates")}</h4>
            <p className="mt-1 text-xs text-muted-foreground">
              {t("pi.prompts.templatesDescription")}
            </p>
          </div>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            onClick={() => void templates.refetch()}
            disabled={templates.isFetching}
            title={t("common.refresh")}
          >
            <RefreshCw
              className={`h-4 w-4 ${
                templates.isFetching ? "animate-spin" : ""
              }`}
            />
          </Button>
        </div>

        <div className="mt-3 grid gap-3 lg:grid-cols-2">
          {(templates.data ?? []).map((template) => (
            <PiTemplateEditor
              key={template.slug}
              template={template}
              onChanged={() => void refreshTemplates()}
            />
          ))}
        </div>

        <div className="mt-4 rounded-lg border border-dashed p-3">
          <div className="mb-2 flex items-center gap-2 text-xs font-medium">
            <FilePlus2 className="h-4 w-4" />
            {t("pi.prompts.newTemplate")}
          </div>
          <Input
            value={slug}
            onChange={(event) => setSlug(event.target.value)}
            placeholder={t("pi.prompts.templateSlug")}
          />
          <Textarea
            value={content}
            onChange={(event) => setContent(event.target.value)}
            className="mt-2 min-h-24 font-mono text-xs"
            placeholder={t("pi.prompts.templateContent")}
            spellCheck={false}
          />
          <div className="mt-2 flex justify-end">
            <Button
              type="button"
              size="sm"
              onClick={() => createTemplate.mutate()}
              disabled={!slug.trim() || createTemplate.isPending}
            >
              {createTemplate.isPending && (
                <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
              )}
              {t("pi.prompts.createTemplate")}
            </Button>
          </div>
        </div>
      </div>
    </section>
  );
}
