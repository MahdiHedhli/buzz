//! Spawn bridge tests: legacy BUZZ_AGENT_THINKING_EFFORT → harness native key.
//!
//! Tests `resolve_effective_agent_env` with a Goose-shaped runtime fixture.
//! Included from `readiness.rs` via `#[path]`; `super::*` resolves in that module.

use std::collections::BTreeMap;

use super::*;
use crate::managed_agents::{
    config_bridge::LEGACY_THINKING_EFFORT_KEY, discovery::KnownAcpRuntime,
};

/// Minimal Goose-like runtime with native effort key and accepted-value set.
fn goose_rt() -> &'static KnownAcpRuntime {
    Box::leak(Box::new(KnownAcpRuntime {
        id: "goose",
        label: "Goose",
        commands: &["goose"],
        aliases: &[],
        avatar_url: "",
        mcp_command: None,
        mcp_hooks: false,
        underlying_cli: None,
        cli_install_commands: &[],
        cli_install_commands_windows: &[],
        adapter_install_commands: &[],
        cli_install_instructions_url: "",
        adapter_install_instructions_url: "",
        cli_install_hint: "",
        adapter_install_hint: "",
        skill_dir: None,
        supports_acp_model_switching: false,
        model_env_var: None,
        provider_env_var: None,
        provider_locked: false,
        default_env: &[],
        config_file_path: None,
        config_file_format: None,
        supports_acp_native_config: true,
        thinking_env_var: Some("GOOSE_THINKING_EFFORT"),
        accepted_effort_values: Some(&["none", "low", "medium", "high", "xhigh", "max"]),
        max_tokens_env_var: None,
        context_limit_env_var: None,
        required_normalized_fields: &[],
        login_hint: None,
        auth_probe_args: None,
    }))
}

/// Build a minimal `ManagedAgentRecord` with the given env_vars.
fn record_with(pairs: &[(&str, &str)]) -> crate::managed_agents::types::ManagedAgentRecord {
    let env_vars = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    crate::managed_agents::types::ManagedAgentRecord {
        pubkey: "p".into(),
        name: "n".into(),
        persona_id: None,
        private_key_nsec: String::new(),
        auth_tag: None,
        relay_url: String::new(),
        avatar_url: None,
        acp_command: "goose".into(),
        agent_command: "goose".into(),
        agent_command_override: None,
        agent_args: vec![],
        mcp_command: String::new(),
        turn_timeout_seconds: 60,
        idle_timeout_seconds: None,
        max_turn_duration_seconds: None,
        parallelism: 1,
        system_prompt: None,
        model: None,
        provider: None,
        persona_source_version: None,
        env_vars,
        start_on_app_launch: false,
        auto_restart_on_config_change: false,
        runtime_pid: None,
        backend: Default::default(),
        backend_agent_id: None,
        provider_binary_path: None,
        team_id: None,
        persona_team_dir: None,
        persona_name_in_team: None,
        created_at: String::new(),
        updated_at: String::new(),
        last_started_at: None,
        last_stopped_at: None,
        last_exit_code: None,
        last_error: None,
        last_error_code: None,
        respond_to: Default::default(),
        respond_to_allowlist: vec![],
        display_name: None,
        slug: None,
        runtime: None,
        name_pool: Vec::new(),
        is_builtin: false,
        is_active: true,
        shared: false,
        source_team: None,
        source_team_persona_slug: None,
        catalog_source: None,
        definition_respond_to: None,
        definition_respond_to_allowlist: Vec::new(),
        definition_parallelism: None,
        relay_mesh: None,
    }
}

fn spawn(
    record: &crate::managed_agents::types::ManagedAgentRecord,
    global: &crate::managed_agents::global_config::GlobalAgentConfig,
    personas: &[crate::managed_agents::types::AgentDefinition],
) -> BTreeMap<String, String> {
    resolve_effective_agent_env(record, personas, Some(goose_rt()), global).env
}

// ── Bridge: legacy-only record → native key appears ──────────────────────────

#[test]
fn legacy_only_record_env_bridges_to_native_key() {
    let record = record_with(&[(LEGACY_THINKING_EFFORT_KEY, "high")]);
    let env = spawn(&record, &Default::default(), &[]);
    assert_eq!(
        env.get("GOOSE_THINKING_EFFORT").map(String::as_str),
        Some("high"),
        "legacy record value must be bridged to native key"
    );
}

// ── Bridge: native key already present → no double-write ─────────────────────

#[test]
fn native_and_legacy_in_record_native_wins_no_double_write() {
    let record = record_with(&[
        ("GOOSE_THINKING_EFFORT", "medium"),
        (LEGACY_THINKING_EFFORT_KEY, "high"),
    ]);
    let env = spawn(&record, &Default::default(), &[]);
    assert_eq!(
        env.get("GOOSE_THINKING_EFFORT").map(String::as_str),
        Some("medium"),
        "native key must win over legacy when both present in record"
    );
}

// ── Bridge: invalid legacy value → key absent (harness default) ──────────────

#[test]
fn invalid_legacy_value_in_record_not_bridged() {
    let record = record_with(&[(LEGACY_THINKING_EFFORT_KEY, "minimal")]);
    let env = spawn(&record, &Default::default(), &[]);
    assert!(
        !env.contains_key("GOOSE_THINKING_EFFORT"),
        "invalid legacy value 'minimal' must not be bridged to native key"
    );
}

// ── Bridge: global-tier legacy NOT bridged ────────────────────────────────────

#[test]
fn global_legacy_effort_is_not_bridged_to_native_key() {
    let record = record_with(&[]); // no record-level effort
    let mut global = crate::managed_agents::global_config::GlobalAgentConfig::default();
    global
        .env_vars
        .insert(LEGACY_THINKING_EFFORT_KEY.to_string(), "high".to_string());
    let env = spawn(&record, &global, &[]);
    assert!(
        !env.contains_key("GOOSE_THINKING_EFFORT"),
        "global legacy effort must not be bridged (could be a buzz-agent default)"
    );
}

// ── Bridge: tier-first — record legacy beats persona native ──────────────────

#[test]
fn record_legacy_beats_persona_native_in_spawn() {
    let mut persona = crate::managed_agents::types::AgentDefinition {
        id: "p1".to_string(),
        display_name: "p1".to_string(),
        avatar_url: None,
        system_prompt: String::new(),
        runtime: None,
        model: None,
        provider: None,
        name_pool: vec![],
        is_builtin: false,
        is_active: true,
        shared: false,
        source_team: None,
        source_team_persona_slug: None,
        catalog_source: None,
        env_vars: std::collections::BTreeMap::new(),
        respond_to: None,
        respond_to_allowlist: vec![],
        parallelism: None,
        created_at: String::new(),
        updated_at: String::new(),
    };
    persona
        .env_vars
        .insert("GOOSE_THINKING_EFFORT".to_string(), "low".to_string());
    let mut record = record_with(&[(LEGACY_THINKING_EFFORT_KEY, "high")]);
    record.persona_id = Some("p1".to_string());
    let env = spawn(&record, &Default::default(), &[persona]);
    assert_eq!(
        env.get("GOOSE_THINKING_EFFORT").map(String::as_str),
        Some("high"),
        "record legacy (tier-first) must beat persona native"
    );
}

// Spawn-bridge tests live in this sibling file so readiness.rs stays within the
// desktop file-size ratchet.
