// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Per-tool MCP handler modules. Each file contains exactly one (or a small
//! cluster of tightly-coupled) `handle_*` functions, extracted from the
//! original monolithic `mcp.rs` for readability.

pub(super) mod store;
pub(super) mod recall;
pub(super) mod capabilities;
pub(super) mod expand_query;
pub(super) mod auto_tag;
pub(super) mod detect_contradiction;
pub(super) mod search;
pub(super) mod get_taxonomy;
pub(super) mod check_duplicate;
pub(super) mod entity_register;
pub(super) mod entity_get_by_alias;
pub(super) mod kg_timeline;
pub(super) mod kg_invalidate;
pub(super) mod kg_query;
pub(super) mod find_paths;
pub(super) mod list;
pub(super) mod load_family;
pub(super) mod delete;
pub(super) mod promote;
pub(super) mod forget;
pub(super) mod update;
pub(super) mod get;
pub(super) mod link;
pub(super) mod verify;
pub(super) mod replay;
pub(super) mod reflect;
pub(super) mod reflection_origin;
// v0.7.0 L2-3 (issue #668) — Reflection invalidation propagation
// (notification, not cascade). Read-side surface for the dependents
// flagged by the L2-3 walker.
pub(super) mod dependents_of_invalidated;
pub(super) mod consolidate;
pub(super) mod namespace;
pub(super) mod agent;
pub(super) mod notify;
pub(super) mod subscribe;
pub(super) mod quota_status;
pub(super) mod pending;
pub(super) mod archive;
pub(super) mod session_start;
// v0.7.0 (issue #691) — substrate-level agent-action rules engine.
// Two read-only MCP tools: `memory_check_agent_action` (the
// PreToolUse hook target) and `memory_rule_list` (operator dashboard
// surface). Mutation tools are explicitly NOT registered over MCP
// per design revision 2026-05-13 — operator uses CLI or HTTP.
pub(super) mod check_agent_action;
pub(super) mod rule_list;
// v0.7.0 L1-5 — Agent Skills ingestion substrate (Pillar 1.5).
pub(super) mod skill_register;
pub(super) mod skill_list;
pub(super) mod skill_get;
pub(super) mod skill_resource;
pub(super) mod skill_export;
// v0.7.0 L2-6 (issue #671) — closing the loop: reflections become skills.
pub(super) mod skill_promote;
// v0.7.0 L2-7 (issue #672) — reflection-skill composition declaration.
pub(super) mod skill_compositional_context;
// v0.7.0 QW-3 — context-offload substrate primitive. Handlers ship
// as substrate-only at v0.7.0; v0.8.0 short-term-context-compression
// wires them into `tool_definitions_for_profile` after the
// profile-count test fleet is rolled forward.
pub mod offload;

// Re-export all handler functions and types to make them accessible from
// the parent `mcp` module (super) without requiring callers to know the
// internal tools:: submodule structure.
pub(super) use self::{
    store::handle_store,
    recall::handle_recall,
    recall::handle_recall_with_pre_recall_hook,
    capabilities::CapabilitiesAccept,
    capabilities::handle_capabilities_with_conn,
    capabilities::handle_capabilities_with_conn_v3,
    capabilities::build_capabilities_summary,
    capabilities::build_capabilities_describe_to_user,
    capabilities::build_capabilities_tools,
    capabilities::build_agent_permitted_families,
    capabilities::effective_tier_label,
    capabilities::overlay_tool_payloads,
    capabilities::format_rule_summary,
    expand_query::handle_expand_query,
    auto_tag::handle_auto_tag,
    detect_contradiction::handle_detect_contradiction,
    search::handle_search,
    get_taxonomy::handle_get_taxonomy,
    check_duplicate::handle_check_duplicate,
    entity_register::handle_entity_register,
    entity_get_by_alias::handle_entity_get_by_alias,
    kg_timeline::handle_kg_timeline,
    kg_invalidate::handle_kg_invalidate,
    kg_query::handle_kg_query,
    find_paths::handle_find_paths,
    list::handle_list,
    load_family::handle_load_family,
    load_family::handle_smart_load,
    delete::handle_delete,
    promote::handle_promote,
    forget::handle_forget,
    forget::handle_stats,
    update::handle_update,
    get::handle_get,
    link::handle_link,
    link::handle_get_links,
    verify::handle_verify,
    replay::handle_replay,
    reflect::handle_reflect,
    reflection_origin::handle_reflection_origin,
    dependents_of_invalidated::handle_dependents_of_invalidated,
    consolidate::handle_consolidate,
    namespace::handle_namespace_set_standard,
    namespace::handle_namespace_get_standard,
    namespace::handle_namespace_clear_standard,
    agent::handle_agent_register,
    agent::handle_agent_list,
    notify::handle_notify,
    notify::handle_inbox,
    subscribe::handle_subscribe,
    subscribe::handle_unsubscribe,
    subscribe::handle_list_subscriptions,
    subscribe::handle_subscription_replay,
    subscribe::handle_subscription_dlq_list,
    quota_status::handle_quota_status,
    pending::handle_pending_list,
    pending::handle_pending_approve,
    pending::handle_pending_reject,
    archive::handle_archive_list,
    archive::handle_archive_restore,
    archive::handle_archive_purge,
    archive::handle_archive_stats,
    archive::handle_gc,
    session_start::handle_session_start,
    check_agent_action::handle_check_agent_action,
    rule_list::handle_rule_list,
    skill_register::handle_skill_register,
    skill_list::handle_skill_list,
    skill_get::handle_skill_get,
    skill_resource::handle_skill_resource,
    skill_export::handle_skill_export,
    skill_promote::handle_skill_promote_from_reflection,
    skill_compositional_context::handle_skill_compositional_context,
};
