import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, it, expect, vi, beforeEach } from "vitest";
import type { ReactElement } from "react";
import { http, HttpResponse } from "msw";
import type { Provider } from "@/types";
import { ProviderList } from "@/components/providers/ProviderList";
import { server } from "../msw/server";

const TAURI_ENDPOINT = "http://tauri.local";

const useDragSortMock = vi.fn();
const useSortableMock = vi.fn();
const providerCardRenderSpy = vi.fn();
const availableForFailoverQueryMock = vi.fn();

vi.mock("@/hooks/useDragSort", () => ({
  useDragSort: (...args: unknown[]) => useDragSortMock(...args),
}));

vi.mock("@/components/providers/ProviderCard", () => ({
  ProviderCard: (props: any) => {
    providerCardRenderSpy(props);
    const {
      provider,
      onSwitch,
      onEdit,
      onDelete,
      onDuplicate,
      onConfigureUsage,
    } = props;

    return (
      <div data-testid={`provider-card-${provider.id}`}>
        <button
          data-testid={`switch-${provider.id}`}
          onClick={() => onSwitch(provider)}
        >
          switch
        </button>
        <button
          data-testid={`edit-${provider.id}`}
          onClick={() => onEdit(provider)}
        >
          edit
        </button>
        <button
          data-testid={`duplicate-${provider.id}`}
          onClick={() => onDuplicate(provider)}
        >
          duplicate
        </button>
        <button
          data-testid={`usage-${provider.id}`}
          onClick={() => onConfigureUsage(provider)}
        >
          usage
        </button>
        <button
          data-testid={`delete-${provider.id}`}
          onClick={() => onDelete(provider)}
        >
          delete
        </button>
        <span data-testid={`is-current-${provider.id}`}>
          {props.isCurrent ? "current" : "inactive"}
        </span>
        <span data-testid={`drag-attr-${provider.id}`}>
          {props.dragHandleProps?.attributes?.["data-dnd-id"] ?? "none"}
        </span>
      </div>
    );
  },
  ProviderSummaryCard: (props: any) => {
    const summaryProps = {
      ...props,
      isCurrent: true,
      variant: "summary",
    };
    providerCardRenderSpy(summaryProps);
    return (
      <div data-testid={`provider-summary-${props.provider.id}`}>
        {props.provider.name}
      </div>
    );
  },
}));

vi.mock("@/components/UsageFooter", () => ({
  default: () => <div data-testid="usage-footer" />,
}));

vi.mock("@dnd-kit/sortable", async () => {
  const actual = await vi.importActual<any>("@dnd-kit/sortable");

  return {
    ...actual,
    useSortable: (...args: unknown[]) => useSortableMock(...args),
  };
});

// Mock hooks that use QueryClient
vi.mock("@/hooks/useStreamCheck", () => ({
  useStreamCheck: () => ({
    checkProvider: vi.fn(),
    isChecking: () => false,
  }),
}));

vi.mock("@/lib/query/failover", () => ({
  useAutoFailoverEnabled: () => ({ data: false }),
  useFailoverQueue: () => ({ data: [] }),
  useAvailableProvidersForFailover: (...args: unknown[]) =>
    availableForFailoverQueryMock(...args),
  useAddToFailoverQueue: () => ({ mutate: vi.fn() }),
  useRemoveFromFailoverQueue: () => ({ mutate: vi.fn() }),
  useReorderFailoverQueue: () => ({ mutate: vi.fn() }),
}));

function createProvider(overrides: Partial<Provider> = {}): Provider {
  return {
    id: overrides.id ?? "provider-1",
    name: overrides.name ?? "Test Provider",
    settingsConfig: overrides.settingsConfig ?? {},
    category: overrides.category,
    createdAt: overrides.createdAt,
    sortIndex: overrides.sortIndex,
    meta: overrides.meta,
    websiteUrl: overrides.websiteUrl,
  };
}

function renderWithQueryClient(ui: ReactElement) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });

  return render(
    <QueryClientProvider client={queryClient}>{ui}</QueryClientProvider>,
  );
}

beforeEach(() => {
  useDragSortMock.mockReset();
  useSortableMock.mockReset();
  providerCardRenderSpy.mockClear();
  availableForFailoverQueryMock.mockReset();
  availableForFailoverQueryMock.mockReturnValue({
    data: [],
    isFetching: false,
    isError: false,
  });

  useSortableMock.mockImplementation(({ id }: { id: string }) => ({
    setNodeRef: vi.fn(),
    attributes: { "data-dnd-id": id },
    listeners: { onPointerDown: vi.fn() },
    transform: null,
    transition: null,
    isDragging: false,
  }));

  useDragSortMock.mockReturnValue({
    sortedProviders: [],
    sensors: [],
    handleDragEnd: vi.fn(),
  });
});

