// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-1 — file-backed reflection chain export.
//!
//! Ships the `ai-memory export-reflections` CLI subcommand so an
//! operator can `cat ~/.ai-memory/reflections/<namespace>/<id>.md`
//! and read what the substrate has synthesised without learning SQL.
//!
//! # Wire shape
//!
//! ```bash
//! ai-memory export-reflections \
//!     --namespace team/alpha \
//!     --out-dir ~/.ai-memory/reflections \
//!     --format md \
//!     --since 2026-05-01T00:00:00Z \
//!     --quiet
//! ```
//!
//! The substrate is the source of truth: the SQL row is authoritative,
//! the file on disk is a derived artefact. Operators may freely
//! delete / regenerate the directory at any time.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use serde::Serialize;

use crate::cli::CliOutput;
use crate::db;
use crate::models::{Memory, MemoryKind};

/// CLI args for `ai-memory export-reflections`.
#[derive(Args, Debug, Clone)]
pub struct ExportReflectionsArgs {
    /// Restrict the export to reflections under this namespace.
    /// When omitted, every reflection memory is exported (one
    /// subdirectory per namespace under `--out-dir`).
    #[arg(long, value_name = "NS")]
    pub namespace: Option<String>,

    /// Output directory root. Defaults to `~/.ai-memory/reflections/`.
    /// The directory is created if it does not exist.
    #[arg(long, value_name = "PATH")]
    pub out_dir: Option<PathBuf>,

    /// Export format. `md` (default) writes a YAML-frontmatter
    /// markdown file per reflection. `json` writes a structured
    /// JSON envelope per reflection.
    #[arg(long, default_value = "md", value_name = "FMT")]
    pub format: String,

    /// Only export reflections created at or after this RFC3339
    /// instant. Pre-existing reflections are skipped.
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,

    /// Suppress per-file output; only emit the final count line.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

/// Result of one export-reflections run — returned so unit tests can
/// assert on counts without re-parsing stdout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Reflections written to disk.
    pub written: usize,
    /// Reflections matched but skipped (already present, etc.).
    pub skipped: usize,
}

/// The "json" format envelope. Mirrors the fields surfaced in the
/// markdown frontmatter so the two outputs carry the same provenance.
#[derive(Debug, Serialize)]
struct JsonEnvelope<'a> {
    memory_id: &'a str,
    namespace: &'a str,
    title: &'a str,
    reflection_depth: i32,
    attest_level: &'a str,
    created_at: &'a str,
    agent_id: &'a str,
    reflects_on: Vec<String>,
    content: &'a str,
}

/// Dispatch entry-point called from `daemon_runtime::run`.
///
/// # Errors
///
/// Propagates DB / I/O errors. Returns `Ok(0)` on success, `Ok(non-zero)`
/// for non-fatal anomalies (e.g. unsupported format) so the harness can
/// map the exit code without `Err` unwinding tripping the post-run
/// WAL checkpoint.
pub fn run(db_path: &Path, args: &ExportReflectionsArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    let format = parse_format(&args.format)?;
    let out_dir = resolve_out_dir(args.out_dir.as_deref())?;
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating out-dir {}", out_dir.display()))?;

    let conn = db::open(db_path)?;
    let mut summary = ExportSummary::default();

    let reflections = collect_reflections(&conn, args.namespace.as_deref(), args.since.as_deref())?;
    for mem in &reflections {
        let edges = collect_outbound_reflects_on(&conn, &mem.id)?;
        let attest_level = summarise_attest_level(&edges);
        let payload = render_payload(mem, &edges, attest_level, format);

        let ns_dir = out_dir.join(sanitise_namespace_for_path(&mem.namespace));
        fs::create_dir_all(&ns_dir)
            .with_context(|| format!("creating namespace dir {}", ns_dir.display()))?;
        let filename = format!("{}.{}", mem.id, format.extension());
        let path = ns_dir.join(&filename);
        fs::write(&path, payload).with_context(|| format!("writing {}", path.display()))?;
        summary.written += 1;
        if !args.quiet {
            writeln!(out.stdout, "wrote {}", path.display())?;
        }
    }
    writeln!(
        out.stdout,
        "exported {} reflection(s) to {}",
        summary.written,
        out_dir.display()
    )?;
    let _ = summary.skipped; // reserved for future "skip-existing" mode.
    Ok(0)
}

