import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { describe, expect, it, vi } from "vitest";

import { PiNativeCatalogPanel } from "@/components/providers/PiNativeCatalogPanel";
import type { Provider } from "@/types";
import { server } from "../msw/server";

const TAURI_ENDPOINT = "http://tauri.local";

function renderPanel(provider: Provider) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const view = render(
    <QueryClientProvider client={queryClient}>
      <PiNativeCatalogPanel
        providers={{ [provider.id]: provider }}
        onCreate={vi.fn()}
      />
    </QueryClientProvider>,
  );
  return { ...view, queryClient };
}

describe("PiNativeCatalogPanel", () => {
  it("does not present a pending current-state query as unconfigured", async () => {
    let releaseCurrentState: (() => void) | undefined;
    const pendingCurrentState = new Promise<void>((resolve) => {
      releaseCurrentState = resolve;
    });
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, async () => {
        await pendingCurrentState;
        return HttpResponse.json({
          ownership: "unconfigured",
          activeRoute: "unavailable",
          routeReason: "unconfigured",
        });
      }),
    );

    renderPanel({
      id: "managed",
      name: "Managed",
      settingsConfig: { models: [] },
    });

    expect(screen.getByText("common.loading")).toBeVisible();
    expect(screen.queryByText("common.notSet")).not.toBeInTheDocument();

    act(() => releaseCurrentState?.());
    expect(await screen.findAllByText("common.notSet")).not.toHaveLength(0);
  });

  it("keeps inspection capability separate from the actual active route", async () => {
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_native_defaults`, () =>
        HttpResponse.json({
          defaultProvider: "managed-native",
          defaultModel: "model",
        }),
      ),
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          providerKey: "managed-native",
          modelId: "model",
          managedProviderId: "managed",
          ownership: "managed",
          gatewayStatus: "proxyable",
          activeRoute: "direct",
          routeReason: "managed_direct",
        }),
      ),
      http.post(`${TAURI_ENDPOINT}/get_pi_native_catalog`, () =>
        HttpResponse.json([
          {
            providerKey: "managed-native",
            displayName: "Managed",
            fingerprint: "opaque",
            kind: "custom_catalog",
            rawValidity: "valid",
            managedAssessment: "manageable",
            compositionStatus: "composed",
            managementStatus: {
              status: "managed",
              providerId: "managed",
            },
            gatewayStatus: "proxyable",
            reasons: [],
          },
        ]),
      ),
    );

    renderPanel({
      id: "managed",
      name: "Managed",
      settingsConfig: { models: [{ id: "model", name: "Model" }] },
    });

    expect(await screen.findByText("pi.current.route.direct")).toBeVisible();
    expect(
      screen.queryByText(/pi\.native\.gateway\.proxyable/),
    ).not.toBeInTheDocument();

    fireEvent.click(await screen.findByText("pi.current.detected"));
    expect(
      await screen.findByText(/pi\.native\.gateway\.proxyable/),
    ).toBeVisible();
    expect(screen.getByText("pi.current.route.direct")).toBeVisible();
  });

  it("refreshes every Pi control-plane view from the same action", async () => {
    const { queryClient } = renderPanel({
      id: "managed",
      name: "Managed",
      settingsConfig: { models: [] },
    });
    const invalidate = vi.spyOn(queryClient, "invalidateQueries");

    const refresh = await screen.findByRole("button", {
      name: "common.refresh",
    });
    await waitFor(() => expect(refresh).toBeEnabled());
    fireEvent.click(refresh);

    await waitFor(() =>
      expect(invalidate).toHaveBeenCalledWith({
        queryKey: ["providers", "pi"],
      }),
    );
    expect(invalidate).toHaveBeenCalledWith({
      queryKey: ["failoverQueue", "pi"],
    });
    expect(invalidate).toHaveBeenCalledWith({
      queryKey: ["availableProvidersForFailover", "pi"],
    });
    expect(invalidate).toHaveBeenCalledWith({
      queryKey: ["autoFailoverEnabled", "pi"],
    });
  });

  it("leaves the managed default under queue control while failover is enabled", async () => {
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_auto_failover_enabled`, () =>
        HttpResponse.json(true),
      ),
    );
    renderPanel({
      id: "managed",
      name: "Managed",
      settingsConfig: { models: [{ id: "model", name: "Model" }] },
    });

    fireEvent.click(await screen.findByText("pi.current.changeManagedDefault"));

    expect(
      await screen.findByText("pi.current.failoverOwnsDefault"),
    ).toBeVisible();
    expect(screen.getByText("pi.native.setDefault")).toBeDisabled();
  });

  it("keeps an unverified Pi built-in selection out of managed failover", async () => {
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_auto_failover_enabled`, () =>
        HttpResponse.json(true),
      ),
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          providerKey: "anthropic",
          modelId: "claude",
          ownership: "pi_native",
          activeRoute: "direct",
          routeReason: "native_catalog_unavailable",
        }),
      ),
    );
    renderPanel({
      id: "managed",
      name: "Managed",
      settingsConfig: { models: [{ id: "model", name: "Model" }] },
    });

    expect(await screen.findByText("pi.current.route.direct")).toBeVisible();
    expect(
      screen.getByText("pi.current.reason.native_catalog_unavailable"),
    ).toBeVisible();
    expect(
      await screen.findByText("pi.current.failoverDirectOverride"),
    ).toBeVisible();
  });
});
