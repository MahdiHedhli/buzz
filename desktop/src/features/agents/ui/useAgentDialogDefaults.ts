import * as React from "react";

import { useBakedBuildEnvQuery } from "../hooks";
import { useGlobalAgentConfig } from "../useGlobalAgentConfig";
import { BUZZ_AGENT_THINKING_EFFORT } from "./buzzAgentConfig";
import { getInheritedAgentDefaults } from "./bakedEnvHelpers";

export function useAgentDialogDefaults({
  inheritedEnvVars = {},
  open,
  nativeThinkingKey,
}: {
  inheritedEnvVars?: Record<string, string>;
  open: boolean;
  /** The current runtime's native thinking-effort env key (e.g.
   * `GOOSE_THINKING_EFFORT`).  When omitted or equal to the legacy key the
   * inherited-effort value is injected under `BUZZ_AGENT_THINKING_EFFORT` as
   * before; otherwise it is injected under the native key so
   * `AgentConfigFields` reads it via `effortPersistenceKey`. */
  nativeThinkingKey?: string | null;
}) {
  const { globalConfig } = useGlobalAgentConfig();
  const { data: bakedEnv } = useBakedBuildEnvQuery({ enabled: open });
  const inheritedDefaults = getInheritedAgentDefaults(globalConfig, bakedEnv);
  const effortKey =
    nativeThinkingKey && nativeThinkingKey !== BUZZ_AGENT_THINKING_EFFORT
      ? nativeThinkingKey
      : BUZZ_AGENT_THINKING_EFFORT;
  const effectiveInheritedEnvVars = React.useMemo(
    () => ({
      ...globalConfig.env_vars,
      ...inheritedEnvVars,
      ...(inheritedDefaults.effort.value
        ? { [effortKey]: inheritedDefaults.effort.value }
        : {}),
    }),
    [
      globalConfig.env_vars,
      inheritedDefaults.effort.value,
      inheritedEnvVars,
      effortKey,
    ],
  );
  return {
    globalConfig,
    inheritedDefaults,
    inheritedEnvVars: effectiveInheritedEnvVars,
  };
}
