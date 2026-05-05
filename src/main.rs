// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]

// W6 reduced `main.rs` to a thin shim: every CLI subcommand and the HTTP
// daemon body now live in `ai_memory::daemon_runtime`. The bin keeps its
// `#[tokio::main]` entry point + the bootstrap calls (color init, config
// load, env-var seeding, clap parse) and immediately delegates. Coverage
// for serve()/dispatch is now attributed to the lib crate.
use ai_memory::daemon_runtime::Cli;
use ai_memory::{audit, color, config, daemon_runtime, logging};
use anyhow::Result;
use clap::Parser;

#[cfg(test)]
use ai_memory::cli::helpers::{human_age, id_short};
#[cfg(test)]
use ai_memory::tls;

#[tokio::main]
async fn main() -> Result<()> {
    color::init();
    let app_config = config::AppConfig::load();
    config::AppConfig::write_default_if_missing();
    daemon_runtime::apply_anonymize_default(&app_config);

    // v0.7.0 K3 — pin the process-wide governance gate posture before
    // any subcommand has a chance to call `db::enforce_governance`.
    // Idempotent (`OnceLock::set`); first writer wins.
    config::set_active_permissions_mode(app_config.effective_permissions_mode());

    // PR-5 (issue #487): bootstrap operational logging + security
    // audit trail. Both are default-OFF; init returns silently when
    // disabled. The `_log_guard` MUST stay in scope for the lifetime
    // of the process — when dropped it flushes the non-blocking
    // tracing writer to disk.
    let _log_guard =
        logging::init_file_logging(&app_config.effective_logging()).unwrap_or_else(|e| {
            eprintln!("ai-memory: file logging init failed (continuing without): {e}");
            None
        });
    if let Err(e) = audit::init_from_config(&app_config.effective_audit()) {
        eprintln!("ai-memory: audit init failed (continuing without): {e}");
    }

    let cli = Cli::parse();
    daemon_runtime::run(cli, &app_config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_short_truncates() {
        assert_eq!(id_short("abcdefghijklmnop"), "abcdefgh");
    }

    #[test]
    fn id_short_short_input() {
        assert_eq!(id_short("abc"), "abc");
    }

    #[test]
    fn id_short_empty() {
        assert_eq!(id_short(""), "");
    }

    #[test]
    fn human_age_just_now() {
        let now = chrono::Utc::now().to_rfc3339();
        assert_eq!(human_age(&now), "just now");
    }

    #[test]
    fn human_age_minutes() {
        let past = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let age = human_age(&past);
        assert!(age.contains("m ago"), "got: {age}");
    }

    #[test]
    fn human_age_hours() {
        let past = (chrono::Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
        let age = human_age(&past);
        assert!(age.contains("h ago"), "got: {age}");
    }

    #[test]
    fn human_age_days() {
        let past = (chrono::Utc::now() - chrono::Duration::days(5)).to_rfc3339();
        let age = human_age(&past);
        assert!(age.contains("d ago"), "got: {age}");
    }

    #[test]
    fn human_age_invalid_returns_input() {
        assert_eq!(human_age("not-a-date"), "not-a-date");
    }

    #[test]
    fn auto_namespace_returns_nonempty() {
        let ns = ai_memory::cli::helpers::auto_namespace();
        assert!(!ns.is_empty());
    }

    // Issue #358: parser must accept inline trailing comments after a
    // fingerprint, in addition to the existing full-line `#` comment skip.
    #[tokio::test]
    async fn fingerprint_allowlist_tolerates_trailing_comments() {
        let fp_a = "a".repeat(64);
        let fp_b = "b".repeat(64);
        let fp_c = format!("{}:{}", "c".repeat(32), "c".repeat(32));
        let body = format!(
            "# authorised mTLS peers\n\
             {fp_a}  # node-1\n\
             \n\
             sha256:{fp_b}\t# node-2 with tab\n\
             {fp_c}\n"
        );
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        let set = tls::load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert_eq!(set.len(), 3, "expected 3 fingerprints, got {}", set.len());
        assert!(set.contains(&[0xaa; 32]));
        assert!(set.contains(&[0xbb; 32]));
        assert!(set.contains(&[0xcc; 32]));
    }

    #[tokio::test]
    async fn fingerprint_allowlist_rejects_embedded_whitespace() {
        // Ultrareview #338 strictness preserved — whitespace before the
        // `#` is fine (gets trimmed), but whitespace inside the hex run
        // still errors so soft-wrap copy-paste artefacts are caught.
        let body = format!("{} {}\n", "a".repeat(32), "a".repeat(32));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        let err = tls::load_fingerprint_allowlist(tmp.path())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unexpected character"),
            "expected strict char-set error, got: {err}"
        );
    }
}
