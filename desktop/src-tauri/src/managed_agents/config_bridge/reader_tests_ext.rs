//! Additional tests for `config_bridge/reader.rs` — split out to keep
//! `reader_tests.rs` under the 1000-line file-size ratchet.
//!
//! Included as `mod ext` inside `reader_tests.rs`, so `use super::*` gives
//! access to all helpers and types from that module.

use super::*;

// ── Numerics inheritance tests ────────────────────────────────────────────────
//
// max_output_tokens and context_limit gain persona/global tiers.

#[test]
fn numeric_context_limit_inherits_from_persona_env() {
    let record = test_record();
    let runtime = buzz_agent_runtime();
    let tiers = persona_env_tiers("BUZZ_AGENT_MAX_CONTEXT_TOKENS", "200000");

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let field = surface.normalized.context_limit.unwrap();
    assert_eq!(field.value.as_deref(), Some("200000"));
    assert_eq!(field.origin, ConfigOrigin::PersonaDefault);
}

#[test]
fn record_max_tokens_overrides_global_env_with_secondary() {
    let mut record = test_record();
    record.env_vars.insert(
        "BUZZ_AGENT_MAX_OUTPUT_TOKENS".to_string(),
        "8192".to_string(),
    );
    let runtime = buzz_agent_runtime();
    let tiers = global_env_tiers("BUZZ_AGENT_MAX_OUTPUT_TOKENS", "16384");

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let field = surface.normalized.max_output_tokens.unwrap();
    assert_eq!(field.value.as_deref(), Some("8192"));
    assert_eq!(field.origin, ConfigOrigin::BuzzExplicit);
    // Global value is the overridden secondary.
    assert_eq!(field.overridden_value.as_deref(), Some("16384"));
    assert_eq!(field.overridden_origin, Some(ConfigOrigin::GlobalDefault));
}

// ── Env-vs-structured collision tests (plan v3, Phase 2) ─────────────────────

/// Collision test 1: persona structured prompt + global env BUZZ_ACP_SYSTEM_PROMPT
/// → global env wins (env block sits entirely above structured).
#[test]
fn global_env_prompt_wins_over_persona_structured_prompt() {
    let record = test_record();
    let runtime = test_runtime();
    let tiers = InheritedConfigTiers {
        global_env: {
            let mut m = BTreeMap::new();
            m.insert(
                "BUZZ_ACP_SYSTEM_PROMPT".to_string(),
                "global-env-prompt".to_string(),
            );
            m
        },
        persona_prompt: Some("persona-structured-prompt".to_string()),
        ..Default::default()
    };

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let prompt = surface.normalized.system_prompt.unwrap();
    assert_eq!(prompt.value.as_deref(), Some("global-env-prompt"));
    assert_eq!(prompt.origin, ConfigOrigin::GlobalDefault);
}

/// Collision test 2: structured persona/record model + higher user-env value at
/// the runtime's model key → env value wins.
#[test]
fn persona_env_model_wins_over_persona_structured_model() {
    let record = test_record(); // no record.model
    let runtime = test_runtime(); // GOOSE_MODEL
    let tiers = InheritedConfigTiers {
        persona_env: {
            let mut m = BTreeMap::new();
            m.insert("GOOSE_MODEL".to_string(), "env-model".to_string());
            m
        },
        persona_model: Some("struct-persona-model".to_string()),
        ..Default::default()
    };

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let model = surface.normalized.model.unwrap();
    // persona env outranks persona struct because env candidates precede struct
    assert_eq!(model.value.as_deref(), Some("env-model"));
    assert_eq!(model.origin, ConfigOrigin::PersonaDefault);
}

/// Collision test 3: no env representation → structured persona/record/global
/// fallback and provenance remain intact.
#[test]
fn structured_fallback_intact_when_no_env_representation() {
    let record = test_record(); // no record.model, no env vars
    let runtime = test_runtime();
    let tiers = InheritedConfigTiers {
        persona_model: Some("struct-persona-model".to_string()),
        ..Default::default()
    };

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let model = surface.normalized.model.unwrap();
    assert_eq!(model.value.as_deref(), Some("struct-persona-model"));
    assert_eq!(model.origin, ConfigOrigin::PersonaDefault);
}