describe("ProviderList Component", () => {
  it("should render skeleton placeholders when loading", () => {
    const { container } = renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="claude"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        isLoading
      />,
    );

    const placeholders = container.querySelectorAll(
      ".border-dashed.border-muted-foreground\\/40",
    );
    expect(placeholders).toHaveLength(3);
  });

  it("should show empty state and trigger create callback when no providers exist", () => {
    const handleCreate = vi.fn();
    useDragSortMock.mockReturnValueOnce({
      sortedProviders: [],
      sensors: [],
      handleDragEnd: vi.fn(),
    });

    renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="claude"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        onCreate={handleCreate}
      />,
    );

    const addButton = screen.getByRole("button", {
      name: "provider.addProvider",
    });
    fireEvent.click(addButton);

    expect(handleCreate).toHaveBeenCalledTimes(1);
  });

  it("should render in order returned by useDragSort and pass through action callbacks", () => {
    const providerA = createProvider({ id: "a", name: "A" });
    const providerB = createProvider({ id: "b", name: "B" });

    const handleSwitch = vi.fn();
    const handleEdit = vi.fn();
    const handleDelete = vi.fn();
    const handleDuplicate = vi.fn();
    const handleUsage = vi.fn();
    const handleOpenWebsite = vi.fn();

    useDragSortMock.mockReturnValue({
      sortedProviders: [providerB, providerA],
      sensors: [],
      handleDragEnd: vi.fn(),
    });

    renderWithQueryClient(
      <ProviderList
        providers={{ a: providerA, b: providerB }}
        currentProviderId="b"
        appId="claude"
        onSwitch={handleSwitch}
        onEdit={handleEdit}
        onDelete={handleDelete}
        onDuplicate={handleDuplicate}
        onConfigureUsage={handleUsage}
        onOpenWebsite={handleOpenWebsite}
      />,
    );

    // Verify sort order
    expect(providerCardRenderSpy).toHaveBeenCalledTimes(2);
    expect(providerCardRenderSpy.mock.calls[0][0].provider.id).toBe("b");
    expect(providerCardRenderSpy.mock.calls[1][0].provider.id).toBe("a");

    // Verify current provider marker
    expect(providerCardRenderSpy.mock.calls[0][0].isCurrent).toBe(true);

    // Drag attributes from useSortable
    expect(
      providerCardRenderSpy.mock.calls[0][0].dragHandleProps?.attributes[
        "data-dnd-id"
      ],
    ).toBe("b");
    expect(
      providerCardRenderSpy.mock.calls[1][0].dragHandleProps?.attributes[
        "data-dnd-id"
      ],
    ).toBe("a");

    // Trigger action buttons
    fireEvent.click(screen.getByTestId("switch-b"));
    fireEvent.click(screen.getByTestId("edit-b"));
    fireEvent.click(screen.getByTestId("duplicate-b"));
    fireEvent.click(screen.getByTestId("usage-b"));
    fireEvent.click(screen.getByTestId("delete-a"));

    expect(handleSwitch).toHaveBeenCalledWith(providerB);
    expect(handleEdit).toHaveBeenCalledWith(providerB);
    expect(handleDuplicate).toHaveBeenCalledWith(providerB);
    expect(handleUsage).toHaveBeenCalledWith(providerB);
    expect(handleDelete).toHaveBeenCalledWith(providerA);

    // Verify useDragSort call parameters
    expect(useDragSortMock).toHaveBeenCalledWith(
      { a: providerA, b: providerB },
      "claude",
    );
  });

  it("filters providers with the search input", () => {
    const providerAlpha = createProvider({ id: "alpha", name: "Alpha Labs" });
    const providerBeta = createProvider({ id: "beta", name: "Beta Works" });

    useDragSortMock.mockReturnValue({
      sortedProviders: [providerAlpha, providerBeta],
      sensors: [],
      handleDragEnd: vi.fn(),
    });

    renderWithQueryClient(
      <ProviderList
        providers={{ alpha: providerAlpha, beta: providerBeta }}
        currentProviderId=""
        appId="claude"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    fireEvent.keyDown(window, { key: "f", metaKey: true });
    const searchInput = screen.getByPlaceholderText(
      "Search name, notes, or URL...",
    );
    // Initially both providers are rendered
    expect(screen.getByTestId("provider-card-alpha")).toBeInTheDocument();
    expect(screen.getByTestId("provider-card-beta")).toBeInTheDocument();

    fireEvent.change(searchInput, { target: { value: "beta" } });
    expect(screen.queryByTestId("provider-card-alpha")).not.toBeInTheDocument();
    expect(screen.getByTestId("provider-card-beta")).toBeInTheDocument();

    fireEvent.change(searchInput, { target: { value: "gamma" } });
    expect(screen.queryByTestId("provider-card-alpha")).not.toBeInTheDocument();
    expect(screen.queryByTestId("provider-card-beta")).not.toBeInTheDocument();
    expect(
      screen.getByText("No providers match your search."),
    ).toBeInTheDocument();
  });

  it.each([
    { isFetching: true, isError: false },
    { isFetching: false, isError: true },
  ])(
    "fails Pi failover eligibility closed while cached capability is stale",
    ({ isFetching, isError }) => {
      const provider = createProvider({ id: "pi-provider" });
      availableForFailoverQueryMock.mockReturnValue({
        data: [provider],
        isFetching,
        isError,
      });
      useDragSortMock.mockReturnValue({
        sortedProviders: [provider],
        sensors: [],
        handleDragEnd: vi.fn(),
      });

      renderWithQueryClient(
        <ProviderList
          providers={{ [provider.id]: provider }}
          currentProviderId=""
          appId="pi"
          onSwitch={vi.fn()}
          onEdit={vi.fn()}
          onDelete={vi.fn()}
          onDuplicate={vi.fn()}
          onOpenWebsite={vi.fn()}
        />,
      );

      expect(providerCardRenderSpy).toHaveBeenCalledWith(
        expect.objectContaining({ isFailoverEligible: false }),
      );
    },
  );

  it("renders a Pi-native current selection through the shared summary card", async () => {
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          providerKey: "anthropic",
          modelId: "claude-opus",
          ownership: "pi_native",
          activeRoute: "direct",
          routeReason: "native_direct",
        }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        onCreate={vi.fn()}
      />,
    );

    await waitFor(() =>
      expect(providerCardRenderSpy).toHaveBeenCalledWith(
        expect.objectContaining({
          variant: "summary",
          provider: expect.objectContaining({
            name: "anthropic",
            icon: "pi",
          }),
          statusBadges: expect.arrayContaining([
            expect.objectContaining({ label: "provider.inUse" }),
            expect.objectContaining({
              label: "provider.noRoutingSupport",
            }),
          ]),
        }),
      ),
    );
    expect(
      screen.queryByRole("button", { name: "provider.addProvider" }),
    ).not.toBeInTheDocument();
  });

  it("does not translate Pi gateway capability into a false routing requirement", async () => {
    const provider = createProvider({
      id: "managed-pi",
      name: "Managed Pi",
    });
    useDragSortMock.mockReturnValue({
      sortedProviders: [provider],
      sensors: [],
      handleDragEnd: vi.fn(),
    });
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          providerKey: "managed-pi",
          modelId: "managed-model",
          managedProviderId: "managed-pi",
          ownership: "managed",
          gatewayStatus: "proxyable",
          activeRoute: "gateway",
          routeReason: "managed_gateway",
        }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{ [provider.id]: provider }}
        currentProviderId="managed-pi"
        appId="pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    await waitFor(() => {
      const currentCard = providerCardRenderSpy.mock.calls
        .map(([props]) => props)
        .find((props) => props.provider.id === "managed-pi");
      expect(currentCard).toMatchObject({
        isCurrent: true,
        statusBadges: undefined,
      });
      expect(currentCard).not.toHaveProperty("subtitle");
    });
  });

  it("shows the import action only for an importable external Pi current selection", async () => {
    let imports = 0;
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          providerKey: "external-provider",
          modelId: "external-model",
          ownership: "external",
          activeRoute: "direct",
          routeReason: "native_direct",
        }),
      ),
      http.post(`${TAURI_ENDPOINT}/get_pi_native_catalog`, () =>
        HttpResponse.json([
          {
            providerKey: "external-provider",
            displayName: "External Provider",
            fingerprint: "opaque-fingerprint",
            kind: "custom_catalog",
            rawValidity: "valid",
            managedAssessment: "manageable",
            compositionStatus: "composed",
            managementStatus: { status: "importable" },
            gatewayStatus: "proxyable",
            reasons: [],
          },
        ]),
      ),
      http.post(`${TAURI_ENDPOINT}/import_pi_native_provider`, () => {
        imports += 1;
        return HttpResponse.json("external-provider");
      }),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        onCreate={vi.fn()}
      />,
    );

    fireEvent.click(
      await screen.findByRole("button", { name: "provider.importCurrent" }),
    );
    await waitFor(() => expect(imports).toBe(1));
    expect(
      screen.queryByRole("button", { name: "provider.addProvider" }),
    ).not.toBeInTheDocument();
  });
});
