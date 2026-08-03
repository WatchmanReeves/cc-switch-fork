import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import PromptPanel from "@/components/prompts/PromptPanel";

const mocks = vi.hoisted(() => ({
  reconcilePiLibrary: vi.fn(),
  reload: vi.fn(),
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
  }),
}));

vi.mock("@/hooks/useTauriEvent", () => ({
  useTauriEvent: vi.fn(),
}));

vi.mock("@/components/prompts/PiNativePromptResources", () => ({
  PiNativePromptResources: () => <div>native resources</div>,
}));

vi.mock("@/hooks/usePromptActions", () => ({
  usePromptActions: () => ({
    prompts: {},
    loading: false,
    currentFileContent: "external native content",
    piLibraryStatus: {
      nativeExists: true,
      nativeRevision: "native-revision",
      matchedPromptId: null,
      needsReconciliation: true,
    },
    reload: mocks.reload,
    savePrompt: vi.fn(),
    deletePrompt: vi.fn(),
    enablePrompt: vi.fn(),
    toggleEnabled: vi.fn(),
    importFromFile: vi.fn(),
    reconcilePiLibrary: mocks.reconcilePiLibrary,
  }),
}));

describe("PromptPanel Pi native reconciliation", () => {
  beforeEach(() => {
    mocks.reconcilePiLibrary.mockReset();
    mocks.reconcilePiLibrary.mockResolvedValue(undefined);
    mocks.reload.mockReset();
  });

  it("shows native drift and exposes the explicit reconciliation action", () => {
    render(<PromptPanel open appId="pi" onOpenChange={vi.fn()} />);

    expect(
      screen.getByText("pi.prompts.libraryDriftTitle"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("pi.prompts.libraryDriftNative"),
    ).toBeInTheDocument();

    fireEvent.click(
      screen.getByRole("button", { name: "pi.prompts.reconcileLibrary" }),
    );
    expect(mocks.reconcilePiLibrary).toHaveBeenCalledTimes(1);
  });
});
