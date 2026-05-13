// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_store` handler and HTTP federation forward helpers.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::llm::OllamaClient;
use crate::models::{Memory, Tier};
use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;

// --- Tool handlers ---

/// Minimum content length (bytes) before the post-store autonomy hook
/// will invoke LLM `auto_tag` / `detect_contradiction`. Below this the
/// LLM round-trip cost exceeds the informational payoff.
const AUTONOMY_MIN_CONTENT_LEN: usize = 50;

/// v0.6.3.1 P2 (G6) — `on_conflict` modes for `memory_store`.
///
/// * `Error`   — refuse the write with a typed CONFLICT error. This is the
///               new default for v2-aware clients.
/// * `Merge`   — keep the v0.6.3 silent-merge upsert behaviour. Default for
///               v1 / unknown clients to preserve backward compatibility.
/// * `Version` — auto-suffix the title with `(2)`, `(3)`, ... to write a
///               distinct row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnConflict {
    Error,
    Merge,
    Version,
}

impl OnConflict {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "error" => Ok(Self::Error),
            "merge" => Ok(Self::Merge),
            "version" => Ok(Self::Version),
            other => Err(format!(
                "invalid on_conflict '{other}' (expected error|merge|version)"
            )),
        }
    }
}

/// Capability profile detection. v2-aware clients default to `Error`; v1 /
/// unknown clients default to `Merge` to preserve the v0.6.3 contract. The
/// determination keys off the MCP client name (captured at `initialize`
/// from `clientInfo.name`). Known v2 clients are listed explicitly so the
/// policy is auditable. The list is intentionally narrow — adding a name
/// here is a deliberate decision that "this client knows how to handle a
/// CONFLICT response from memory_store".
fn default_on_conflict_for_client(mcp_client: Option<&str>) -> OnConflict {
    let Some(client) = mcp_client else {
        return OnConflict::Merge;
    };
    // Match on the prefix before any '@' — `ai:foo@host:pid-N` style ids.
    let head = client.split('@').next().unwrap_or(client);
    let normalized = head.to_ascii_lowercase();
    // v2-capable clients (explicitly opted-in via known name).
    const V2_CLIENT_PREFIXES: &[&str] = &["ai:claude-code", "ai:ai-memory-cli/v2"];
    for prefix in V2_CLIENT_PREFIXES {
        if normalized.starts_with(prefix) {
            return OnConflict::Error;
        }
    }
    OnConflict::Merge
}

/// Forward an MCP write call to a local HTTP daemon so the daemon's
/// federation fanout coordinator (`broadcast_store_quorum` / `broadcast_link_quorum`
/// / `broadcast_delete_quorum`) takes over replication. Closes the
/// MCP-stdio-vs-federation gap surfaced by a2a-gate v0.6.0 r6 (#318).
///
/// Returns the daemon's JSON body on 2xx, or a structured error string
/// that the MCP layer surfaces as a JSON-RPC `result.error`. On 5xx /
/// transport failure the caller gets a clear message naming the
/// forward URL so operators can distinguish "fanout daemon down"
/// from "quorum not met".
fn forward_to_http(
    method: reqwest::Method,
    url: &str,
    body: Option<&Value>,
    extra_headers: &[(&str, String)],
) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("federation_forward: build client: {e}"))?;
    let mut req = client.request(method, url);
    for (k, v) in extra_headers {
        req = req.header(*k, v);
    }
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .map_err(|e| format!("federation_forward: POST {url}: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| format!("federation_forward: read body from {url}: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "federation_forward: {url} returned {status}: {text}"
        ));
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("federation_forward: parse body from {url}: {e} (raw: {text})"))
}

