// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — Task G4: hook decision contract.
//
// G3 (PR #567) shipped a *local* prototype of `HookDecision` in
// `src/hooks/executor.rs` with only `Allow` + `Deny` so the
// subprocess executor had something to deserialize against. G4
// lifts the type into this dedicated module and adds the two
// remaining variants the v0.7 epic calls for: `Modify(MemoryDelta)`
// (a pre-event-only delta the executor merges back into the
// in-flight payload) and `AskUser` (an interactive prompt the
// chain runner G5 will fan out to the operator surface).
//
// # JSON wire contract
//
// Every decision is a single JSON object with an `action`
// discriminator. The exact shapes:
//
// ```json
// {"action": "allow"}
// {"action": "modify", "delta": {...}}
// {"action": "deny",   "reason": "redact required", "code": 403}
// {"action": "ask_user", "prompt": "...", "options": ["yes","no"], "default": "no"}
// ```
//
// * `allow` carries no fields; an empty `{}` payload (or empty
//   stdout) is *also* treated as `Allow` so a no-op observability
//   hook can `print("{}\n")` from any language and stay correct.
// * `modify` requires a `delta` field. The delta type is
//   [`crate::hooks::events::MemoryDelta`] — every field is optional
//   so a hook may rewrite only what it cares about.
// * `deny` requires a `reason`; `code` defaults to `403` if the
//   hook omits it (matches the G3 prototype's behaviour).
// * `ask_user` requires `prompt` and `options`; `default` is
//   optional and names one of `options`. The chain runner (G5)
//   surfaces `AskUser` to the operator and resumes the chain
//   once the human picks an option.
//
// Unknown `action` strings, missing required fields, and trailing
// junk are all rejected with [`DecisionParseError`]. The executor
// surfaces those as a `tracing::warn!("hook returned malformed
// decision")` and degrades to `Allow` so a buggy hook can't
// brick the request path — the bias is "fail open, log loudly".
//
// # Pre-event-only `Modify` validation
//
// `Modify` only makes sense for pre- events: post- events report
// what *already happened*, so there's nothing for a delta to
// rewrite. The epic offered a choice between a compile-time guard
// (separate types per pre/post) and a runtime guard in the
// dispatcher. We picked the runtime guard:
//
//   * The compile-time path would fork the `HookDecision` type
//     into `PreHookDecision` / `PostHookDecision`, double the
//     surface area on every executor + chain method, and force
//     callers to know an event's pre/post-ness at call sites that
//     today take an opaque `HookEvent` tag.
//   * The runtime path is a single function call —
//     [`HookDecision::degrade_modify_for_post_event`] — that the
//     dispatcher invokes after parsing the child's response. If a
//     hook returns `Modify` for a post- event we log a warning
//     and treat it as `Allow`. Same fail-open posture as the
//     malformed-payload path.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::events::{HookEvent, MemoryDelta};

// ---------------------------------------------------------------------------
// HookDecision — full G4 enum
// ---------------------------------------------------------------------------

/// The four decision shapes a hook subprocess may return.
///
/// See the module-level documentation for the JSON wire contract
/// and the runtime validation rules.
///
/// `PartialEq` is hand-rolled rather than derived because
/// [`MemoryDelta`] (the inner of `Modify`) holds a
/// `serde_json::Value` and `Value` is not itself `Eq`. Equality
/// for `Modify` falls back to a JSON-canonical comparison so
/// tests can assert structural equality without caring about
/// field ordering inside the metadata bag.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HookDecision {
    /// Continue the memory operation unchanged. Wire shape:
    /// `{"action":"allow"}` (or empty `{}` / empty stdout).
    Allow,
    /// Rewrite the in-flight payload before the memory operation
    /// runs. Only valid on pre- events; on post- events the
    /// dispatcher logs a warning and degrades to `Allow`.
    Modify(ModifyPayload),
    /// Halt the memory operation. `reason` surfaces in the
    /// operator log and (when G7+ wires the executor into the
    /// request path) the API response. `code` is an HTTP-style
    /// integer the API surface translates to a status code.
    Deny {
        reason: String,
        #[serde(default = "default_deny_code")]
        code: i32,
    },
    /// Pause the chain and surface `prompt` to the operator
    /// alongside `options`. The chain runner (G5) resumes once
    /// the human picks one. `default` (if present) names the
    /// option the runner falls back to on operator timeout.
    AskUser {
        prompt: String,
        options: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
}

