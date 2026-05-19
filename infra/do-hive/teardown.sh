#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# Track E1 — DO CPU agent hive teardown (issue #833).
#
# Idempotent: safe to re-run. `terraform destroy` walks the state file
# and removes every droplet + the VPC + firewall the spawn provisioned.
# Cost stops accruing the moment the droplets disappear from the DO
# fleet (per-second billing).
#
# Audit: writes the destroyed manifest under
# `.local-runs/do-hive-runs/<UTC-ISO>/teardown.json` so the operator
# can prove zero remaining droplets at hive shutdown.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
SCRATCH_ROOT="${REPO_ROOT}/.local-runs/do-hive-runs"
NOW="$(date -u +%Y-%m-%dT%H-%M-%SZ)"

cd "${HERE}"

if [[ -z "${DIGITALOCEAN_TOKEN:-}" ]]; then
  echo "[teardown.sh] REFUSE: DIGITALOCEAN_TOKEN must be set." >&2
  exit 2
fi

mkdir -p "${SCRATCH_ROOT}/${NOW}"
terraform init -input=false >/dev/null
terraform output -json > "${SCRATCH_ROOT}/${NOW}/pre-destroy.json" || true
terraform destroy -input=false -auto-approve
terraform output -json > "${SCRATCH_ROOT}/${NOW}/teardown.json" 2>/dev/null || echo '{}' > "${SCRATCH_ROOT}/${NOW}/teardown.json"

echo "[teardown.sh] Destroy complete. Verify with:"
echo "    doctl compute droplet list --tag-name ai-memory-hive"
echo "(expected: empty list)"
