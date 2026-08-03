mod buzz_agent;
mod claude;
mod codex;
mod goose;
pub(crate) mod reader;
mod schema_walker;
pub(crate) mod types;

pub(crate) use types::*;

/// The legacy effort env key written by pre-migration saves.
///
/// Harnesses whose native `thinking_env_var` differs from this constant
/// (currently: Goose uses `GOOSE_THINKING_EFFORT`) need the alias resolver
/// below to translate old saves.  buzz-agent's native key equals this constant,
/// so no aliasing applies there.
pub(crate) const LEGACY_THINKING_EFFORT_KEY: &str = "BUZZ_AGENT_THINKING_EFFORT";

/// Resolve the thinking-effort value for a single env-var tier map, with
/// within-tier legacy aliasing.
///
/// Lookup order for each tier (applied independently per tier, not globally):
///   1. Native key (`native_key`) — returned as-is; UI never writes invalid values.
///   2. Legacy key (`BUZZ_AGENT_THINKING_EFFORT`) — honoured only when `native_key`
///      differs from the legacy key **and** the value is in `accepted`.
///      An invalid legacy value is skipped (yields `None` for this tier) so the
///      next tier can supply a valid candidate.
///
/// `global_tier` must be `true` for global-env maps: per the plan the legacy
/// key is excluded from the global tier to avoid attributing a Buzz-agent
/// default to a Goose intent.
pub(crate) fn effort_tier_alias(
    map: &std::collections::BTreeMap<String, String>,
    native_key: &str,
    accepted: &[&str],
    global_tier: bool,
) -> Option<String> {
    // Native key is always honoured (UI enforces the accepted-value set for new saves).
    if let Some(v) = map.get(native_key) {
        return Some(v.clone());
    }
    // Legacy alias — only when keys differ and this is not the global tier.
    if !global_tier && native_key != LEGACY_THINKING_EFFORT_KEY {
        if let Some(v) = map.get(LEGACY_THINKING_EFFORT_KEY) {
            if accepted.contains(&v.as_str()) {
                return Some(v.clone());
            }
            // Invalid legacy value for this harness: skip, fall through to next tier.
        }
    }
    None
}

/// Read the goose harness config file (`~/.config/goose/config.yaml`).
///
/// Used by readiness evaluation to silence requirements that are already
/// satisfied in the file config layer — the harness reads this file at startup
/// so env vars we would otherwise require are not needed from Buzz.
pub(crate) fn read_goose_file_config() -> Option<RuntimeFileConfig> {
    goose::read_config_file()
}
