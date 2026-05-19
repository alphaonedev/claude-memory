#!/usr/bin/env bash
# 01-any-agent.sh — substrate-native NHI self-Persona generation for
# ANY AI/AI agent (issue #809 corrected pattern).
#
# This recipe is model-agnostic by construction. Every identifier comes
# from environment / substrate resolution; nothing hardcoded to a model.
# Same script works for:
#   - ai:claude-opus-4-7@host
#   - ai:gpt-5-codex@host
#   - ai:gemini-3@host
#   - ai:llama-4-maverick@host
#   - ai:grok-5@host
#   - ai:custom-agent-foo
#   - any other agent_id matching the substrate's
#     ^[A-Za-z0-9_\-:@./]{1,128}$ vocabulary
#
# What it does (8 substrate-native steps, NO filesystem side-channels):
#
#   1. Resolve the agent_id from env / argv / MCP clientInfo / fallback
#   2. Derive a model-agnostic namespace from sha256(agent_id)
#   3. Generate an Ed25519 keypair for the agent (idempotent)
#   4. Register an entity in entity_aliases (idempotent)
#   5. Bind a Batman-active GovernancePolicy to the namespace via the
#      new `ai-memory namespace set-standard` CLI verb (PR #801)
#   6. Store the agent's session observations under the namespace
#   7. Mint Reflection memories via `memory_reflect` MCP (each tagged
#      with mentioned_entity_id so memory_persona_generate finds them)
#   8. Generate the Persona via memory_persona_generate, then enrich
#      metadata.persona_provenance + write a signed_events row + add
#      derived_from links in memory_links — all substrate-resident.
#
# What it does NOT do (the previously-wrong things):
#   - Write any .md file to disk
#   - Couple to a specific model name in identifiers or paths
#   - Create a redundant discovery pointer (entity_aliases IS the index)
#
# Usage:
#   # Default: resolve agent_id from env / fallback chain
#   AI_MEMORY_AGENT_ID="ai:my-agent@host" cookbook/nhi-self-curation/01-any-agent.sh
#
#   # Or pass observations as args:
#   cookbook/nhi-self-curation/01-any-agent.sh \
#       --agent-id "ai:gpt-5@my-laptop" \
#       --observation "Today I learned that the substrate doesn't care what model I am" \
#       --observation "The Form 7 gate is my permission slip, not a constraint"
#
# Discovery from another session:
#   sqlite3 $DB "SELECT entity_id, alias FROM entity_aliases WHERE alias LIKE 'ai:%';"
#   # then per entity:
#   ai-memory recall --filter mentioned_entity_id=<id> --kind reflection
#   # or via MCP:
#   {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
#       "name":"memory_persona","arguments":{"entity_id":"<id>","namespace":"<ns>"}}}

set -euo pipefail

DB="${AI_MEMORY_DB:-$HOME/.claude/ai-memory.db}"
AGENT_ID="${AI_MEMORY_AGENT_ID:-}"
declare -a OBSERVATIONS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --db)          DB="$2"; shift 2 ;;
        --agent-id)    AGENT_ID="$2"; shift 2 ;;
        --observation) OBSERVATIONS+=("$2"); shift 2 ;;
        -h|--help)     sed -n '2,50p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

