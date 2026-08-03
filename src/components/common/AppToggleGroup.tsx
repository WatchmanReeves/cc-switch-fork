import React from "react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import type { AppId } from "@/lib/api/types";
import { APP_IDS, APP_ICON_MAP } from "@/config/appConfig";

interface AppToggleGroupProps {
  apps: Partial<Record<AppId, boolean>>;
  onToggle: (app: AppId, enabled: boolean) => void;
  appIds?: AppId[];
  stateByApp?: Partial<Record<AppId, AppToggleVisualState>>;
}

export interface AppToggleVisualState {
  /** 应用实际是否发现/启用了资源。 */
  active: boolean;
  /** 用户期望状态；点击切换时以此取反。 */
  desired: boolean;
  statusLabel?: string;
  warning?: boolean;
}

export const AppToggleGroup: React.FC<AppToggleGroupProps> = ({
  apps,
  onToggle,
  appIds = APP_IDS,
  stateByApp,
}) => {
  return (
    <div className="flex items-center gap-1.5 flex-shrink-0">
      {appIds.map((app) => {
        const { label, icon, activeClass } = APP_ICON_MAP[app];
        const visualState = stateByApp?.[app];
        const desired = visualState?.desired ?? Boolean(apps[app]);
        const active = visualState?.active ?? desired;
        const warning =
          visualState?.warning ?? (visualState ? active !== desired : false);
        return (
          <Tooltip key={app}>
            <TooltipTrigger asChild>
              <button
                type="button"
                aria-label={label}
                aria-pressed={active}
                onClick={() => onToggle(app, !desired)}
                className={`w-7 h-7 rounded-lg flex items-center justify-center transition-all ${
                  active ? activeClass : "opacity-35 hover:opacity-70"
                } ${warning ? "ring-2 ring-amber-500 ring-offset-1 ring-offset-background" : ""}`}
              >
                {icon}
              </button>
            </TooltipTrigger>
            <TooltipContent side="bottom">
              <p>
                {label}
                {active ? " ✓" : ""}
              </p>
              {visualState?.statusLabel && (
                <p className="max-w-64 text-xs text-muted-foreground">
                  {visualState.statusLabel}
                </p>
              )}
            </TooltipContent>
          </Tooltip>
        );
      })}
    </div>
  );
};
