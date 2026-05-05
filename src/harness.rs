// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 Track B (B4) — Harness detection from MCP `clientInfo.name`.
//!
//! When an MCP client opens a JSON-RPC `initialize` handshake, it sends
//! a `clientInfo` object with a `name` field that identifies the
//! harness — e.g. `"claude-code"`, `"codex"`, `"cursor"`, `"cline"`.
//! The substrate captures that string at handshake time
//! (`src/mcp.rs::serve_stdio` already stashes it as `mcp_client_name`)
//! and then needs to make a behavioural decision: does this harness
//! surface tools registered *after* the initial `tools/list` to the
//! LLM, or does it cache the manifest at session start?
//!
//! Track B's runtime loaders (B1 `memory_load_family`, B2
//! `memory_smart_load`) only deliver value on harnesses with deferred
//! registration — on eager-load harnesses (Codex, Cursor, etc.) they
//! still return the schemas, but the LLM has to know it should ask the
//! operator to restart with `--profile <family>` rather than expect
//! the new tools to appear mid-session. The harness layer carries
//! that bit so the loaders, the capabilities-v3 response, and the
//! `to_invoke` helper text can all agree on the contract per harness.
//!
//! # Compatibility matrix
//!
//! Source of truth: `docs/v0.7/compatibility-matrix.html` (shipped
//! with Track D2). Today only Claude Code supports deferred-tool
//! registration via its `ToolSearch` mechanism. Every other harness
//! defaults to **false** (conservative) so the LLM doesn't promise an
//! end-user that a tool will appear mid-session and then strand the
//! conversation when the cached manifest never gets refreshed.
//!
//! Unknown harnesses (`Generic(name)`) also default to false. If an
//! operator runs the substrate behind a custom MCP client that *does*
//! support deferred registration, they can either (a) add the harness
//! name to `Harness::detect`'s match arms or (b) wait for a future
//! release that exposes a runtime override (out of scope for B4).
//!
//! # Wire surface
//!
//! The detected harness's `supports_deferred_registration()` value is
//! surfaced verbatim in the v3 capabilities response as the top-level
//! field `your_harness_supports_deferred_registration` (boolean). When
//! no `clientInfo` was provided (e.g. an HTTP caller or a malformed
//! handshake), the field is omitted (`Option::None` +
//! `skip_serializing_if`) so legacy callers see no schema drift.
//!
//! # Example
//!
//! ```
//! use ai_memory::harness::Harness;
//!
//! // Fuzzy + case-insensitive substring matching.
//! assert_eq!(Harness::detect("claude-code"), Harness::ClaudeCode);
//! assert_eq!(Harness::detect("Claude Code"), Harness::ClaudeCode);
//! assert_eq!(Harness::detect("claude_code"), Harness::ClaudeCode);
//!
//! assert!(Harness::ClaudeCode.supports_deferred_registration());
//! assert!(!Harness::Codex.supports_deferred_registration());
//! ```

use serde::{Deserialize, Serialize};

/// MCP harness detected from the `initialize.clientInfo.name` field.
///
/// The variants cover the harnesses called out in
/// `docs/v0.7/compatibility-matrix.html` (Track D2). Unknown harnesses
/// fall through to `Generic(String)` carrying the original
/// `clientInfo.name` so downstream logging / metrics can attribute
/// behaviour to the unrecognised client without losing the name.
///
/// `serde` uses `snake_case` so the wire shape matches the rest of the
/// v3 capabilities document. `Generic` carries an inner string and
/// serialises as `{"generic": "<name>"}` per serde's default
/// externally-tagged enum representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    /// Anthropic Claude Code — supports deferred-tool registration via
    /// `ToolSearch`. The only first-class harness today where B1's
    /// `memory_load_family` actually surfaces new tools mid-session.
    ClaudeCode,
    /// OpenAI Codex CLI — eager-load only.
    Codex,
    /// Anysphere Cursor — MCP via stdio, eager-load only.
    Cursor,
    /// VS Code Cline extension — MCP via stdio, eager-load only.
    Cline,
    /// Continue.dev (VS Code / JetBrains) — MCP via stdio, eager-load only.
    Continue,
    /// Aider CLI pair-programmer — MCP via stdio, eager-load only.
    Aider,
    /// Block / Square Goose — MCP via stdio, eager-load only.
    Goose,
    /// Anthropic Claude Desktop — eager-load only.
    ClaudeDesktop,
    /// Unknown harness; carries the original `clientInfo.name` so the
    /// operator can grep logs for "this is the harness I forgot to
    /// register" without losing the identifying string.
    Generic(String),
}