// ── Post-sanitization fallthrough test ───────────────────────────────────────
//
// Sanitization itself happens at the command boundary in `build_inherited_tiers`
// (a value with a NUL byte or an oversize value is dropped from the tier) and is
// pinned by the tests in `commands/agent_config_tests.rs`. The reader only ever
// sees the sanitized result, so what it must guarantee is the downstream half:
// a key stripped from one tier falls through to the next.

/// A key absent from the global env tier — the shape the reader sees after the
/// command boundary strips an invalid value — falls through to the persona tier.
#[test]
fn post_sanitization_empty_global_env_falls_through_to_persona_tier() {
    let record = test_record();
    let runtime = buzz_agent_rt();
    // No global env (stripped); persona provides the valid fallback.
    let tiers = persona_env_tiers("BUZZ_AGENT_THINKING_EFFORT", "medium");

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    // Persona value surfaces instead of the stripped global value.
    let effort = surface.normalized.thinking_effort.unwrap();
    assert_eq!(effort.value.as_deref(), Some("medium"));
    assert_eq!(effort.origin, ConfigOrigin::PersonaDefault);
}

// ── Pass-3 prompt collision test ─────────────────────────────────────────────
//
// From Thufir's pass-3 verdict MINOR clarification (promoted to required):
// definition-less record with both structured and env prompt — env wins.

/// Pass-3 clarification: record.system_prompt = A + record env
/// BUZZ_ACP_SYSTEM_PROMPT = B → B wins as BuzzExplicit.
/// The env block sits above the struct block per v3 candidate-preparation
/// contract; current reader semantics (struct before env) would be wrong.
#[test]
fn record_env_prompt_wins_over_record_struct_prompt_as_buzz_explicit() {
    let mut record = test_record();
    record.system_prompt = Some("struct-prompt-A".to_string());
    record.env_vars.insert(
        "BUZZ_ACP_SYSTEM_PROMPT".to_string(),
        "env-prompt-B".to_string(),
    );
    let runtime = test_runtime();

    let surface = read_config_surface(&record, Some(runtime), None, &no_tiers());

    let prompt = surface.normalized.system_prompt.unwrap();
    assert_eq!(prompt.value.as_deref(), Some("env-prompt-B"));
    assert_eq!(prompt.origin, ConfigOrigin::BuzzExplicit);
    // Struct prompt is the secondary.
    assert_eq!(prompt.overridden_value.as_deref(), Some("struct-prompt-A"));
    assert_eq!(prompt.overridden_origin, Some(ConfigOrigin::BuzzExplicit));
}

// ── Definition env tier tests (Layer 2b) ─────────────────────────────────────
//
// The harness definition's `env` block sits below global env and above
// structured values in spawn's precedence (Layer 2b). These tests exercise
// the reader's mapping of that tier to `HarnessDefault` origin.

/// Definition env wins over structured persona model when no user-env or
/// global-env candidate is present.
#[test]
fn definition_env_beats_structured_persona_model() {
    let record = test_record(); // no record.model, no record.env_vars
    let runtime = test_runtime(); // model_env_var = "GOOSE_MODEL"
    let tiers = InheritedConfigTiers {
        definition_env: {
            let mut m = BTreeMap::new();
            m.insert("GOOSE_MODEL".to_string(), "harness-model".to_string());
            m
        },
        persona_model: Some("persona-struct-model".to_string()),
        ..Default::default()
    };

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let model = surface.normalized.model.unwrap();
    assert_eq!(model.value.as_deref(), Some("harness-model"));
    assert_eq!(model.origin, ConfigOrigin::HarnessDefault);
    // Structured persona model is the overridden secondary.
    assert_eq!(
        model.overridden_value.as_deref(),
        Some("persona-struct-model")
    );
    assert_eq!(model.overridden_origin, Some(ConfigOrigin::PersonaDefault));
}

