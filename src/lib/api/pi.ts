import { invoke } from "@tauri-apps/api/core";

export type PiRawValidity = "valid" | "invalid" | "unknown";
export type PiManagedAssessment = "manageable" | "unsupported";
export type PiCompositionStatus = "composed" | "failed" | "unknown";
export type PiGatewayStatus = "proxyable" | "direct_only" | "unknown";

export type PiManagementStatus =
  | { status: "importable" }
  | { status: "managed"; providerId: string }
  | { status: "unsupported" };

export interface PiDiagnosticReason {
  layer: "raw_schema" | "managed" | "composition" | "gateway";
  code: string;
  jsonPointer?: string;
}

export interface PiNativeDiagnostic {
  providerKey: string;
  displayName?: string;
  fingerprint: string;
  kind: string;
  rawValidity: PiRawValidity;
  managedAssessment: PiManagedAssessment;
  compositionStatus: PiCompositionStatus;
  managementStatus: PiManagementStatus;
  gatewayStatus: PiGatewayStatus;
  reasons: PiDiagnosticReason[];
}

export interface PiNativeDefaults {
  defaultProvider?: string;
  defaultModel?: string;
  sessionDir?: string;
}

export type PiCurrentOwnership =
  | "managed"
  | "pi_native"
  | "external"
  | "unconfigured";
export type PiActiveRoute = "gateway" | "direct" | "unavailable";

export interface PiCurrentState {
  providerKey?: string;
  modelId?: string;
  managedProviderId?: string;
  ownership: PiCurrentOwnership;
  gatewayStatus?: PiGatewayStatus;
  activeRoute: PiActiveRoute;
  routeReason:
    | "unconfigured"
    | "native_direct"
    | "managed_gateway"
    | "managed_direct"
    | "managed_projection_mismatch"
    | "failover_primary_mismatch"
    | "native_catalog_unavailable"
    | "selection_unavailable";
}

export type PiSessionDiscovery =
  | {
      status: "available";
      root: string;
      source: "environment" | "settings" | "default";
    }
  | {
      status: "requires_project_context";
      configuredPath: string;
      source: "environment" | "settings";
    }
  | {
      status: "unavailable";
      reason: string;
    };

export const piApi = {
  async getNativeCatalog(): Promise<PiNativeDiagnostic[]> {
    return await invoke("get_pi_native_catalog");
  },

  async importNativeProvider(
    providerKey: string,
    expectedFingerprint: string,
  ): Promise<string> {
    return await invoke("import_pi_native_provider", {
      providerKey,
      expectedFingerprint,
    });
  },

  async getNativeDefaults(): Promise<PiNativeDefaults> {
    return await invoke("get_pi_native_defaults");
  },

  async getCurrentState(): Promise<PiCurrentState> {
    return await invoke("get_pi_current_state");
  },

  async getSessionDiscovery(): Promise<PiSessionDiscovery> {
    return await invoke("get_pi_session_discovery");
  },

  async setDefaultModel(providerId: string, modelId: string): Promise<boolean> {
    return await invoke("set_pi_default_model", { providerId, modelId });
  },

  async resetGatewayCredential(): Promise<boolean> {
    return await invoke("reset_pi_gateway_credential");
  },
};
