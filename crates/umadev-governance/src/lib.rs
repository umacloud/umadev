//! `umadev-governance` — the kernel that enforces `UMADEV_HOST_SPEC_V1`.
//!
//! Every UmaDev binding (CLI hook entry, agent runtime, MCP shim,
//! CI evaluator) calls into this crate. Functions in here are the
//! single source of truth for "what counts as commercial-grade output";
//! anything that wants to enforce or audit UmaDev rules MUST use
//! these functions rather than re-implementing the regex.
//!
//! Safety contract:
//! - Every public function fails open: an exceptional input returns
//!   [`Decision::pass`] or an empty audit record. The host MUST NEVER
//!   be blocked by a bug in the governor.
//! - No global mutable state. Every function takes inputs and returns
//!   pure data; callers handle persistence.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::doc_lazy_continuation,
    clippy::must_use_candidate,
    clippy::too_many_arguments,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::match_same_arms
)]

pub mod audit;
pub mod compliance;
pub mod context;
pub mod design;
pub mod policy;
pub mod rules;
pub mod tokenizer;

pub use design::{
    extract_fonts, is_generic_font, scan_design_quality, DesignFinding, DesignSeverity,
};

pub use audit::{
    extract_api_urls, record_api_calls, record_tool_call, ApiCallRecord, ToolCallRecord,
};
pub use compliance::{
    build_compliance_mapping, content_sha256, file_sha256, ClauseEvidence, ComplianceFrameworks,
    CLAUSE_COMPLIANCE,
};
pub use context::{compose_session_context, SessionContext};
pub use policy::{DisabledSection, ExclusionsSection, ExtraSection, Policy};
pub use rules::{
    check_a11y, check_ai_slop, check_api_error_convention, check_bare_catch,
    check_c_buffer_overflow, check_c_malloc_null_check, check_color_tokens, check_csp_required,
    check_dangerous_bash, check_db_transaction_rollback, check_debug_residue, check_deep_nesting,
    check_emoji, check_error_boundary, check_eval_injection, check_frontend_db_access,
    check_go_panic, check_hardcoded_secret, check_hsts_header, check_https_redirect,
    check_i18n_required, check_input_validation, check_insecure_cookie, check_insecure_cors,
    check_java_system_exit, check_jwt_defects, check_kotlin_nonnull_assertion,
    check_loose_array_types, check_magic_numbers, check_malicious_urls, check_missing_auth_guard,
    check_non_null_assertion, check_php_shell_exec, check_python_bare_except, check_python_global,
    check_rate_limiting, check_ruby_eval_send, check_rust_unwrap, check_security_headers,
    check_sensitive_path, check_sql_injection, check_ssrf, check_structured_logging,
    check_swift_force_unwrap, check_todo_residue, check_ts_any, check_typosquat_packages,
    check_unreliable_sources, check_unsafe_deserialization, check_unused_variables,
    check_xpath_injection, check_xxe, scan_content, scan_content_with_policy, Decision,
};
#[allow(unused_imports)]
pub use rules::{
    check_clickjacking_protection, check_client_redirect_injection, check_client_secret_leak,
    check_clojure_eval, check_csrf_protection, check_dangerous_inner_html, check_dart_dynamic,
    check_document_cookie_access, check_elixir_to_atom, check_empty_deps_array,
    check_file_upload_validation, check_for_in_array, check_fsharp_null, check_graphql_depth_limit,
    check_graphql_introspection, check_graphql_n_plus_1, check_hard_delete, check_hardcoded_config,
    check_haskell_unsafe_io, check_info_leakage, check_inline_event_handlers,
    check_insecure_file_perms, check_insecure_jsonp, check_insecure_random, check_insecure_storage,
    check_insecure_tls, check_loose_equality, check_lua_loadstring, check_mass_assignment,
    check_mutable_default_export, check_ocaml_magic, check_open_redirect, check_path_traversal,
    check_perl_eval_regex, check_plaintext_password, check_promise_without_catch,
    check_prototype_pollution, check_r_hardcoded_path, check_react_list_key, check_redos_regex,
    check_referrer_redirect, check_render_side_effects, check_response_splitting,
    check_scala_null_return, check_sensitive_logging, check_state_mutation, check_toctou_race,
    check_unhandled_fetch_error, check_unsafe_date_parse, check_unsafe_json_parse,
    check_unsafe_parse, check_unsafe_post_message, check_unsafe_window_open,
    check_unsynchronized_mutation, check_untyped_props, check_use_effect_cleanup,
    check_var_declarations, check_websocket_auth, check_wildcard_imports,
};

/// Re-export the spec marker so downstream crates can pin against it.
pub use umadev_spec::SPEC_VERSION;
