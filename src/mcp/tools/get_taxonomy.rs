// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_get_taxonomy` handler.

use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_get_taxonomy(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    // Defaults match the JSON schema. Trailing '/' is forgiven so MCP
    // clients can pass either `"alpha"` or `"alpha/"` without an extra
    // round trip — the underlying validate_namespace rejects the
    // trailing slash form, so we strip it before validating.
    let prefix_raw = params
        .get("namespace_prefix")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let prefix_owned: Option<String> = prefix_raw.map(|s| s.trim_end_matches('/').to_string());
    if let Some(p) = prefix_owned.as_deref() {
        validate::validate_namespace(p).map_err(|e| e.to_string())?;
    }
    let depth = usize::try_from(params.get("depth").and_then(Value::as_u64).unwrap_or(8))
        .unwrap_or(usize::MAX)
        .min(crate::models::MAX_NAMESPACE_DEPTH);
    let limit = usize::try_from(params.get("limit").and_then(Value::as_u64).unwrap_or(1000))
        .unwrap_or(usize::MAX)
        .clamp(1, 10_000);

    let tax =
        db::get_taxonomy(conn, prefix_owned.as_deref(), depth, limit).map_err(|e| e.to_string())?;
    Ok(json!({
        "tree": tax.tree,
        "total_count": tax.total_count,
        "truncated": tax.truncated,
    }))
}
