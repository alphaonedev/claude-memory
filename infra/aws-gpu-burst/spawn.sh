#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# =============================================================================
# Track E2 — AWS GPU burst hive spawn (issue #834). MONEY-GATED.
# =============================================================================
#
# Refuses `terraform apply` unless the operator explicitly approves spend
# via env var `AI_MEMORY_OPERATOR_AWS_SPEND_APPROVED=1`. AI NHI agents
# MUST NOT set this var.
#
# $200 budget cap per #834 mandate. The cost banner below is the
# operator's at-a-glance reference.
# =============================================================================

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
SCRATCH_ROOT="${REPO_ROOT}/.local-runs/aws-burst-runs"
NOW="$(date -u +%Y-%m-%dT%H-%M-%SZ)"

cd "${HERE}"

print_cost() {
  cat <<'COST'
========================================================================
  Track E2 — AWS GPU burst cost estimate (issue #834)
------------------------------------------------------------------------
                              $/hr      $/24h    $/48h    $/72h
  --------------------------  --------  -------  -------  -------
  base (1 vLLM + 5 agents)    ~0.96     ~23      ~46      ~69
  10-agent variant            ~1.17     ~28      ~56      ~84

  $200 budget cap → $130 reserve for re-runs / spot interruption.

  Spot pricing is volatile; this script uses spot_price = $0.75 for
  the g5.2xlarge to keep fulfilment >95% in us-east-1a. Adjust in
  main.tf if the live quote is colder.
========================================================================
COST
}

require_money_gate() {
  if [[ "${AI_MEMORY_OPERATOR_AWS_SPEND_APPROVED:-0}" != "1" ]]; then
    cat >&2 <<'GATE'
[spawn.sh] REFUSE: AWS spend not approved.

    export AI_MEMORY_OPERATOR_AWS_SPEND_APPROVED=1

AI NHI agents are forbidden from setting this var. Operator only.
GATE
    exit 2
  fi
  if [[ -z "${AWS_ACCESS_KEY_ID:-}" || -z "${AWS_SECRET_ACCESS_KEY:-}" ]]; then
    echo "[spawn.sh] REFUSE: AWS credentials must be sourced from operator vault." >&2
    exit 2
  fi
  for v in TF_VAR_ssh_key_name TF_VAR_ssh_source_cidr; do
    if [[ -z "${!v:-}" ]]; then
      echo "[spawn.sh] REFUSE: ${v} must be set." >&2
      exit 2
    fi
  done
}

cmd="${1:-plan}"
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
    echo "[spawn.sh] Remember: g5.2xlarge spot incurs charges continuously."
    echo "[spawn.sh] Run teardown.sh as soon as the capture window closes."
    ;;
  outputs)
    terraform output
    ;;
  *)
    echo "usage: spawn.sh {plan|apply|outputs|cost}" >&2
    exit 1
    ;;
esac
