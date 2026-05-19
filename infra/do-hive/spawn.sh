#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# =============================================================================
# Track E1 — Digital Ocean CPU agent hive spawn wrapper (issue #833).
# =============================================================================
#
# MONEY-GATED. Refuses to call `terraform apply` unless the operator has
# explicitly approved spend by setting:
#
#     AI_MEMORY_OPERATOR_DO_SPEND_APPROVED=1
#
# AI NHI agents MUST NOT set this env var. Only the human operator does.
#
# Modes:
#   - `infra/do-hive/spawn.sh plan`     → `terraform plan` (no spend; safe)
#   - `infra/do-hive/spawn.sh apply`    → `terraform apply` (MONEY; gated)
#   - `infra/do-hive/spawn.sh outputs`  → re-emit outputs from last state
#   - `infra/do-hive/spawn.sh cost`     → render the cost-estimate header
#
# Audit: every `apply` writes a stamped manifest under
# `.local-runs/do-hive-runs/<UTC-ISO>/` with droplet IPs, IDs, and the
# resolved `terraform.tfstate` so a post-mortem can reconstruct what
# code each agent saw.
# =============================================================================

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
SCRATCH_ROOT="${REPO_ROOT}/.local-runs/do-hive-runs"
NOW="$(date -u +%Y-%m-%dT%H-%M-%SZ)"

cd "${HERE}"

cmd="${1:-plan}"

# --- Cost banner ------------------------------------------------------------

print_cost() {
  cat <<'COST'
========================================================================
  Track E1 — DO CPU hive cost estimate (issue #833)
------------------------------------------------------------------------
  N agents   $/hr     $/24h    $/month
  ---------  -------  -------  --------
       4     0.072    1.73       51
      10     0.144    3.46      104   ← reference hive
      25     0.324    7.78      234
      50     0.624    14.98     449

  +inference (xAI Grok 4.3 via API): per-token, billed to operator's
  xAI account. Not provisioned by this Terraform.

  Smoke-test target: ~$2 budget for a 1h N=4 capture run.
========================================================================
COST
}

# --- Money-gate -------------------------------------------------------------

require_money_gate() {
  if [[ "${AI_MEMORY_OPERATOR_DO_SPEND_APPROVED:-0}" != "1" ]]; then
    cat >&2 <<'GATE'
[spawn.sh] REFUSE: spend not approved.

This script spawns paid Digital Ocean droplets. The operator must
explicitly approve spend by setting:

    export AI_MEMORY_OPERATOR_DO_SPEND_APPROVED=1

AI NHI agents are forbidden from setting this var. Operator only.
GATE
    exit 2
  fi
  if [[ -z "${DIGITALOCEAN_TOKEN:-}" ]]; then
    echo "[spawn.sh] REFUSE: DIGITALOCEAN_TOKEN must be set (sourced from operator vault)." >&2
    exit 2
  fi
  if [[ -z "${TF_VAR_ssh_pubkey_fingerprint:-}" ]]; then
    echo "[spawn.sh] REFUSE: TF_VAR_ssh_pubkey_fingerprint must be set." >&2
    exit 2
  fi
}

case "${cmd}" in
  cost)
    print_cost
    ;;
  plan)
    print_cost
    terraform init -input=false
    terraform plan -input=false
    ;;
  apply)
    print_cost
    require_money_gate
    terraform init -input=false
    mkdir -p "${SCRATCH_ROOT}/${NOW}"
    terraform apply -input=false -auto-approve
    terraform output -json > "${SCRATCH_ROOT}/${NOW}/outputs.json"
    cp terraform.tfstate "${SCRATCH_ROOT}/${NOW}/terraform.tfstate"
    echo "[spawn.sh] Apply complete. Audit dump: ${SCRATCH_ROOT}/${NOW}/"
    ;;
  outputs)
    terraform output
    ;;
  *)
    echo "usage: spawn.sh {plan|apply|outputs|cost}" >&2
    exit 1
    ;;
esac
