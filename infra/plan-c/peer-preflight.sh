#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# Issue #878 — peer-mesh reach preflight.
#
# Probes every peer URL declared in `$PEER_URLS` (comma-separated) and
# aborts with EX_CONFIG (78) if any are unreachable. Intended to be
# sourced or invoked by `entrypoint.plan-c.sh` before exec'ing the
# daemon, so a host-port-collision misconfiguration surfaces with a
# clear error at boot instead of silently degrading quorum writes
# minutes later.
#
# Inputs (env):
#   PEER_URLS                       comma-separated list, may be empty
#   AI_MEMORY_SKIP_PEER_PREFLIGHT   when "1" the probe is a no-op
#   PEER_PREFLIGHT_TIMEOUT_S        per-peer curl timeout, default 5
#
# Exit codes:
#   0   — all reachable, or PEER_URLS empty, or bypass requested
#   78  — at least one peer unreachable (sysexits.h EX_CONFIG)
#
# The probe is a GET against `<peer>/api/v1/capabilities` — the same
# endpoint the docker-compose healthcheck uses. We accept ANY HTTP
# response (including 401 for api-key-gated peers and 404/405 for
# differently-routed peers) as "reachable"; only connect-refused /
# host-unreachable / timeout count as failures (curl `%{http_code}`
# returns `000` in those cases).

set -u

peer_preflight() {
  local peers="${PEER_URLS:-}"
  local skip="${AI_MEMORY_SKIP_PEER_PREFLIGHT:-0}"
  local timeout="${PEER_PREFLIGHT_TIMEOUT_S:-5}"

  if [ -z "$peers" ]; then
    return 0
  fi
  if [ "$skip" = "1" ]; then
    echo "[#878 preflight] skipped (AI_MEMORY_SKIP_PEER_PREFLIGHT=1)"
    return 0
  fi

  echo "[#878 preflight] probing peer-mesh URLs..."
  local IFS=','
  read -ra PEER_LIST <<<"$peers"

  local unreachable=()
  local peer probe_url http_code
  for peer in "${PEER_LIST[@]}"; do
    # Trim leading/trailing whitespace.
    peer="${peer#"${peer%%[![:space:]]*}"}"
    peer="${peer%"${peer##*[![:space:]]}"}"
    [ -z "$peer" ] && continue
    probe_url="${peer%/}/api/v1/capabilities"
    # curl with `-w '%{http_code}'` prints `000` to stdout on connect
    # failure / timeout (and ALSO returns non-zero exit). We capture
    # stdout and discard curl's exit code — `000` is the canonical
    # unreachable sentinel both ways.
    http_code=$(curl -o /dev/null -s -w '%{http_code}' \
      --max-time "$timeout" -X GET "$probe_url" 2>/dev/null)
    # Fallback: if curl was OOM-killed or absent, http_code is empty.
    [ -z "$http_code" ] && http_code="000"
    if [ "$http_code" = "000" ]; then
      echo "[#878 preflight] UNREACHABLE peer=$peer (probe_url=$probe_url)" >&2
      unreachable+=("$peer")
    else
      echo "[#878 preflight] ok peer=$peer (http=$http_code)"
    fi
  done

  if [ "${#unreachable[@]}" -gt 0 ]; then
    cat >&2 <<EOM
[#878 preflight] FAILED — ${#unreachable[@]} peer(s) unreachable.

Likely causes:
  - Another host process owns the published port (SSH forward, python
    -m http.server, stale docker proxy after \`colima delete\`, etc.)
  - The peer container isn't started yet (use \`depends_on\` in the
    compose file, or set AI_MEMORY_SKIP_PEER_PREFLIGHT=1 to bypass)
  - The peer URL uses host.docker.internal but the host's docker proxy
    isn't routing port-N to the container that owns the daemon

Recommended long-term fix: move to a user-defined bridge network with
container-DNS peer URLs (e.g. \`http://ic-bob:19077\` instead of
\`http://host.docker.internal:9078\`). See infra/plan-c/docker-compose.yml
for the canonical recipe. Issue tracker: #878.

Unreachable peers:
EOM
    local u
    for u in "${unreachable[@]}"; do
      echo "  - $u" >&2
    done
    return 78
  fi

  echo "[#878 preflight] all ${#PEER_LIST[@]} peer(s) reachable."
  return 0
}

# Allow this file to be both sourced (function is defined for the
# caller) and invoked directly (function runs). The `${BASH_SOURCE[0]}`
# == `$0` check is the canonical "am I being executed?" idiom.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  peer_preflight
  exit $?
fi
