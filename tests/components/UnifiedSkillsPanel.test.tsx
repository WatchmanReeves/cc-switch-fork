import { createRef } from "react";
import { render, screen, waitFor, act } from "@testing-library/react";
import { describe, expect, it, vi, beforeEach } from "vitest";

import UnifiedSkillsPanel, {
  type UnifiedSkillsPanelHandle,
} from "@/components/skills/UnifiedSkillsPanel";

const scanUnmanagedMock = vi.fn();
const toggleSkillAppMock = vi.fn();
const uninstallSkillMock = vi.fn();
const importSkillsMock = vi.fn();
const installFromZipMock = vi.fn();
const deleteSkillBackupMock = vi.fn();
const restoreSkillBackupMock = vi.fn();
const skillsHookState = vi.hoisted(() => ({
  installed: [] as unknown[],
  piStatuses: {} as Record<string, unknown>,
  piStatusesLoading: false,
  piStatusesError: false,
}));

vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
    info: vi.fn(),
  },
}));

vi.mock("@/hooks/useSkills", () => ({
  useInstalledSkills: () => ({
    data: skillsHookState.installed,
    isLoading: false,
  }),
  usePiSkillStatuses: () => ({
    data: skillsHookState.piStatuses,
    isLoading: skillsHookState.piStatusesLoading,
    isError: skillsHookState.piStatusesError,
  }),
  useSkillBackups: () => ({
    data: [],
    refetch: vi.fn(),
    isFetching: false,
  }),
  useDeleteSkillBackup: () => ({
    mutateAsync: deleteSkillBackupMock,
    isPending: false,
  }),
  useToggleSkillApp: () => ({
    mutateAsync: toggleSkillAppMock,
  }),
  useRestoreSkillBackup: () => ({
    mutateAsync: restoreSkillBackupMock,
    isPending: false,
  }),
  useUninstallSkill: () => ({
    mutateAsync: uninstallSkillMock,
  }),
  useScanUnmanagedSkills: () => ({
    data: [
      {
        directory: "shared-skill",
        name: "Shared Skill",
        description: "Imported from Grok Build",
        foundIn: ["grokbuild"],
        path: "/tmp/shared-skill",
      },
    ],
    refetch: scanUnmanagedMock,
  }),
  useImportSkillsFromApps: () => ({
    mutateAsync: importSkillsMock,
  }),
  useInstallSkillsFromZip: () => ({
    mutateAsync: installFromZipMock,
  }),
  useCheckSkillUpdates: () => ({
    data: [],
    refetch: vi.fn(),
    isFetching: false,
  }),
  useUpdateSkill: () => ({
    mutateAsync: vi.fn(),
    isPending: false,
  }),
}));

describe("UnifiedSkillsPanel", () => {
  beforeEach(() => {
    scanUnmanagedMock.mockResolvedValue({
      data: [
        {
          directory: "shared-skill",
          name: "Shared Skill",
          description: "Imported from Grok Build",
          foundIn: ["grokbuild"],
          path: "/tmp/shared-skill",
        },
      ],
    });
    toggleSkillAppMock.mockReset();
    uninstallSkillMock.mockReset();
    importSkillsMock.mockReset();
    installFromZipMock.mockReset();
    deleteSkillBackupMock.mockReset();
    restoreSkillBackupMock.mockReset();
    skillsHookState.installed = [];
    skillsHookState.piStatuses = {};
    skillsHookState.piStatusesLoading = false;
    skillsHookState.piStatusesError = false;
  });

  it("opens the import dialog without crashing when app toggles render", async () => {
    const ref = createRef<UnifiedSkillsPanelHandle>();

    render(
      <UnifiedSkillsPanel
        ref={ref}
        onOpenDiscovery={() => {}}
        currentApp="claude"
      />,
    );

    await act(async () => {
      await ref.current?.openImport();
    });

    await waitFor(() => {
      expect(screen.getByText("skills.import")).toBeInTheDocument();
      expect(screen.getByText("Shared Skill")).toBeInTheDocument();
      expect(screen.getByText("/tmp/shared-skill")).toBeInTheDocument();
    });

    await act(async () => {
      screen.getByText("skills.importSelected").click();
    });

    await waitFor(() => {
      expect(importSkillsMock).toHaveBeenCalledWith([
        {
          directory: "shared-skill",
          apps: expect.objectContaining({ grokbuild: true }),
        },
      ]);
    });
  });

  it("renders Pi active state from inspection and toggles the desired state", async () => {
    skillsHookState.installed = [
      {
        id: "skill-1",
        name: "Pi Skill",
        directory: "pi-skill",
        apps: {
          claude: false,
          codex: false,
          gemini: false,
          opencode: false,
          openclaw: false,
          hermes: false,
          pi: true,
        },
        installedAt: 1,
        updatedAt: 1,
      },
    ];
    skillsHookState.piStatuses = {
      "skill-1": {
        desiredEnabled: true,
        ownedDeployment: false,
        effectivelyDiscovered: false,
        ownership: "foreign",
        discovery: "absent",
        issue: "collision",
      },
    };
    toggleSkillAppMock.mockResolvedValue(true);

    render(
      <UnifiedSkillsPanel
        onOpenDiscovery={() => {}}
        currentApp="pi"
      />,
    );

    expect(
      screen.getByText("Pi: skills.piStatus.foreignConflict"),
    ).toBeInTheDocument();
    const piToggle = screen.getByRole("button", { name: "Pi" });
    expect(piToggle).toHaveAttribute("aria-pressed", "false");

    await act(async () => {
      piToggle.click();
    });

    await waitFor(() => {
      expect(toggleSkillAppMock).toHaveBeenCalledWith({
        id: "skill-1",
        app: "pi",
        enabled: false,
      });
    });
  });
});