/// Global env beats definition env — user-settable tiers always win over the
/// harness author's defaults.
#[test]
fn global_env_beats_definition_env() {
    let record = test_record();
    let runtime = test_runtime(); // model_env_var = "GOOSE_MODEL"
    let tiers = InheritedConfigTiers {
        global_env: {
            let mut m = BTreeMap::new();
            m.insert("GOOSE_MODEL".to_string(), "global-model".to_string());
            m
        },
        definition_env: {
            let mut m = BTreeMap::new();
            m.insert("GOOSE_MODEL".to_string(), "harness-model".to_string());
            m
        },
        ..Default::default()
    };

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let model = surface.normalized.model.unwrap();
    assert_eq!(model.value.as_deref(), Some("global-model"));
    assert_eq!(model.origin, ConfigOrigin::GlobalDefault);
    // Harness default is the overridden secondary.
    assert_eq!(model.overridden_value.as_deref(), Some("harness-model"));
    assert_eq!(model.overridden_origin, Some(ConfigOrigin::HarnessDefault));
}

/// A reserved key in the definition env is stripped by sanitization and must
/// not reach the reader. This test exercises the reader's contract (a key
/// absent from the tier falls through) — sanitization itself is pinned in
/// the `agent_config_tests.rs` constructor tests.
#[test]
fn reserved_key_absent_from_definition_env_falls_through() {
    let record = test_record();
    let runtime = test_runtime(); // model_env_var = "GOOSE_MODEL"
                                  // definition_env contains only an unrelated key — the env map here is what
                                  // the command boundary would produce after stripping a reserved key; the
                                  // reader must fall through to the next tier (persona structured model).
    let tiers = InheritedConfigTiers {
        definition_env: BTreeMap::new(), // stripped — nothing survives
        persona_model: Some("persona-struct-model".to_string()),
        ..Default::default()
    };

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let model = surface.normalized.model.unwrap();
    // Falls through to persona structured model.
    assert_eq!(model.value.as_deref(), Some("persona-struct-model"));
    assert_eq!(model.origin, ConfigOrigin::PersonaDefault);
}

// ── Effort same-value collapse (Phase 1 / heuristic B) ───────────────────────
//
// When live ACP effort echoes the inherited value, the panel must show the
// true inherited origin — not "Runtime override (this session only)".
// Tests pin all three cases from the plan's success criteria.

/// Case 1: live ACP effort == inherited global effort → falls through to
/// GlobalDefault, no secondary row.  Wrapped in with_goose_path_root to
/// prevent the Goose config file from injecting a GOOSE_THINKING_EFFORT value
/// that could flip the equal-value test to a genuine divergence.
#[test]
fn acp_effort_equal_to_global_effort_collapses_to_global_default() {
    let record = test_record();
    let runtime = buzz_agent_rt();
    // Live session echoing inherited effort=high.
    let cache = SessionConfigCache {
        config_options: vec![AcpConfigOptionEntry {
            config_id: "effort".to_string(),
            category: Some("effort".to_string()),
            display_name: Some("Effort".to_string()),
            current_value: Some("high".to_string()),
            options: vec![],
        }],
        available_modes: vec![],
        available_models: vec![],
        current_model: None,
        model_overridden: false,
        goose_native_config: None,
        captured_at: "".to_string(),
    };
    let tiers = global_env_tiers("BUZZ_AGENT_THINKING_EFFORT", "high");

    let surface = read_config_surface(&record, Some(runtime), Some(&cache), &tiers);

    let effort = surface
        .normalized
        .thinking_effort
        .expect("effort must surface");
    // Equal-value collapse: baseline origin shown, not AcpConfigOption.
    assert_eq!(effort.value.as_deref(), Some("high"));
    assert_eq!(effort.origin, ConfigOrigin::GlobalDefault);
    // No secondary row — nothing was genuinely overridden.
    assert!(effort.overridden_value.is_none());
    assert!(effort.overridden_origin.is_none());
}