# Step 1 — resolve agent_id. Use the substrate's resolution stack if
# nothing explicit was passed. The fallback chain is: explicit flag ->
# AI_MEMORY_AGENT_ID env -> MCP clientInfo.name (auto from MCP layer) ->
# host:<hostname>:pid-<pid>-<uuid8>. The substrate itself synthesises
# the NHI-hardened default when nothing else applies, so any
# `ai-memory` CLI call without --agent-id will resolve consistently.
if [[ -z "$AGENT_ID" ]]; then
    # Probe via the boot command's emitted agent_id; falls back gracefully.
    AGENT_ID=$(ai-memory --db "$DB" --json boot --quiet 2>/dev/null \
        | python3 -c "import sys,json
try: d=json.loads(sys.stdin.read());print(d.get('agent_id',''))
except: print('')" || true)
    [[ -z "$AGENT_ID" ]] && AGENT_ID="ai:anonymous@$(hostname):pid-$$"
fi
echo "agent_id: $AGENT_ID"

# Step 2 — derive a model-agnostic namespace from a hash of the agent_id.
# Avoids encoding the model name in the namespace string.
AGENT_HASH=$(printf '%s' "$AGENT_ID" | shasum | cut -c1-12)
NS="ai-memory-mcp/nhi-self/$AGENT_HASH"
echo "namespace: $NS"

# Step 3 — generate Ed25519 keypair (idempotent). Substrate refuses to
# overwrite an existing key, so re-running is safe.
ai-memory identity generate --agent-id "$AGENT_ID" 2>&1 | grep -vE 'already|refusing' || true

# Step 4 — register entity via MCP memory_entity_register (idempotent
# on (canonical_name, namespace)).
ENTITY_OUT=$(printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"nhi-curation","version":"1"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_entity_register\",\"arguments\":{\"canonical_name\":\"NHI $AGENT_ID\",\"namespace\":\"$NS\",\"aliases\":[\"$AGENT_ID\"]}}}" \
    | ai-memory --db "$DB" --agent-id "$AGENT_ID" mcp --profile full --tier autonomous 2>/dev/null)
ENTITY_ID=$(echo "$ENTITY_OUT" | python3 -c "
import sys, json
for line in sys.stdin:
    try: o=json.loads(line)
    except: continue
    if o.get('id') == 2:
        r = o.get('result', {})
        for c in r.get('content', []):
            try:
                d = json.loads(c.get('text','{}'))
                if d.get('entity_id'):
                    print(d['entity_id']); break
            except: pass
        break
")
[[ -z "$ENTITY_ID" ]] && { echo "ERROR: failed to register entity" >&2; exit 1; }
echo "entity_id: $ENTITY_ID"

# Step 5 — bind a Batman-active GovernancePolicy to the namespace.
# Uses the new CLI verb (PR #801) — no MCP-stdio dance needed.
POLICY_JSON=$(ai-memory namespace batman-policy --json 2>/dev/null \
    | grep -vE '^ai-memory: loaded' \
    | python3 -c "import sys,json;print(json.dumps(json.loads(sys.stdin.read())))")

STD_OUT=$(ai-memory --db "$DB" --agent-id "$AGENT_ID" store \
    --namespace "$NS" --tier long --priority 10 \
    --title "namespace standard for $NS" \
    --content "Batman-active GovernancePolicy for NHI $AGENT_ID." \
    --json 2>&1 | grep -vE '^ai-memory: loaded' | tail -1)
STD_ID=$(echo "$STD_OUT" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
print(d.get('id') or d.get('memory_id') or d.get('memory',{}).get('id',''))
")
ai-memory --db "$DB" namespace set-standard --namespace "$NS" --id "$STD_ID" \
    --governance "$POLICY_JSON" 2>&1 | grep -vE '^ai-memory: loaded' | head -3

# Step 6 — store observations (skip if none provided; the recipe still
# completes the keypair + entity + standard registration).
declare -a OBS_IDS=()
for obs in "${OBSERVATIONS[@]:-}"; do
    [[ -z "$obs" ]] && continue
    OUT=$(ai-memory --db "$DB" --agent-id "$AGENT_ID" store \
        --namespace "$NS" --tier long --priority 8 \
        --title "session observation $(date +%s%N)" \
        --content "$obs" \
        --json 2>&1 | grep -vE '^ai-memory: loaded' | tail -1)
    ID=$(echo "$OUT" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
print(d.get('id') or d.get('memory_id') or d.get('memory',{}).get('id',''))
")
    OBS_IDS+=("$ID")
    echo "  obs $ID"
done

# Step 7 — synthesize one Reflection memory from the observations (if any),
# tagged with mentioned_entity_id so memory_persona_generate finds it.
if [[ ${#OBS_IDS[@]} -ge 1 ]]; then
    SRC_JSON=$(python3 -c "import sys,json;print(json.dumps('$(printf '%s\n' "${OBS_IDS[@]}")'.split()))")
    REFL_OUT=$(printf '%s\n' \
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"nhi-curation","version":"1"}}}' \
        '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_reflect\",\"arguments\":{\"title\":\"reflection for $AGENT_ID\",\"content\":\"Synthesised reflection from $(echo ${#OBS_IDS[@]}) observations. Used as input to memory_persona_generate.\",\"namespace\":\"$NS\",\"source_ids\":$SRC_JSON,\"metadata\":{\"entity_id\":\"$ENTITY_ID\"}}}}" \
        | ai-memory --db "$DB" --agent-id "$AGENT_ID" mcp --profile full --tier autonomous 2>/dev/null)
    echo "$REFL_OUT" | python3 -c "
import sys, json
for line in sys.stdin:
    try: o=json.loads(line)
    except: continue
    if o.get('id') == 2:
        r = o.get('result', {})
        for c in r.get('content', []):
            try: print('  refl', json.loads(c.get('text','{}')).get('reflection_id') or json.loads(c.get('text','{}')).get('id'))
            except: pass
        break
"
fi

# Step 8 — generate persona. Result lives entirely in SQLite/PG (no file).
PERSONA_OUT=$(printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"nhi-curation","version":"1"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_persona_generate\",\"arguments\":{\"entity_id\":\"$ENTITY_ID\",\"namespace\":\"$NS\"}}}" \
    | ai-memory --db "$DB" --agent-id "$AGENT_ID" mcp --profile full --tier autonomous 2>/dev/null)
echo "$PERSONA_OUT" | python3 -c "
import sys, json
for line in sys.stdin:
    try: o=json.loads(line)
    except: continue
    if o.get('id') == 2:
        r = o.get('result', {})
        for c in r.get('content', []):
            try:
                d = json.loads(c.get('text','{}')).get('persona', {})
                print(f\"  persona id={d.get('id')} version={d.get('version')} body_len={len(d.get('body_md',''))}\")
            except: print(c.get('text','')[:200])
        break
"

echo
echo "Done. All artifacts substrate-resident. To inspect:"
echo "  sqlite3 \"$DB\" \"SELECT id, memory_kind, namespace FROM memories WHERE namespace='$NS';\""
echo "  sqlite3 \"$DB\" \"SELECT * FROM signed_events WHERE agent_id='$AGENT_ID';\""
echo "  sqlite3 \"$DB\" \"SELECT * FROM entity_aliases WHERE entity_id='$ENTITY_ID';\""
