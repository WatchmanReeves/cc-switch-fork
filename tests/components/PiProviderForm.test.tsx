import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { PiProviderForm } from "@/components/providers/forms/PiProviderForm";
import composerOracle from "../fixtures/pi/native-oracle/composer-oracle-v1.json";
import { http, HttpResponse } from "msw";
import { server } from "../msw/server";

const TAURI_ENDPOINT = "http://tauri.local";

describe("PiProviderForm", () => {
  it("applies a maintained preset without creating a Pi-owned provider key", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <PiProviderForm
        appId="pi"
        submitLabel="Save preset"
        onSubmit={onSubmit}
        onCancel={() => {}}
      />,
    );

    fireEvent.click(screen.getByText("Kimi", { selector: "span" }));
    fireEvent.change(screen.getByLabelText("pi.form.credential"), {
      target: { value: "literal-key" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save preset" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0]).toMatchObject({
      providerKey: "cc-switch-kimi",
      name: "Kimi",
      presetCategory: "cn_official",
    });
    expect(JSON.parse(onSubmit.mock.calls[0][0].settingsConfig)).toMatchObject({
      api: "openai-completions",
      baseUrl: "https://api.moonshot.cn/v1",
      apiKey: "literal-key",
    });
  });

  it("submits only explicit model fields and leaves pinned defaults to Pi", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);

    render(
      <PiProviderForm
        appId="pi"
        submitLabel="Save Pi provider"
        onSubmit={onSubmit}
        onCancel={() => {}}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "providerPreset.custom" }),
    );
    fireEvent.change(screen.getByPlaceholderText("my-provider"), {
      target: { value: "verified-provider" },
    });
    fireEvent.change(screen.getByPlaceholderText("My Pi provider"), {
      target: { value: "Verified provider" },
    });
    fireEvent.change(screen.getByPlaceholderText("openai-responses"), {
      target: { value: "openai-responses" },
    });
    fireEvent.change(
      screen.getByPlaceholderText("https://api.example.com/v1"),
      {
        target: { value: "https://api.example.com/v1" },
      },
    );
    fireEvent.change(screen.getByPlaceholderText("model-id"), {
      target: { value: "opaque-model" },
    });

    fireEvent.click(screen.getByRole("button", { name: "Save Pi provider" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    const submitted = onSubmit.mock.calls[0][0];
    expect(submitted.providerKey).toBe("verified-provider");
    expect(JSON.parse(submitted.settingsConfig)).toEqual({
      name: "Verified provider",
      api: "openai-responses",
      baseUrl: "https://api.example.com/v1",
      models: [{ id: "opaque-model" }],
    });
  });

  it("round-trips every field in the pinned all-fields composer vector", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const vector = composerOracle.cases.find(
      (candidate) => candidate.id === "combined-all-fields-precedence",
    );
    expect(vector).toBeDefined();
    const input = JSON.parse(JSON.stringify(vector?.input)) as Record<
      string,
      unknown
    >;

    render(
      <PiProviderForm
        appId="pi"
        providerId="all-fields"
        submitLabel="Save all fields"
        onSubmit={onSubmit}
        onCancel={() => {}}
        initialData={{
          name: String(input.name),
          settingsConfig: input,
        }}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Save all fields" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(JSON.parse(onSubmit.mock.calls[0][0].settingsConfig)).toEqual(input);
  });

  it("preserves pinned exact model IDs instead of trimming them", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const input = {
      name: "Exact IDs",
      api: "openai-responses",
      baseUrl: "https://api.example.com/v1",
      models: [{ id: " " }, { id: " model " }],
    };

    render(
      <PiProviderForm
        appId="pi"
        providerId="exact-ids"
        submitLabel="Save exact IDs"
        onSubmit={onSubmit}
        onCancel={() => {}}
        initialData={{ name: input.name, settingsConfig: input }}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Save exact IDs" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(JSON.parse(onSubmit.mock.calls[0][0].settingsConfig)).toEqual(input);
  });

  it("preserves an explicitly false authHeader instead of erasing it", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const input = {
      name: "Explicit false",
      api: "openai-responses",
      baseUrl: "https://api.example.com/v1",
      authHeader: false,
      models: [{ id: "model" }],
    };

    render(
      <PiProviderForm
        appId="pi"
        providerId="explicit-false"
        submitLabel="Save explicit false"
        onSubmit={onSubmit}
        onCancel={() => {}}
        initialData={{ name: input.name, settingsConfig: input }}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Save explicit false" }),
    );
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(JSON.parse(onSubmit.mock.calls[0][0].settingsConfig)).toEqual(input);
  });

  it("creates and submits typed failover endpoints from the real form entry", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);

    render(
      <PiProviderForm
        appId="pi"
        submitLabel="Save endpoint provider"
        onSubmit={onSubmit}
        onCancel={() => {}}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "providerPreset.custom" }),
    );
    fireEvent.change(screen.getByPlaceholderText("my-provider"), {
      target: { value: "endpoint-provider" },
    });
    fireEvent.change(screen.getByPlaceholderText("My Pi provider"), {
      target: { value: "Endpoint provider" },
    });
    fireEvent.change(screen.getByPlaceholderText("openai-responses"), {
      target: { value: "openai-responses" },
    });
    fireEvent.change(screen.getByPlaceholderText("model-id"), {
      target: { value: "endpoint-model" },
    });

    fireEvent.click(
      screen.getByRole("button", { name: "pi.form.manageEndpoints" }),
    );
    const endpointInput = await screen.findByPlaceholderText(
      "endpointTest.addEndpointPlaceholder",
    );
    fireEvent.change(endpointInput, {
      target: { value: "https://failover.example/v1" },
    });
    fireEvent.keyDown(endpointInput, { key: "Enter" });
    fireEvent.click(screen.getByRole("button", { name: "common.save" }));
    fireEvent.click(
      screen.getByRole("button", { name: "Save endpoint provider" }),
    );

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(JSON.parse(onSubmit.mock.calls[0][0].settingsConfig)).toMatchObject({
      baseUrl: "https://failover.example/v1",
      models: [{ id: "endpoint-model" }],
    });
    expect(onSubmit.mock.calls[0][0].meta.custom_endpoints).toEqual({
      "https://failover.example/v1": expect.objectContaining({
        url: "https://failover.example/v1",
      }),
    });
  });

  it("treats persisted endpoint membership as authoritative in edit mode", async () => {
    let endpointReads = 0;
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_custom_endpoints`, () => {
        endpointReads += 1;
        return HttpResponse.json([]);
      }),
    );

    render(
      <PiProviderForm
        appId="pi"
        providerId="managed"
        submitLabel="Save managed provider"
        onSubmit={vi.fn()}
        onCancel={() => {}}
        initialData={{
          name: "Managed",
          settingsConfig: {
            api: "openai-responses",
            baseUrl: "https://api.example.com/v1",
            models: [{ id: "model" }],
          },
          meta: {
            custom_endpoints: {
              "https://removed.example/v1": {
                url: "https://removed.example/v1",
                addedAt: null,
              },
            },
          },
        }}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "pi.form.manageEndpoints" }),
    );

    await waitFor(() =>
      expect(
        screen.queryByText("https://removed.example/v1"),
      ).not.toBeInTheDocument(),
    );
    expect(endpointReads).toBe(1);

    fireEvent.click(screen.getByRole("button", { name: "Done" }));
    fireEvent.click(
      screen.getByRole("button", { name: "pi.form.manageEndpoints" }),
    );
    await waitFor(() => expect(endpointReads).toBe(2));
    expect(
      screen.queryByText("https://removed.example/v1"),
    ).not.toBeInTheDocument();
  });
});
