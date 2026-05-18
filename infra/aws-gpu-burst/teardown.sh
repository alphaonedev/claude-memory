#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# Track E2 — AWS GPU burst hive teardown (issue #834). Idempotent.
#
# `terraform destroy` removes spot requests (cancels them) plus the t3
# instances + EBS + VPC + security group + IGW + subnet + route table.
# Spot fleets bill per-second; the moment the request is cancelled and
# instances stop, charges stop.
#
# Critical safety note: a hanging `aws_spot_instance_request` left
# fulfilled but unreferenced will continue to bill. teardown.sh writes
# the post-destroy state under .local-runs/ so the operator can
# cross-check `aws ec2 describe-spot-instance-requests --region us-east-1`
# to confirm zero remaining active requests.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
SCRATCH_ROOT="${REPO_ROOT}/.local-runs/aws-burst-runs"
NOW="$(date -u +%Y-%m-%dT%H-%M-%SZ)"

cd "${HERE}"

if [[ -z "${AWS_ACCESS_KEY_ID:-}" || -z "${AWS_SECRET_ACCESS_KEY:-}" ]]; then
  echo "[teardown.sh] REFUSE: AWS credentials must be sourced from operator vault." >&2
  exit 2
fi

mkdir -p "${SCRATCH_ROOT}/${NOW}"
terraform init -input=false >/dev/null
terraform output -json > "${SCRATCH_ROOT}/${NOW}/pre-destroy.json" || true
terraform destroy -input=false -auto-approve
echo '{}' > "${SCRATCH_ROOT}/${NOW}/teardown.json"

cat <<'POST'
[teardown.sh] Destroy complete. Cross-check the AWS-side state with:

    aws ec2 describe-spot-instance-requests \
      --region us-east-1 \
      --filters Name=tag:Project,Values=ai-memory-track-e2 \
                Name=state,Values=active,open

(expected: empty result. Any non-empty result is a billing leak —
investigate with `terraform state list` and `aws ec2 cancel-spot-instance-requests`.)
POST