/// Payload wrapper for [`HookDecision::Modify`]. The wire shape
/// is `{"action":"modify","delta":{...}}`, so the inner field is
/// named `delta` rather than letting serde flatten the
/// [`MemoryDelta`] fields onto the decision object — keeping the
/// delta nested means future expansions (extra metadata, hook
/// trace ids) won't collide with `MemoryDelta` field names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModifyPayload {
    pub delta: MemoryDelta,
}

impl PartialEq for HookDecision {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (HookDecision::Allow, HookDecision::Allow) => true,
            (HookDecision::Modify(a), HookDecision::Modify(b)) => {
                // MemoryDelta carries a serde_json::Value (metadata)
                // which is not Eq; compare via canonical JSON.
                serde_json::to_value(&a.delta).ok() == serde_json::to_value(&b.delta).ok()
            }
            (
                HookDecision::Deny {
                    reason: r1,
                    code: c1,
                },
                HookDecision::Deny {
                    reason: r2,
                    code: c2,
                },
            ) => r1 == r2 && c1 == c2,
            (
                HookDecision::AskUser {
                    prompt: p1,
                    options: o1,
                    default: d1,
                },
                HookDecision::AskUser {
                    prompt: p2,
                    options: o2,
                    default: d2,
                },
            ) => p1 == p2 && o1 == o2 && d1 == d2,
            _ => false,
        }
    }
}

fn default_deny_code() -> i32 {
    403
}

// ---------------------------------------------------------------------------
// DecisionParseError — strict deserialization errors
// ---------------------------------------------------------------------------

/// Errors surfaced by [`HookDecision::parse`]. Hand-rolled
/// `Display + Error` per the v0.7 lesson (no `thiserror` in this
/// crate's hot dependency tree).
///
/// Each variant is intentionally narrow so the executor's warning
/// log can name the failure mode (`unknown action "foo"` vs
/// `missing required field "reason"`).
#[derive(Debug)]
pub enum DecisionParseError {
    /// The payload was not a JSON object (e.g. an array or scalar).
    NotAnObject,
    /// The payload was a JSON object but had no `action` key. This
    /// is *not* the same as an empty `{}` — empty objects are
    /// treated as `Allow` per the wire contract. `NotAnObject`
    /// fires only when the bytes parse as JSON but `action` is
    /// missing on a non-empty object.
    MissingAction,
    /// The `action` discriminator named a string we don't recognise.
    UnknownAction(String),
    /// The decision shape is recognised but a required field is
    /// missing (`Deny` without `reason`, `Modify` without `delta`,
    /// `AskUser` without `prompt` or `options`).
    MissingField {
        action: &'static str,
        field: &'static str,
    },
    /// Underlying JSON syntax / type error from `serde_json`.
    Malformed(String),
}

impl std::fmt::Display for DecisionParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecisionParseError::NotAnObject => {
                write!(f, "hook decision must be a JSON object")
            }
            DecisionParseError::MissingAction => {
                write!(f, "hook decision missing required \"action\" field")
            }
            DecisionParseError::UnknownAction(a) => {
                write!(f, "hook decision has unknown action \"{a}\"")
            }
            DecisionParseError::MissingField { action, field } => {
                write!(
                    f,
                    "hook decision action=\"{action}\" missing required field \"{field}\""
                )
            }
            DecisionParseError::Malformed(msg) => {
                write!(f, "hook decision malformed: {msg}")
            }
        }
    }
}

impl std::error::Error for DecisionParseError {}

// ---------------------------------------------------------------------------
// HookDecision — parsing + runtime validation
// ---------------------------------------------------------------------------

