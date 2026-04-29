<!--
Thanks for contributing to ai-memory!

Required reading for contributors:
- CONTRIBUTING.md
- docs/ENGINEERING_STANDARDS.md  (authoritative)

Required reading for AI agents (and the humans driving them):
- docs/AI_DEVELOPER_WORKFLOW.md
- docs/AI_DEVELOPER_GOVERNANCE.md

PRs target `develop`. Never target `main`.
-->

## Summary

<!-- 1–3 bullets: what changed and why. -->

-
-

## AI involvement

<!--
Required when an AI coding agent (Claude Code, Cursor, Copilot, Codex, Grok CLI,
Gemini CLI, Continue.dev, Windsurf, OpenClaw, etc.) authored any part of this PR.

If no AI agent was involved, write "None — human-authored" and delete the
remaining fields in this section.

See docs/AI_DEVELOPER_GOVERNANCE.md for authority classes and attribution rules.
-->

- **Agent:** <!-- e.g. Claude Opus 4.6 / Codex / Gemini 2.5 / "None — human-authored" -->
- **Authority class:** <!-- Trivial | Standard | Sensitive (Restricted is human-only) -->
- **Human approver(s) for any Sensitive items:** <!-- @handle, or "n/a" -->
- **ai-memory entries created/updated:** <!-- ids or "none" -->
- **Co-Authored-By trailer present on every AI-authored commit:** <!-- yes / no -->

## Linked issues

<!-- Use "Closes #123" to auto-close on merge, or "Refs #123" for related work. -->

Closes #

## Test plan

- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic` clean
- [ ] `AI_MEMORY_NO_CONFIG=1 cargo test` all passing
- [ ] `cargo audit` clean (or warnings explained)
- [ ] Manual security checklist reviewed (Engineering Standards §3.2) — applicable to source changes
- [ ] Documentation sync where applicable (test counts, MCP tool counts — see Engineering Standards §2.6)
- [ ] CLA on file for the accountable contributor

## Notes for reviewers

<!-- Anything reviewers should know: tradeoffs, follow-ups, things intentionally
left out of scope. -->