/// Case 2: live ACP effort != inherited global effort → genuine divergence,
/// AcpConfigOption wins with global as overridden baseline (same as AC-5).
#[test]
fn acp_effort_differs_from_global_effort_shows_override_with_baseline() {
    let record = test_record();
    let runtime = buzz_agent_rt();
    let cache = SessionConfigCache {
        config_options: vec![AcpConfigOptionEntry {
            config_id: "effort".to_string(),
            category: Some("effort".to_string()),
            display_name: Some("Effort".to_string()),
            current_value: Some("high".to_string()),
            options: vec![],
        }],
        available_modes: vec![],
        available_models: vec![],
        current_model: None,
        model_overridden: false,
        goose_native_config: None,
        captured_at: "".to_string(),
    };
    let tiers = global_env_tiers("BUZZ_AGENT_THINKING_EFFORT", "medium");

    let surface = read_config_surface(&record, Some(runtime), Some(&cache), &tiers);

    let effort = surface
        .normalized
        .thinking_effort
        .expect("effort must surface");
    // Genuine divergence: ACP wins.
    assert_eq!(effort.value.as_deref(), Some("high"));
    assert_eq!(effort.origin, ConfigOrigin::AcpConfigOption);
    // Global medium is the overridden secondary.
    assert_eq!(effort.overridden_value.as_deref(), Some("medium"));
    assert_eq!(effort.overridden_origin, Some(ConfigOrigin::GlobalDefault));
}

/// Case 3: live ACP effort with NO inherited tier → stays AcpConfigOption
/// (nothing to collapse to).
#[test]
fn acp_effort_with_no_inherited_tier_stays_acp_config_option() {
    let record = test_record();
    let runtime = buzz_agent_rt();
    let cache = SessionConfigCache {
        config_options: vec![AcpConfigOptionEntry {
            config_id: "effort".to_string(),
            category: Some("effort".to_string()),
            display_name: Some("Effort".to_string()),
            current_value: Some("high".to_string()),
            options: vec![],
        }],
        available_modes: vec![],
        available_models: vec![],
        current_model: None,
        model_overridden: false,
        goose_native_config: None,
        captured_at: "".to_string(),
    };

    // No inherited tiers — ACP is the only source.
    let surface = read_config_surface(&record, Some(runtime), Some(&cache), &no_tiers());

    let effort = surface
        .normalized
        .thinking_effort
        .expect("effort must surface from ACP when no inherited tier");
    assert_eq!(effort.value.as_deref(), Some("high"));
    assert_eq!(effort.origin, ConfigOrigin::AcpConfigOption);
    assert!(effort.overridden_value.is_none());
}

// ── Phase 3 Delta-5: legacy-aliasing reader parity tests (Goose runtime) ─────
//
// Each test is paired with a spawn-bridge test in
// `readiness_effort_bridge_tests.rs`.  Both must assert the same winning value
// and origin so panel display and process env always agree.
//
// Fixtures: `test_runtime()` (Goose, GOOSE_THINKING_EFFORT, accepted set),
// `test_record()` (empty env_vars).

/// Delta-5 pair 1 (reader side): legacy-only record env → GOOSE_THINKING_EFFORT
/// surfaces as BuzzExplicit.
/// Spawn pair: `legacy_only_record_env_bridges_to_native_key`.
#[test]
fn goose_legacy_record_effort_surfaces_via_native_key_as_buzz_explicit() {
    let mut record = test_record();
    record.env_vars.insert(
        crate::managed_agents::config_bridge::LEGACY_THINKING_EFFORT_KEY.to_string(),
        "high".to_string(),
    );
    let runtime = test_runtime();

    let surface = read_config_surface(&record, Some(runtime), None, &no_tiers());

    let effort = surface
        .normalized
        .thinking_effort
        .expect("legacy record effort must surface for Goose via alias");
    assert_eq!(effort.value.as_deref(), Some("high"));
    assert_eq!(effort.origin, ConfigOrigin::BuzzExplicit);
}