impl HookDecision {
    /// Parse a decision payload from a hook subprocess.
    ///
    /// An empty / whitespace-only line and a literal `{}` are both
    /// treated as `Allow` per the wire contract — see the
    /// module-level documentation. Anything else is parsed
    /// strictly: unknown actions, missing required fields, and
    /// non-object payloads all return a [`DecisionParseError`].
    ///
    /// # Errors
    ///
    /// Returns [`DecisionParseError`] when the payload is not a
    /// JSON object, when `action` is unknown, when a required
    /// field is missing, or when the JSON itself is syntactically
    /// invalid.
    pub fn parse(line: &str) -> Result<Self, DecisionParseError> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "{}" {
            return Ok(HookDecision::Allow);
        }

        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| DecisionParseError::Malformed(e.to_string()))?;
        let obj = value.as_object().ok_or(DecisionParseError::NotAnObject)?;

        // Empty object after parse — same fail-open semantics as
        // the literal "{}" short-circuit above.
        if obj.is_empty() {
            return Ok(HookDecision::Allow);
        }

        let action = obj
            .get("action")
            .ok_or(DecisionParseError::MissingAction)?
            .as_str()
            .ok_or_else(|| DecisionParseError::Malformed("\"action\" must be a string".into()))?;

        match action {
            "allow" => Ok(HookDecision::Allow),
            "modify" => {
                let delta_v = obj.get("delta").ok_or(DecisionParseError::MissingField {
                    action: "modify",
                    field: "delta",
                })?;
                let delta: MemoryDelta = serde_json::from_value(delta_v.clone())
                    .map_err(|e| DecisionParseError::Malformed(e.to_string()))?;
                Ok(HookDecision::Modify(ModifyPayload { delta }))
            }
            "deny" => {
                let reason = obj
                    .get("reason")
                    .ok_or(DecisionParseError::MissingField {
                        action: "deny",
                        field: "reason",
                    })?
                    .as_str()
                    .ok_or_else(|| {
                        DecisionParseError::Malformed("\"reason\" must be a string".into())
                    })?
                    .to_string();
                let code = obj
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .map_or_else(default_deny_code, |c| {
                        i32::try_from(c).unwrap_or(default_deny_code())
                    });
                Ok(HookDecision::Deny { reason, code })
            }
            "ask_user" => {
                let prompt = obj
                    .get("prompt")
                    .ok_or(DecisionParseError::MissingField {
                        action: "ask_user",
                        field: "prompt",
                    })?
                    .as_str()
                    .ok_or_else(|| {
                        DecisionParseError::Malformed("\"prompt\" must be a string".into())
                    })?
                    .to_string();
                let options_v = obj.get("options").ok_or(DecisionParseError::MissingField {
                    action: "ask_user",
                    field: "options",
                })?;
                let options: Vec<String> = serde_json::from_value(options_v.clone())
                    .map_err(|e| DecisionParseError::Malformed(e.to_string()))?;
                let default = match obj.get("default") {
                    None => None,
                    Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_str()
                            .ok_or_else(|| {
                                DecisionParseError::Malformed("\"default\" must be a string".into())
                            })?
                            .to_string(),
                    ),
                };
                Ok(HookDecision::AskUser {
                    prompt,
                    options,
                    default,
                })
            }
            other => Err(DecisionParseError::UnknownAction(other.to_string())),
        }
    }

    /// Runtime guard for the pre-event-only constraint on
    /// `Modify`. If `self` is `Modify` and `event` is a post-
    /// event, log a warning and return `Allow`. Otherwise return
    /// `self` unchanged.
    ///
    /// The dispatcher (G5) calls this after parsing the child's
    /// decision but before applying the delta, so a misbehaving
    /// hook can't sneak a `Modify` past a post- event.
    #[must_use]
    pub fn degrade_modify_for_post_event(self, event: HookEvent) -> Self {
        if matches!(self, HookDecision::Modify(_)) && !is_pre_event(event) {
            tracing::warn!(
                event = ?event,
                "hooks: Modify decision returned for post- event; degrading to Allow"
            );
            return HookDecision::Allow;
        }
        self
    }
}

/// Returns `true` if `event` is a pre- variant (i.e. fires before
/// the underlying memory operation runs).
///
/// Lives next to [`HookDecision::degrade_modify_for_post_event`]
/// because the runtime guard is the only consumer today; G5's
/// chain runner will reach for it the same way when wiring
/// `Modify` accumulation through the pipeline.
#[must_use]
pub fn is_pre_event(event: HookEvent) -> bool {
    matches!(
        event,
        HookEvent::PreStore
            | HookEvent::PreRecall
            | HookEvent::PreSearch
            | HookEvent::PreDelete
            | HookEvent::PrePromote
            | HookEvent::PreLink
            | HookEvent::PreConsolidate
            | HookEvent::PreGovernanceDecision
            | HookEvent::PreArchive
            | HookEvent::PreTranscriptStore
    )
}

// ---------------------------------------------------------------------------
// Custom Deserialize — strict, named errors
// ---------------------------------------------------------------------------