/// MCP `memory_store` → HTTP `POST {forward_url}/api/v1/memories`.
/// Translates the MCP params (which mirror the HTTP request body field
/// names verbatim, with the exception of how `metadata.agent_id` is
/// surfaced) into the HTTP daemon's `CreateMemoryRequest` shape, then
/// reshapes the 201 response into the MCP `memory_store` envelope
/// callers expect (`{id, tier, title, namespace, agent_id, ...}`).
fn forward_store_to_http(
    forward_url: &str,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let url = format!("{}/api/v1/memories", forward_url.trim_end_matches('/'));

    // Resolve agent_id with the same precedence chain the local path
    // uses, then surface it as an X-Agent-Id header (the HTTP handler's
    // canonical resolution channel for daemon-mode multi-tenancy).
    let explicit_agent_id = params["agent_id"]
        .as_str()
        .or_else(|| params["metadata"]["agent_id"].as_str());
    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;

    // The HTTP request body mirrors the MCP params; pass them through
    // and let the HTTP handler do all validation, governance, quota,
    // dedup, embedding, audit, and federation broadcast.
    let body = params.clone();
    let headers: &[(&str, String)] = &[("X-Agent-Id", agent_id)];

    forward_to_http(reqwest::Method::POST, &url, Some(&body), headers)
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_store(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    embedder: Option<&dyn Embed>,
    llm: Option<&OllamaClient>,
    vector_index: Option<&VectorIndex>,
    resolved_ttl: &crate::config::ResolvedTtl,
    autonomous_hooks: bool,
    mcp_client: Option<&str>,
    federation_forward_url: Option<&str>,
) -> Result<Value, String> {
    // v0.7.0 (issue #318) — when operators have configured a federation
    // forward URL, every MCP write routes through the local HTTP daemon
    // so its `broadcast_store_quorum` fanout runs. Direct-SQLite path
    // below is the legacy single-node behaviour, preserved as default
    // for environments without a sibling `ai-memory serve` process.
    if let Some(url) = federation_forward_url {
        return forward_store_to_http(url, params, mcp_client);
    }

    let title = params["title"].as_str().ok_or("title is required")?;
    let content = params["content"].as_str().ok_or("content is required")?;
    let tier_str = params["tier"].as_str().unwrap_or("mid");
    let tier = Tier::from_str(tier_str).ok_or(format!("invalid tier: {tier_str}"))?;
    let namespace = params["namespace"].as_str().unwrap_or("global").to_string();
    let source = params["source"].as_str().unwrap_or("claude").to_string();
    // v0.6.3.1 P2 (G6) — explicit `on_conflict` overrides the per-client default.
    let on_conflict = if let Some(s) = params["on_conflict"].as_str() {
        OnConflict::parse(s)?
    } else {
        default_on_conflict_for_client(mcp_client)
    };
    // B4 (R2-LOW) — clamp to i32 range instead of panicking on out-of-range
    // JSON. A maliciously-crafted `"priority": 9999999999` would have crashed
    // the stdio MCP server pre-fix. `validate_priority` below enforces the
    // semantic 1-10 range, so the clamp is purely a panic guard.
    let priority = i32::try_from(params["priority"].as_i64().unwrap_or(5)).unwrap_or(i32::MAX);
    let confidence = params["confidence"].as_f64().unwrap_or(1.0);
    let tags: Vec<String> = params["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    validate::validate_title(title).map_err(|e| e.to_string())?;
    validate::validate_content(content).map_err(|e| e.to_string())?;
    validate::validate_namespace(&namespace).map_err(|e| e.to_string())?;
    validate::validate_source(&source).map_err(|e| e.to_string())?;
    validate::validate_tags(&tags).map_err(|e| e.to_string())?;
    validate::validate_priority(priority).map_err(|e| e.to_string())?;
    validate::validate_confidence(confidence).map_err(|e| e.to_string())?;

    let mut metadata = if params["metadata"].is_object() {
        params["metadata"].clone()
    } else {
        serde_json::json!({})
    };
    // Resolve agent_id via the NHI-hardened precedence chain and merge into
    // metadata. Explicit values win in this order:
    //   1. top-level `agent_id` param
    //   2. embedded `metadata.agent_id` (backward compatible with callers
    //      that supply it inline)
    //   3. env / MCP clientInfo / host / anonymous (handled inside `identity`)
    let explicit_agent_id = params["agent_id"]
        .as_str()
        .or_else(|| metadata.get("agent_id").and_then(serde_json::Value::as_str));
    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(agent_id.clone()),
        );
    }
    // #151 scope: top-level `scope` param OR inline metadata.scope
    let explicit_scope = params["scope"]
        .as_str()
        .or_else(|| metadata.get("scope").and_then(serde_json::Value::as_str))
        .map(str::to_string);
    if let Some(ref s) = explicit_scope {
        validate::validate_scope(s).map_err(|e| e.to_string())?;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("scope".to_string(), serde_json::Value::String(s.clone()));
        }
    }
    validate::validate_metadata(&metadata).map_err(|e| e.to_string())?;

    let now = chrono::Utc::now();
    let expires_at = resolved_ttl
        .ttl_for_tier(&tier)
        .map(|s| (now + chrono::Duration::seconds(s)).to_rfc3339());

    // v0.6.3.1 P2 (G6) — apply the conflict policy BEFORE building the
    // canonical Memory. `Version` mode rewrites `title` to a free suffix;
    // `Error` mode short-circuits with a typed error if the row already
    // exists; `Merge` defers to the legacy code path below.
    let resolved_title = match on_conflict {
        OnConflict::Error => {
            if let Some(existing_id) =
                db::find_by_title_namespace(conn, title, &namespace).map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "CONFLICT: memory with title '{title}' already exists in namespace \
                     '{namespace}' (existing id: {existing_id}). Pass \
                     on_conflict='merge' to update in place or 'version' to suffix the title."
                ));
            }
            title.to_string()
        }
        OnConflict::Version => {
            db::next_versioned_title(conn, title, &namespace).map_err(|e| e.to_string())?
        }
        OnConflict::Merge => title.to_string(),
    };

    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace,
        title: resolved_title,
        content: content.to_string(),
        tags,
        priority: priority.clamp(1, 10),
        confidence: confidence.clamp(0.0, 1.0),
        source,
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
        metadata,
        reflection_depth: 0,
    };

    // v0.7.0 K9 — unified permission pipeline. The K9 evaluator
    // composes declarative `[permissions.rules]` matchers + the K3
    // `[permissions].mode` knob + (when wired) hook decisions into
    // a single `Decision`. Deny-first: if a rule denies, we
    // short-circuit before the K3 governance gate ever resolves a
    // policy. Allow falls through to the existing K3 / governance
    // gate so legacy `[governance]` policies continue to work.
    {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let payload = serde_json::to_value(&mem).unwrap_or_default();
        let ctx = PermissionContext {
            op: Op::MemoryStore,
            namespace: mem.namespace.clone(),
            agent_id: agent_id.clone(),
            payload,
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("store denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "store",
                    "namespace": mem.namespace,
                }));
            }
        }
    }

    // Task 1.9: governance enforcement (store-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let payload = serde_json::to_value(&mem).unwrap_or_default();
        match db::enforce_governance(
            conn,
            GovernedAction::Store,
            &mem.namespace,
            &agent_id,
            None,
            None,
            &payload,
        )
        .map_err(|e| e.to_string())?
        {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                return Err(format!("store denied by governance: {reason}"));
            }
            GovernanceDecision::Pending(pending_id) => {
                // v0.7.0 K4 — surface the new pending row through the
                // subscription dispatcher so K10's Approval API sees a
                // uniform stream of `approval_requested` events
                // regardless of which transport (MCP / HTTP) created
                // the row. Best-effort, fire-and-forget: a dispatch
                // failure must not roll back the pending row.
                crate::subscriptions::dispatch_approval_requested(conn, &pending_id, db_path);
                return Ok(json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": "store",
                    "namespace": mem.namespace,
                }));
            }
        }
    }

    // True dedup: check for exact title+namespace match (#97).
    //
    // v0.6.3.1 P2 (G6) — only the Merge policy enters the dedup-then-update
    // branch. `Error` mode already short-circuited above; `Version` mode
    // already rewrote the title to a free suffix so an exact dup cannot
    // exist. Both still call `find_contradictions` so the response can
    // surface `potential_contradictions` (similar-title fuzzy matches).
    let existing = db::find_contradictions(conn, &mem.title, &mem.namespace).unwrap_or_default();
    let exact_dup = if matches!(on_conflict, OnConflict::Merge) {
        existing
            .iter()
            .find(|c| c.title == mem.title && c.namespace == mem.namespace)
    } else {
        None
    };
    if let Some(dup) = exact_dup {
        // Update existing memory instead of creating a duplicate.
        // Preserve the original agent_id (provenance is immutable) — the
        // existing memory's metadata.agent_id wins over anything in the
        // incoming store.
        let preserved_metadata = crate::identity::preserve_agent_id(&dup.metadata, &mem.metadata);
        let (_found, content_changed) = db::update(
            conn,
            &dup.id,
            None,                       // title (unchanged)
            Some(mem.content.as_str()), // content (update)
            Some(&mem.tier),            // tier
            None,                       // namespace (unchanged)
            Some(&mem.tags),            // tags
            Some(mem.priority),         // priority
            Some(mem.confidence),       // confidence
            None,                       // expires_at
            Some(&preserved_metadata),  // metadata (agent_id preserved)
        )
        .map_err(|e| e.to_string())?;
        // Regenerate embedding if content changed during dedup update
        if content_changed && let Some(emb) = embedder {
            let text = format!("{} {}", mem.title, mem.content);
            if let Ok(embedding) = emb.embed(&text) {
                let _ = db::set_embedding(conn, &dup.id, &embedding);
                if let Some(idx) = vector_index {
                    idx.remove(&dup.id);
                    idx.insert(dup.id.clone(), embedding);
                }
            }
        }
        // #196: echo the preserved agent_id (original on dedup, not the caller's)
        let echoed_agent_id = preserved_metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return Ok(json!({
            "id": dup.id,
            "tier": mem.tier,
            "title": mem.title,
            "namespace": mem.namespace,
            "agent_id": echoed_agent_id,
            "duplicate": true,
            "action": "updated existing memory"
        }));
    }

    // v0.7 K8 — per-agent quota gate. Pre-write check; on exceeded
    // limit returns a `QUOTA_EXCEEDED` diagnostic naming the limit
    // hit. Bytes counted = (title + content + serialized metadata)
    // to match the post-write `record_op` accounting below.
    let payload_bytes = i64::try_from(
        mem.title.len()
            + mem.content.len()
            + serde_json::to_string(&mem.metadata)
                .map(|s| s.len())
                .unwrap_or(0),
    )
    .unwrap_or(i64::MAX);
    // H12 (#628 blocker): combine the quota check + counter
    // increment in a single atomic transaction so concurrent writers
    // cannot each pass the check and then both bump the counter past
    // the cap.
    if let Err(e) = crate::quotas::check_and_record(
        conn,
        &agent_id,
        crate::quotas::QuotaOp::Memory {
            bytes: payload_bytes,
        },
    ) {
        return Err(e.to_string());
    }

    let actual_id = match db::insert(conn, &mem) {
        Ok(id) => id,
        Err(e) => {
            // Insert failed AFTER we committed quota — refund so the
            // counter reflects only successful stores.
            if let Err(re) = crate::quotas::refund_op(
                conn,
                &agent_id,
                crate::quotas::QuotaOp::Memory {
                    bytes: payload_bytes,
                },
            ) {
                tracing::warn!("quota refund_op failed for agent {}: {}", &agent_id, re);
            }
            return Err(e.to_string());
        }
    };

    // PR-5 (issue #487): security audit trail. No-op when disabled.
    crate::audit::emit(crate::audit::EventBuilder::new(
        crate::audit::AuditAction::Store,
        crate::audit::actor(
            agent_id.clone(),
            mcp_client.map_or("host_fallback", |_| "mcp_client_info"),
            explicit_scope.clone(),
        ),
        crate::audit::target_memory(
            actual_id.clone(),
            mem.namespace.clone(),
            Some(mem.title.clone()),
            Some(mem.tier.to_string()),
            explicit_scope.clone(),
        ),
    ));

    // Exclude self-ID from contradictions (both proposed and actual, since upsert may reuse existing ID)
    let contradiction_ids: Vec<String> = existing
        .iter()
        .filter(|c| c.id != mem.id && c.id != actual_id)
        .map(|c| c.id.clone())
        .collect();

    // Generate and store embedding if embedder is available
    if let Some(emb) = embedder {
        let text = format!("{} {}", mem.title, mem.content);
        match emb.embed(&text) {
            Ok(embedding) => {
                if let Err(e) = db::set_embedding(conn, &actual_id, &embedding) {
                    tracing::warn!("failed to store embedding for {}: {}", &actual_id, e);
                }
                // Add to HNSW index for fast ANN search
                if let Some(idx) = vector_index {
                    idx.insert(actual_id.clone(), embedding);
                }
            }
            Err(e) => {
                tracing::warn!("failed to generate embedding for {}: {}", &actual_id, e);
            }
        }
    }

    // v0.6.0.0 post-store autonomy hooks. When enabled via
    // `AI_MEMORY_AUTONOMOUS_HOOKS=1` or `autonomous_hooks = true` in
    // config.toml AND an LLM is wired AND the content is long enough
    // to be meaningfully taggable, fire `auto_tag` + `detect_contradiction`
    // synchronously and persist the results into the memory's metadata.
    // Best-effort: any LLM error is logged and does not fail the store.
    // Skipped for internal/system namespaces to avoid feedback loops.
    let mut auto_tags: Vec<String> = Vec::new();
    let mut confirmed_contradictions: Vec<String> = Vec::new();
    let hooks_skipped_reason: Option<&'static str> = if !autonomous_hooks {
        Some("disabled")
    } else if llm.is_none() {
        Some("no_llm")
    } else if mem.content.len() < AUTONOMY_MIN_CONTENT_LEN {
        Some("content_too_short")
    } else if mem.namespace.starts_with('_') {
        Some("internal_namespace")
    } else {
        None
    };
    if hooks_skipped_reason.is_none()
        && let Some(llm_client) = llm
    {
        match llm_client.auto_tag(&mem.title, &mem.content, None) {
            Ok(tags) => {
                auto_tags = tags.into_iter().take(8).collect();
            }
            Err(e) => {
                tracing::warn!("auto_tag hook failed for {}: {}", &actual_id, e);
            }
        }
        for cand in &existing {
            if cand.id == actual_id || cand.id == mem.id {
                continue;
            }
            match llm_client.detect_contradiction(&mem.content, &cand.content) {
                Ok(true) => confirmed_contradictions.push(cand.id.clone()),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        "detect_contradiction hook failed ({actual_id} vs {}): {e}",
                        cand.id
                    );
                }
            }
        }
        // Persist hook results into metadata. Best-effort — a failed update
        // here does not fail the store (the memory is already committed).
        if !auto_tags.is_empty() || !confirmed_contradictions.is_empty() {
            let mut updated_metadata = mem.metadata.clone();
            if let Some(obj) = updated_metadata.as_object_mut() {
                if !auto_tags.is_empty() {
                    obj.insert("auto_tags".to_string(), json!(auto_tags));
                }
                if !confirmed_contradictions.is_empty() {
                    obj.insert(
                        "confirmed_contradictions".to_string(),
                        json!(confirmed_contradictions),
                    );
                }
            }
            if let Err(e) = db::update(
                conn,
                &actual_id,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(&updated_metadata),
            ) {
                tracing::warn!(
                    "autonomy-hook metadata update failed for {}: {}",
                    &actual_id,
                    e
                );
            }
        }
    }

    // v0.6.0.0: fire webhook subscribers on successful store. Best-effort
    // fire-and-forget — each subscriber gets its own OS thread; the
    // response here does not wait on any webhook dispatch.
    crate::subscriptions::dispatch_event(
        conn,
        "memory_store",
        &actual_id,
        &mem.namespace,
        Some(&agent_id),
        db_path,
    );

    // #196: echo the resolved agent_id
    let mut response = json!({
        "id": actual_id,
        "tier": mem.tier,
        "title": mem.title,
        "namespace": mem.namespace,
        "agent_id": agent_id,
    });
    if !contradiction_ids.is_empty() {
        response["potential_contradictions"] = json!(contradiction_ids);
    }
    if !auto_tags.is_empty() {
        response["auto_tags"] = json!(auto_tags);
    }
    if !confirmed_contradictions.is_empty() {
        response["confirmed_contradictions"] = json!(confirmed_contradictions);
    }
    if let Some(reason) = hooks_skipped_reason
        && autonomous_hooks
    {
        response["autonomy_hook_skipped"] = json!(reason);
    }
    Ok(response)
}
