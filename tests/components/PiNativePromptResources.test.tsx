import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { PiNativePromptResources } from "@/components/prompts/PiNativePromptResources";
import { promptsApi, type PiPromptFileKind } from "@/lib/api/prompts";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
  }),
}));

vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

const renderResources = () => {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });
  return render(
    <QueryClientProvider client={client}>
      <PiNativePromptResources />
    </QueryClientProvider>,
  );
};

describe("PiNativePromptResources", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(promptsApi, "getPiPromptFile").mockImplementation(
      async (kind: PiPromptFileKind) => ({
        kind,
        path:
          kind === "system_override"
            ? "/agent/SYSTEM.md"
            : "/agent/APPEND_SYSTEM.md",
        exists: kind === "system_append",
        revision: kind === "system_append" ? "append-revision" : "missing",
        content: kind === "system_append" ? "append" : "",
      }),
    );
    vi.spyOn(promptsApi, "listPiPromptTemplates").mockResolvedValue([
      {
        slug: "empty",
        content: "",
        revision: "empty-revision",
      },
    ]);
    vi.spyOn(promptsApi, "upsertPiPromptTemplate").mockResolvedValue({
      slug: "new-empty",
      content: "",
      revision: "created-revision",
    });
    vi.spyOn(promptsApi, "replacePiPromptFile").mockImplementation(
      async (kind, _revision, content) => ({
        kind,
        path: "/agent/SYSTEM.md",
        exists: true,
        revision: "saved-empty",
        content,
      }),
    );
  });

  it("uses file presence as state, rejects blank direct saves, and permits empty templates", async () => {
    renderResources();

    await waitFor(() => expect(screen.getByText("/empty")).toBeInTheDocument());
    expect(screen.getByText("pi.prompts.active")).toBeInTheDocument();
    expect(screen.getByText("pi.prompts.inactive")).toBeInTheDocument();

    const instructionEditors = screen.getAllByPlaceholderText(
      "pi.prompts.instructionPlaceholder",
    );
    fireEvent.change(instructionEditors[1], { target: { value: "" } });
    const saveButtons = screen.getAllByRole("button", { name: "common.save" });
    expect(saveButtons[1]).toBeDisabled();
    fireEvent.change(instructionEditors[1], {
      target: { value: "new append" },
    });
    expect(saveButtons[1]).toBeEnabled();
    fireEvent.click(saveButtons[1]);
    await waitFor(() =>
      expect(promptsApi.replacePiPromptFile).toHaveBeenCalledWith(
        "system_append",
        "append-revision",
        "new append",
      ),
    );

    fireEvent.change(screen.getByPlaceholderText("pi.prompts.templateSlug"), {
      target: { value: "new-empty" },
    });
    const create = screen.getByRole("button", {
      name: "pi.prompts.createTemplate",
    });
    expect(create).toBeEnabled();
    fireEvent.click(create);

    await waitFor(() =>
      expect(promptsApi.upsertPiPromptTemplate).toHaveBeenCalledWith(
        "new-empty",
        "missing",
        "",
      ),
    );
  });

  it("requires confirmation before creating the dangerous SYSTEM override", async () => {
    renderResources();

    const instructionEditors = await screen.findAllByPlaceholderText(
      "pi.prompts.instructionPlaceholder",
    );
    fireEvent.change(instructionEditors[0], {
      target: { value: "replace the system prompt" },
    });
    fireEvent.click(
      screen.getAllByRole("button", { name: "common.save" })[0],
    );

    expect(promptsApi.replacePiPromptFile).not.toHaveBeenCalled();
    expect(
      screen.getByText("pi.prompts.activateOverrideTitle"),
    ).toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("button", { name: "pi.prompts.activateOverride" }),
    );
    await waitFor(() =>
      expect(promptsApi.replacePiPromptFile).toHaveBeenCalledWith(
        "system_override",
        "missing",
        "replace the system prompt",
      ),
    );
  });
});