impl<'de> Deserialize<'de> for HookDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Funnel through `parse` so the strict-validation path is
        // the same one the executor uses on stdout. Any
        // [`DecisionParseError`] becomes a serde custom error.
        let value = Value::deserialize(deserializer)?;
        let as_text = serde_json::to_string(&value).map_err(serde::de::Error::custom)?;
        HookDecision::parse(&as_text).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- Round-trip per variant -------------------------------------------

    #[test]
    fn allow_round_trips() {
        let d = HookDecision::Allow;
        let json = serde_json::to_string(&d).expect("encode");
        assert_eq!(json, r#"{"action":"allow"}"#);
        let back: HookDecision = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, HookDecision::Allow);
    }

    #[test]
    fn modify_round_trips_with_delta() {
        let delta = MemoryDelta {
            tags: Some(vec!["redacted".into()]),
            priority: Some(5),
            ..Default::default()
        };
        let d = HookDecision::Modify(ModifyPayload {
            delta: delta.clone(),
        });
        let json = serde_json::to_string(&d).expect("encode");
        // Wire shape sanity: action + delta nested.
        let v: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["action"], json!("modify"));
        assert_eq!(v["delta"]["tags"], json!(["redacted"]));
        assert_eq!(v["delta"]["priority"], json!(5));

        let back: HookDecision = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, HookDecision::Modify(ModifyPayload { delta }));
    }

    #[test]
    fn deny_round_trips_with_explicit_code() {
        let d = HookDecision::Deny {
            reason: "redact required".into(),
            code: 451,
        };
        let json = serde_json::to_string(&d).expect("encode");
        let back: HookDecision = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, d);
    }

    #[test]
    fn deny_default_code_when_omitted() {
        let d = HookDecision::parse(r#"{"action":"deny","reason":"nope"}"#).expect("parse");
        match d {
            HookDecision::Deny { reason, code } => {
                assert_eq!(reason, "nope");
                assert_eq!(code, 403, "missing code defaults to 403");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_round_trips() {
        let d = HookDecision::AskUser {
            prompt: "Promote to long-term?".into(),
            options: vec!["yes".into(), "no".into()],
            default: Some("no".into()),
        };
        let json = serde_json::to_string(&d).expect("encode");
        let v: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["action"], json!("ask_user"));
        assert_eq!(v["options"], json!(["yes", "no"]));
        assert_eq!(v["default"], json!("no"));

        let back: HookDecision = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, d);
    }

    #[test]
    fn ask_user_default_optional() {
        let raw = r#"{"action":"ask_user","prompt":"continue?","options":["a","b"]}"#;
        let d = HookDecision::parse(raw).expect("parse");
        match d {
            HookDecision::AskUser {
                prompt,
                options,
                default,
            } => {
                assert_eq!(prompt, "continue?");
                assert_eq!(options, vec!["a".to_string(), "b".to_string()]);
                assert!(default.is_none());
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    // ---- Allow shorthand (empty payload) ----------------------------------

    #[test]
    fn empty_payload_treated_as_allow() {
        assert_eq!(HookDecision::parse("").unwrap(), HookDecision::Allow);
        assert_eq!(HookDecision::parse("   ").unwrap(), HookDecision::Allow);
        assert_eq!(HookDecision::parse("{}").unwrap(), HookDecision::Allow);
        assert_eq!(HookDecision::parse("{ }").unwrap(), HookDecision::Allow);
    }

    // ---- Strict-validation error surface ----------------------------------

    #[test]
    fn unknown_action_rejected_with_named_error() {
        let err = HookDecision::parse(r#"{"action":"explode"}"#).unwrap_err();
        match err {
            DecisionParseError::UnknownAction(a) => assert_eq!(a, "explode"),
            other => panic!("expected UnknownAction, got {other:?}"),
        }
    }

    #[test]
    fn missing_action_rejected() {
        let err = HookDecision::parse(r#"{"reason":"why"}"#).unwrap_err();
        assert!(matches!(err, DecisionParseError::MissingAction));
    }

    #[test]
    fn deny_missing_reason_rejected() {
        let err = HookDecision::parse(r#"{"action":"deny"}"#).unwrap_err();
        match err {
            DecisionParseError::MissingField { action, field } => {
                assert_eq!(action, "deny");
                assert_eq!(field, "reason");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn modify_missing_delta_rejected() {
        let err = HookDecision::parse(r#"{"action":"modify"}"#).unwrap_err();
        match err {
            DecisionParseError::MissingField { action, field } => {
                assert_eq!(action, "modify");
                assert_eq!(field, "delta");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_missing_prompt_rejected() {
        let err = HookDecision::parse(r#"{"action":"ask_user","options":["a"]}"#).unwrap_err();
        match err {
            DecisionParseError::MissingField { action, field } => {
                assert_eq!(action, "ask_user");
                assert_eq!(field, "prompt");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_missing_options_rejected() {
        let err = HookDecision::parse(r#"{"action":"ask_user","prompt":"?"}"#).unwrap_err();
        match err {
            DecisionParseError::MissingField { action, field } => {
                assert_eq!(action, "ask_user");
                assert_eq!(field, "options");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn non_object_payload_rejected() {
        let err = HookDecision::parse(r#"["allow"]"#).unwrap_err();
        assert!(matches!(err, DecisionParseError::NotAnObject));
    }

    #[test]
    fn malformed_json_rejected() {
        let err = HookDecision::parse(r"not json at all").unwrap_err();
        assert!(matches!(err, DecisionParseError::Malformed(_)));
    }

    // ---- Modify-on-post-event runtime guard --------------------------------

    /// Stand-in for G5's dispatcher: parses a decision, then runs
    /// the runtime guard. This is the harness the executor will
    /// reach for once the chain runner lands.
    fn dispatch(event: HookEvent, raw: &str) -> HookDecision {
        let parsed = HookDecision::parse(raw).expect("parse");
        parsed.degrade_modify_for_post_event(event)
    }

    #[test]
    fn modify_on_pre_event_passes_through() {
        let raw = r#"{"action":"modify","delta":{"priority":9}}"#;
        let d = dispatch(HookEvent::PreStore, raw);
        match d {
            HookDecision::Modify(m) => assert_eq!(m.delta.priority, Some(9)),
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn modify_on_post_event_degrades_to_allow() {
        let raw = r#"{"action":"modify","delta":{"priority":9}}"#;
        // PostStore is a post- event — Modify must degrade.
        assert_eq!(
            dispatch(HookEvent::PostStore, raw),
            HookDecision::Allow,
            "Modify on post_store must degrade to Allow"
        );
        assert_eq!(
            dispatch(HookEvent::PostRecall, raw),
            HookDecision::Allow,
            "Modify on post_recall must degrade to Allow"
        );
        assert_eq!(
            dispatch(HookEvent::OnIndexEviction, raw),
            HookDecision::Allow,
            "Modify on on_index_eviction must degrade to Allow"
        );
    }

    #[test]
    fn allow_on_post_event_unchanged() {
        // The guard only touches Modify.
        assert_eq!(
            dispatch(HookEvent::PostStore, r#"{"action":"allow"}"#),
            HookDecision::Allow
        );
    }

    #[test]
    fn deny_on_post_event_unchanged() {
        let raw = r#"{"action":"deny","reason":"x","code":500}"#;
        assert_eq!(
            dispatch(HookEvent::PostStore, raw),
            HookDecision::Deny {
                reason: "x".into(),
                code: 500
            }
        );
    }

    // ---- is_pre_event coverage --------------------------------------------

    #[test]
    fn is_pre_event_classifies_all_variants() {
        // Pre- variants
        for ev in [
            HookEvent::PreStore,
            HookEvent::PreRecall,
            HookEvent::PreSearch,
            HookEvent::PreDelete,
            HookEvent::PrePromote,
            HookEvent::PreLink,
            HookEvent::PreConsolidate,
            HookEvent::PreGovernanceDecision,
            HookEvent::PreArchive,
            HookEvent::PreTranscriptStore,
        ] {
            assert!(is_pre_event(ev), "expected {ev:?} to be a pre- event");
        }
        // Post- + on- variants
        for ev in [
            HookEvent::PostStore,
            HookEvent::PostRecall,
            HookEvent::PostSearch,
            HookEvent::PostDelete,
            HookEvent::PostPromote,
            HookEvent::PostLink,
            HookEvent::PostConsolidate,
            HookEvent::PostGovernanceDecision,
            HookEvent::OnIndexEviction,
            HookEvent::PostTranscriptStore,
        ] {
            assert!(!is_pre_event(ev), "expected {ev:?} to be a post-/on- event");
        }
    }

    // ---- Display surface for DecisionParseError ----------------------------

    #[test]
    fn parse_error_display_is_descriptive() {
        let cases = [
            DecisionParseError::NotAnObject,
            DecisionParseError::MissingAction,
            DecisionParseError::UnknownAction("foo".into()),
            DecisionParseError::MissingField {
                action: "deny",
                field: "reason",
            },
            DecisionParseError::Malformed("expected `,`".into()),
        ];
        for e in &cases {
            let s = e.to_string();
            assert!(!s.is_empty(), "Display empty for {e:?}");
            assert!(
                s.contains("hook decision"),
                "Display missing context for {e:?}: {s}"
            );
        }
    }
}