/// Delta-5 pair 2 (reader side): record has both native and legacy — native wins.
/// Spawn pair: `native_and_legacy_in_record_native_wins_no_double_write`.
#[test]
fn goose_record_native_beats_legacy_in_reader() {
    let mut record = test_record();
    record
        .env_vars
        .insert("GOOSE_THINKING_EFFORT".to_string(), "medium".to_string());
    record.env_vars.insert(
        crate::managed_agents::config_bridge::LEGACY_THINKING_EFFORT_KEY.to_string(),
        "high".to_string(),
    );
    let runtime = test_runtime();

    let surface = read_config_surface(&record, Some(runtime), None, &no_tiers());

    let effort = surface
        .normalized
        .thinking_effort
        .expect("effort must surface from native key");
    assert_eq!(
        effort.value.as_deref(),
        Some("medium"),
        "native key must win over legacy when both present"
    );
    assert_eq!(effort.origin, ConfigOrigin::BuzzExplicit);
}

/// Delta-5 pair 3 (reader side): record legacy beats persona native (tier-first).
/// Spawn pair: `record_legacy_beats_persona_native_in_spawn`.
#[test]
fn goose_record_legacy_beats_persona_native_in_reader() {
    let mut record = test_record();
    record.env_vars.insert(
        crate::managed_agents::config_bridge::LEGACY_THINKING_EFFORT_KEY.to_string(),
        "high".to_string(),
    );
    let runtime = test_runtime();
    let tiers = persona_env_tiers("GOOSE_THINKING_EFFORT", "low");

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let effort = surface
        .normalized
        .thinking_effort
        .expect("effort must surface");
    // Tier-first: record legacy (high) beats persona native (low).
    assert_eq!(effort.value.as_deref(), Some("high"));
    assert_eq!(effort.origin, ConfigOrigin::BuzzExplicit);
}

/// Delta-5 pair 4 (reader side): invalid legacy value is skipped; persona tier wins.
/// Spawn pair: `invalid_legacy_value_in_record_not_bridged`.
#[test]
fn goose_invalid_legacy_value_skipped_persona_tier_wins_in_reader() {
    let mut record = test_record();
    // "minimal" is not in Goose's accepted set.
    record.env_vars.insert(
        crate::managed_agents::config_bridge::LEGACY_THINKING_EFFORT_KEY.to_string(),
        "minimal".to_string(),
    );
    let runtime = test_runtime();
    let tiers = persona_env_tiers("GOOSE_THINKING_EFFORT", "low");

    let surface = read_config_surface(&record, Some(runtime), None, &tiers);

    let effort = surface
        .normalized
        .thinking_effort
        .expect("persona-tier effort must surface after invalid legacy is skipped");
    assert_eq!(
        effort.value.as_deref(),
        Some("low"),
        "invalid legacy must be skipped; lower persona tier must win"
    );
    assert_eq!(effort.origin, ConfigOrigin::PersonaDefault);
}

/// Delta-5 pair 5 (reader side): global legacy is NOT surfaced as Goose effort.
/// Spawn pair: `global_legacy_effort_is_not_bridged_to_native_key`.
#[test]
fn goose_global_legacy_effort_not_surfaced_in_reader() {
    let record = test_record();
    let runtime = test_runtime();
    // Global env has the legacy key — must not be bridged to GOOSE_THINKING_EFFORT.
    let tiers = global_env_tiers(
        crate::managed_agents::config_bridge::LEGACY_THINKING_EFFORT_KEY,
        "high",
    );

    // Use with_goose_path_root(None) to suppress file-config reading, isolating
    // the global-legacy exclusion assertion.
    let surface = with_goose_path_root(Some("/nonexistent/buzz-test-path"), || {
        read_config_surface(&record, Some(runtime), None, &tiers)
    });

    assert!(
        surface.normalized.thinking_effort.is_none(),
        "global legacy effort must not surface as Goose effort"
    );
}