/// One outbound `reflects_on` edge — the substrate-side projection that
/// drives both the markdown body and the JSON envelope.
#[derive(Debug, Clone)]
pub(crate) struct ReflectsOnEdge {
    pub target_id: String,
    pub attest_level: String,
    pub created_at: String,
}

/// Visible-for-testing: parse `--format` into the enum.
pub(crate) fn parse_format(spec: &str) -> Result<ExportFormat> {
    match spec.to_lowercase().as_str() {
        "md" | "markdown" => Ok(ExportFormat::Markdown),
        "json" => Ok(ExportFormat::Json),
        other => anyhow::bail!("unsupported export format '{other}' (expected 'md' or 'json')"),
    }
}

/// Supported export formats. `pub` because the substrate-side hook
/// at `crate::hooks::post_reflect::auto_export` carries an
/// `AutoExportConfig.format: ExportFormat` field, and Rust requires
/// the type to be at least as public as the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Json,
}

impl ExportFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Json => "json",
        }
    }
}

/// Resolve `--out-dir` (or the canonical default) to an absolute path.
///
/// Default = `${HOME}/.ai-memory/reflections/`. Falls back to
/// `./.ai-memory/reflections/` (relative to CWD) when `HOME` is
/// unavailable — typical in CI containers and the test harness, where
/// writing to a project-local relative path is the only valid choice.
pub(crate) fn resolve_out_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".ai-memory").join("reflections"));
    }
    Ok(PathBuf::from(".ai-memory").join("reflections"))
}

/// Namespace → safe filesystem path component. Slashes are preserved
/// (so `team/alpha` becomes nested subdirs), every other "weird"
/// character is replaced with `_`. The substrate already validates
/// namespace strings on the write path, so the universe of inputs is
/// already constrained — this is defence-in-depth.
pub(crate) fn sanitise_namespace_for_path(ns: &str) -> PathBuf {
    let mut buf = PathBuf::new();
    for component in ns.split('/') {
        let cleaned: String = component
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if !cleaned.is_empty() {
            buf.push(cleaned);
        }
    }
    if buf.as_os_str().is_empty() {
        buf.push("_unnamed");
    }
    buf
}

/// Read every reflection-kind memory matching the supplied filters.
///
/// The query is namespace-scoped on the SQL side (cheap) and
/// memory-kind / since-filtered in Rust (correct over `Memory`'s
/// model-level fields rather than column-level oddities).
fn collect_reflections(
    conn: &rusqlite::Connection,
    namespace: Option<&str>,
    since: Option<&str>,
) -> Result<Vec<Memory>> {
    // The substrate's `db::list` already understands the namespace +
    // since filters; we layer the memory_kind filter on top in Rust so
    // we don't need to thread a new column-filter into the substrate
    // signature for a CLI-side cosmetic export.
    let now = Utc::now().to_rfc3339();
    let _ = now; // future: time-bounded resume
    let rows = db::list(
        conn,
        namespace,
        None,
        i32::MAX as usize,
        0,
        None,
        since,
        None,
        None,
        None,
    )?;
    Ok(rows
        .into_iter()
        .filter(|m| matches!(m.memory_kind, MemoryKind::Reflection))
        .collect())
}