impl Harness {
    /// Detect a harness from the `clientInfo.name` field of the MCP
    /// `initialize` request.
    ///
    /// Matching is **case-insensitive** and **fuzzy substring** — the
    /// raw name is normalised by lower-casing and stripping the
    /// punctuation harnesses use as separators (`-`, `_`, ` `,
    /// `.`) before comparison. So `"claude-code"`, `"Claude Code"`,
    /// `"claude_code"`, and `"CLAUDE.CODE"` all detect as
    /// `Harness::ClaudeCode`.
    ///
    /// Unknown names round-trip into `Generic(<original>)` preserving
    /// the input verbatim so logging / metrics keep a useful label.
    #[must_use]
    pub fn detect(client_name: &str) -> Self {
        let normalised = client_name
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' ' | '.'))
            .flat_map(char::to_lowercase)
            .collect::<String>();

        // Order matters: `claudecode` and `claudedesktop` both contain
        // `claude`, so the more-specific match must come first. The
        // substring check is `contains`, so `claudecodecli` (a
        // hypothetical wrapper) still detects as ClaudeCode.
        if normalised.contains("claudecode") {
            Self::ClaudeCode
        } else if normalised.contains("claudedesktop") {
            Self::ClaudeDesktop
        } else if normalised.contains("codex") {
            Self::Codex
        } else if normalised.contains("cursor") {
            Self::Cursor
        } else if normalised.contains("cline") {
            Self::Cline
        } else if normalised.contains("continue") {
            Self::Continue
        } else if normalised.contains("aider") {
            Self::Aider
        } else if normalised.contains("goose") {
            Self::Goose
        } else {
            Self::Generic(client_name.to_string())
        }
    }

    /// Whether this harness exposes tools registered *after* the
    /// initial `tools/list` to the LLM mid-session.
    ///
    /// `true` only for harnesses with documented deferred-tool
    /// registration support. Today that's just Claude Code via its
    /// `ToolSearch` mechanism. Every other known harness eager-loads
    /// the manifest at session start and won't surface a tool added
    /// later — so B1's `memory_load_family` falls back to a
    /// "restart with `--profile <family>`" hint on those harnesses.
    ///
    /// **Default for unknown harnesses (`Generic`)**: `false`
    /// (conservative). It's better to under-promise than to claim a
    /// tool will appear and have the conversation strand on a stale
    /// cached manifest.
    #[must_use]
    pub fn supports_deferred_registration(&self) -> bool {
        match self {
            Self::ClaudeCode => true,
            Self::Codex
            | Self::Cursor
            | Self::Cline
            | Self::Continue
            | Self::Aider
            | Self::Goose
            | Self::ClaudeDesktop
            | Self::Generic(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // detect() — Claude Code matches under every common spelling.
    // -----------------------------------------------------------------
    #[test]
    fn detect_claude_code_canonical_kebab() {
        assert_eq!(Harness::detect("claude-code"), Harness::ClaudeCode);
    }

    #[test]
    fn detect_claude_code_title_case_with_space() {
        assert_eq!(Harness::detect("Claude Code"), Harness::ClaudeCode);
    }

    #[test]
    fn detect_claude_code_snake_case() {
        assert_eq!(Harness::detect("claude_code"), Harness::ClaudeCode);
    }

    #[test]
    fn detect_claude_code_screaming_with_dots() {
        assert_eq!(Harness::detect("CLAUDE.CODE"), Harness::ClaudeCode);
    }

    #[test]
    fn detect_claude_code_versioned_suffix() {
        // A real-world harness sometimes ships its name as
        // `claude-code/1.2.3` or `claude-code-cli`; substring match
        // catches both without per-version updates.
        assert_eq!(Harness::detect("claude-code-cli"), Harness::ClaudeCode);
        assert_eq!(Harness::detect("claude-code/1.2.3"), Harness::ClaudeCode);
    }

    // -----------------------------------------------------------------
    // detect() — every other named harness round-trips.
    // -----------------------------------------------------------------
    #[test]
    fn detect_codex_variants() {
        assert_eq!(Harness::detect("codex"), Harness::Codex);
        assert_eq!(Harness::detect("Codex"), Harness::Codex);
        assert_eq!(Harness::detect("codex-cli"), Harness::Codex);
        assert_eq!(Harness::detect("openai-codex"), Harness::Codex);
    }

    #[test]
    fn detect_cursor_variants() {
        assert_eq!(Harness::detect("cursor"), Harness::Cursor);
        assert_eq!(Harness::detect("Cursor"), Harness::Cursor);
        assert_eq!(Harness::detect("cursor-mcp"), Harness::Cursor);
    }

    #[test]
    fn detect_cline_variants() {
        assert_eq!(Harness::detect("cline"), Harness::Cline);
        assert_eq!(Harness::detect("Cline"), Harness::Cline);
        assert_eq!(Harness::detect("vscode-cline"), Harness::Cline);
    }

    #[test]
    fn detect_continue_variants() {
        assert_eq!(Harness::detect("continue"), Harness::Continue);
        assert_eq!(Harness::detect("Continue"), Harness::Continue);
        assert_eq!(Harness::detect("continue.dev"), Harness::Continue);
    }

    #[test]
    fn detect_aider_variants() {
        assert_eq!(Harness::detect("aider"), Harness::Aider);
        assert_eq!(Harness::detect("Aider"), Harness::Aider);
        assert_eq!(Harness::detect("aider-cli"), Harness::Aider);
    }

    #[test]
    fn detect_goose_variants() {
        assert_eq!(Harness::detect("goose"), Harness::Goose);
        assert_eq!(Harness::detect("Goose"), Harness::Goose);
        assert_eq!(Harness::detect("block-goose"), Harness::Goose);
    }

    #[test]
    fn detect_claude_desktop_variants() {
        // Claude Desktop must NOT be misclassified as ClaudeCode even
        // though its name is a superstring of "claude". The match
        // ordering in `detect()` checks `claudecode` first.
        assert_eq!(Harness::detect("claude-desktop"), Harness::ClaudeDesktop);
        assert_eq!(Harness::detect("Claude Desktop"), Harness::ClaudeDesktop);
        assert_eq!(Harness::detect("ClaudeDesktop"), Harness::ClaudeDesktop);
    }

    // -----------------------------------------------------------------
    // detect() — unknown names round-trip into Generic preserving
    // the original string verbatim (no normalisation, no truncation).
    // -----------------------------------------------------------------
    #[test]
    fn detect_unknown_preserves_original_name() {
        let raw = "MyCustomMcpClient/0.1";
        let h = Harness::detect(raw);
        match h {
            Harness::Generic(s) => assert_eq!(s, raw),
            other => panic!("expected Generic; got {other:?}"),
        }
    }

    #[test]
    fn detect_empty_name_is_generic() {
        // An empty clientInfo.name is malformed but defensively
        // mapped to Generic("") rather than panicking.
        assert_eq!(Harness::detect(""), Harness::Generic(String::new()));
    }

    // -----------------------------------------------------------------
    // supports_deferred_registration — compat matrix per docs/v0.7.
    // -----------------------------------------------------------------
    #[test]
    fn deferred_registration_only_claude_code_today() {
        assert!(Harness::ClaudeCode.supports_deferred_registration());

        assert!(!Harness::Codex.supports_deferred_registration());
        assert!(!Harness::Cursor.supports_deferred_registration());
        assert!(!Harness::Cline.supports_deferred_registration());
        assert!(!Harness::Continue.supports_deferred_registration());
        assert!(!Harness::Aider.supports_deferred_registration());
        assert!(!Harness::Goose.supports_deferred_registration());
        assert!(!Harness::ClaudeDesktop.supports_deferred_registration());
    }

    #[test]
    fn deferred_registration_unknown_defaults_false() {
        // Conservative: unknown harnesses default to false so we
        // never promise mid-session tool surfacing we can't deliver.
        assert!(
            !Harness::Generic("some-random-mcp-client".to_string())
                .supports_deferred_registration()
        );
        assert!(!Harness::Generic(String::new()).supports_deferred_registration());
    }

    // -----------------------------------------------------------------
    // serde — round-trip through JSON for the wire shape.
    // -----------------------------------------------------------------
    #[test]
    fn serde_round_trips_named_variants() {
        let h = Harness::ClaudeCode;
        let s = serde_json::to_string(&h).expect("serialize");
        assert_eq!(s, "\"claude_code\"");
        let back: Harness = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, Harness::ClaudeCode);
    }

    #[test]
    fn serde_round_trips_generic_variant() {
        let h = Harness::Generic("foo".to_string());
        let s = serde_json::to_string(&h).expect("serialize");
        let back: Harness = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, h);
    }
}