/// Read all `reflects_on` outbound edges (this memory → its sources)
/// with their attestation level.
fn collect_outbound_reflects_on(
    conn: &rusqlite::Connection,
    memory_id: &str,
) -> Result<Vec<ReflectsOnEdge>> {
    let mut stmt = conn.prepare(
        "SELECT target_id, COALESCE(attest_level, 'unsigned'), created_at \
         FROM memory_links \
         WHERE source_id = ?1 AND relation = 'reflects_on' \
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![memory_id], |row| {
        Ok(ReflectsOnEdge {
            target_id: row.get(0)?,
            attest_level: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Summarise the per-edge attestation into one row-level label for
/// the frontmatter. Promotion order: `signed > peer_attested >
/// self_signed > unsigned`. No outbound edges → `"unsigned"`.
pub(crate) fn summarise_attest_level(edges: &[ReflectsOnEdge]) -> &'static str {
    let mut best = 0u8;
    for e in edges {
        let rank: u8 = match e.attest_level.as_str() {
            "signed" => 3,
            "peer_attested" => 2,
            "self_signed" => 1,
            _ => 0,
        };
        if rank > best {
            best = rank;
        }
    }
    match best {
        3 => "signed",
        2 => "peer_attested",
        1 => "self_signed",
        _ => "unsigned",
    }
}

/// Read `metadata.agent_id` off a reflection memory. Returns the empty
/// string when the field is missing — the canonical "unknown" shape
/// for downstream readers (`grep -v "^agent_id: $"` still finds rows).
pub(crate) fn agent_id_of(mem: &Memory) -> &str {
    mem.metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// Render the export payload (md or json) as a UTF-8 `String`.
pub(crate) fn render_payload(
    mem: &Memory,
    edges: &[ReflectsOnEdge],
    attest_level: &str,
    format: ExportFormat,
) -> String {
    match format {
        ExportFormat::Markdown => render_markdown(mem, edges, attest_level),
        ExportFormat::Json => render_json(mem, edges, attest_level),
    }
}

/// Render the YAML-frontmatter markdown body. Frontmatter fields are
/// emitted in a stable order (memory_id, namespace, reflection_depth,
/// attest_level, created_at, agent_id) followed by a sequence of
/// `reflects_on` edges. The body of the reflection follows after a
/// blank line, exactly as it was stored.
fn render_markdown(mem: &Memory, edges: &[ReflectsOnEdge], attest_level: &str) -> String {
    let agent_id = agent_id_of(mem);
    let mut out = String::with_capacity(256 + mem.content.len());
    out.push_str("---\n");
    out.push_str(&format!("memory_id: {}\n", mem.id));
    out.push_str(&format!("namespace: {}\n", yaml_scalar(&mem.namespace)));
    out.push_str(&format!("title: {}\n", yaml_scalar(&mem.title)));
    out.push_str(&format!("reflection_depth: {}\n", mem.reflection_depth));
    out.push_str(&format!("attest_level: {attest_level}\n"));
    out.push_str(&format!("created_at: {}\n", mem.created_at));
    out.push_str(&format!("agent_id: {}\n", yaml_scalar(agent_id)));
    out.push_str("reflects_on:\n");
    if edges.is_empty() {
        out.push_str("  []\n");
    } else {
        for e in edges {
            out.push_str(&format!(
                "  - target_id: {}\n    attest_level: {}\n    created_at: {}\n",
                e.target_id, e.attest_level, e.created_at,
            ));
        }
    }
    out.push_str("---\n\n");
    out.push_str(&mem.content);
    if !mem.content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Render the JSON envelope.
fn render_json(mem: &Memory, edges: &[ReflectsOnEdge], attest_level: &str) -> String {
    let agent_id = agent_id_of(mem);
    let env = JsonEnvelope {
        memory_id: &mem.id,
        namespace: &mem.namespace,
        title: &mem.title,
        reflection_depth: mem.reflection_depth,
        attest_level,
        created_at: &mem.created_at,
        agent_id,
        reflects_on: edges.iter().map(|e| e.target_id.clone()).collect(),
        content: &mem.content,
    };
    // `to_string_pretty` keeps the JSON human-readable when the
    // operator inspects it; `to_string` would land everything on one
    // line, which is hostile to `git diff` and to `cat`.
    serde_json::to_string_pretty(&env).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// Conservatively quote a YAML scalar. We escape only the obviously
/// dangerous shapes (containing `:` `#` `"` `'` or starting with `-`
/// `?` `*` `&`); everything else is emitted bare. The shape is
/// deliberately simple — operators read these files; we never
/// round-trip them back through a YAML parser, so over-engineering
/// the escape is dead weight.
fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.starts_with(['-', '?', '*', '&', '!', '|', '>', '\'', '"', '%', '@', '`'])
        || s.contains(':')
        || s.contains('#')
        || s.contains('\n');
    if needs_quote {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tier;
    use chrono::Utc;
    use tempfile::TempDir;

    fn fresh_db() -> (rusqlite::Connection, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).expect("db::open");
        (conn, dir)
    }

    fn make_reflection(ns: &str, depth: i32, title: &str, body: &str, agent_id: &str) -> Memory {
        let now = Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: body.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": agent_id}),
            reflection_depth: depth,
            memory_kind: MemoryKind::Reflection,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: crate::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        }
    }

    #[test]
    fn parse_format_accepts_md_and_json() {
        assert_eq!(parse_format("md").unwrap(), ExportFormat::Markdown);
        assert_eq!(parse_format("markdown").unwrap(), ExportFormat::Markdown);
        assert_eq!(parse_format("MD").unwrap(), ExportFormat::Markdown);
        assert_eq!(parse_format("json").unwrap(), ExportFormat::Json);
        assert!(parse_format("yaml").is_err());
    }

    #[test]
    fn sanitise_namespace_handles_slashes_and_weird_chars() {
        let p = sanitise_namespace_for_path("team/alpha");
        assert_eq!(p, PathBuf::from("team").join("alpha"));
        let p2 = sanitise_namespace_for_path("evil:ns?with*bits");
        assert_eq!(p2, PathBuf::from("evil_ns_with_bits"));
        let p3 = sanitise_namespace_for_path("");
        assert_eq!(p3, PathBuf::from("_unnamed"));
    }

    #[test]
    fn summarise_attest_level_promotes_to_highest() {
        let mk = |s: &str| ReflectsOnEdge {
            target_id: "x".into(),
            attest_level: s.into(),
            created_at: "2026-01-01".into(),
        };
        assert_eq!(summarise_attest_level(&[]), "unsigned");
        assert_eq!(
            summarise_attest_level(&[mk("unsigned"), mk("unsigned")]),
            "unsigned"
        );
        assert_eq!(
            summarise_attest_level(&[mk("unsigned"), mk("self_signed")]),
            "self_signed"
        );
        assert_eq!(
            summarise_attest_level(&[mk("self_signed"), mk("peer_attested")]),
            "peer_attested"
        );
        assert_eq!(
            summarise_attest_level(&[mk("peer_attested"), mk("signed")]),
            "signed"
        );
    }

    #[test]
    fn render_markdown_carries_frontmatter_and_edges() {
        let mem = make_reflection(
            "team/alpha",
            2,
            "lesson learned",
            "Body line.\n",
            "agent-without-colon",
        );
        let edges = vec![
            ReflectsOnEdge {
                target_id: "src-1".into(),
                attest_level: "unsigned".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
            },
            ReflectsOnEdge {
                target_id: "src-2".into(),
                attest_level: "signed".into(),
                created_at: "2026-01-02T00:00:00Z".into(),
            },
        ];
        let s = render_markdown(&mem, &edges, "signed");
        assert!(s.starts_with("---\n"));
        assert!(s.contains(&format!("memory_id: {}\n", mem.id)));
        assert!(s.contains("namespace: team/alpha\n"));
        assert!(s.contains("reflection_depth: 2\n"));
        assert!(s.contains("attest_level: signed\n"));
        // Bare scalar (no quotes) when value has no YAML-unsafe chars.
        assert!(s.contains("agent_id: agent-without-colon\n"));
        assert!(s.contains("  - target_id: src-1\n"));
        assert!(s.contains("    attest_level: signed\n"));
        assert!(s.ends_with("Body line.\n"));
    }

    #[test]
    fn render_markdown_quotes_agent_id_with_colon() {
        // `ai:test` style ids contain a `:` and must be quoted on the
        // YAML wire so a downstream YAML parser doesn't misread the
        // remainder as a nested mapping value.
        let mem = make_reflection("ns", 1, "t", "body", "ai:bot");
        let s = render_markdown(&mem, &[], "unsigned");
        assert!(s.contains("agent_id: \"ai:bot\"\n"));
    }

    #[test]
    fn render_markdown_quotes_yaml_unsafe_strings() {
        let mut mem = make_reflection("global", 1, "weird: title", "body", "");
        mem.namespace = "weird:ns".into();
        let s = render_markdown(&mem, &[], "unsigned");
        // Title carries a colon — must be quoted.
        assert!(s.contains("title: \"weird: title\"\n"));
        // Namespace carries a colon — quoted on the frontmatter row,
        // even though `sanitise_namespace_for_path` would replace it
        // on disk.
        assert!(s.contains("namespace: \"weird:ns\"\n"));
        // Empty agent_id quoted as "" (so grep finds the row).
        assert!(s.contains("agent_id: \"\"\n"));
        // No edges → bracket form.
        assert!(s.contains("reflects_on:\n  []\n"));
    }

    #[test]
    fn render_json_emits_pretty_envelope() {
        let mem = make_reflection("ns", 1, "t", "body content\n", "ai:bot");
        let edges = vec![ReflectsOnEdge {
            target_id: "src".into(),
            attest_level: "self_signed".into(),
            created_at: "2026-01-01".into(),
        }];
        let s = render_json(&mem, &edges, "self_signed");
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["memory_id"].as_str().unwrap(), mem.id);
        assert_eq!(parsed["namespace"].as_str().unwrap(), "ns");
        assert_eq!(parsed["reflection_depth"].as_i64().unwrap(), 1);
        assert_eq!(parsed["attest_level"].as_str().unwrap(), "self_signed");
        assert_eq!(parsed["agent_id"].as_str().unwrap(), "ai:bot");
        assert_eq!(parsed["reflects_on"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["reflects_on"][0].as_str().unwrap(), "src");
        assert!(parsed["content"].as_str().unwrap().contains("body content"));
    }

    #[test]
    fn resolve_out_dir_explicit_overrides_default() {
        let p = resolve_out_dir(Some(Path::new("/tmp/some-path"))).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/some-path"));
    }

    #[test]
    fn collect_reflections_filters_observations() {
        let (conn, _g) = fresh_db();
        // Reflection
        let r = make_reflection("ns-r", 1, "rfl", "rfl body", "ai:a");
        db::insert(&conn, &r).unwrap();
        // Observation (same namespace)
        let mut obs = make_reflection("ns-r", 0, "obs", "obs body", "ai:a");
        obs.memory_kind = MemoryKind::Observation;
        obs.reflection_depth = 0;
        db::insert(&conn, &obs).unwrap();

        let collected = collect_reflections(&conn, Some("ns-r"), None).unwrap();
        assert_eq!(collected.len(), 1);
        assert!(matches!(collected[0].memory_kind, MemoryKind::Reflection));
    }

    #[test]
    fn agent_id_of_returns_empty_when_absent() {
        let mut mem = make_reflection("n", 1, "t", "c", "");
        mem.metadata = serde_json::json!({});
        assert_eq!(agent_id_of(&mem), "");
    }
}
