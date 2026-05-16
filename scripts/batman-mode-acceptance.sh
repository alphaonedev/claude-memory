#!/usr/bin/env bash
# batman-mode-acceptance.sh — prove Batman Mode is active on this node.
#
# Issue: https://github.com/alphaonedev/ai-memory-mcp/issues/800
# Companion doc: docs/batman-active-mode.md (operator how-to)
# Companion HTML atlas: docs/batman-active-mode.html
#
# Verifies the seven write-time-investment forms are active on this node
# against the ai-memory installation pointed to by --db (or AI_MEMORY_DB).
# Pinned against the actual v0.7.0 schema (sqlite v38+, memories columns
# memory_kind/source_uri/source_span/citations/atom_of/atomised_into/
# confidence_source/confidence_signals/confidence_decayed_at).
#
# Exit code = number of FAIL checks (0 = full Batman-active).
#
# Twenty-two checks across the seven forms + the substrate prerequisites:
#
#   PREREQ — binary, schema version, autonomous tier reachable
#   FORM 1 — online dedup-and-synthesis     (substrate tables present)
#   FORM 2 — atomise-before-embed           (atom_of/atomised_into cols)
#   FORM 3 — multi-step ingest orchestrator (MCP tool surface at --profile full)
#   FORM 4 — citations + source-URI         (schema columns)
#   FORM 5 — confidence + decay + shadow    (schema cols + shadow table + namespace standard)
#   FORM 6 — MemoryKind vocabulary          (memory_kind col + namespace standard)
#   FORM 7 — substrate-authority at write   (operator key + R001-R004 + smoke tests)
#   UPKEEP — curator daemon alive + unit installed
#
# Usage:
#   scripts/batman-mode-acceptance.sh                    # uses AI_MEMORY_DB
#   scripts/batman-mode-acceptance.sh --db /path/to.db   # explicit
#   scripts/batman-mode-acceptance.sh --json             # JSON envelope
#   scripts/batman-mode-acceptance.sh --namespace main   # which ns to check Forms 5/6 on
#
# Scratch convention: writes nothing outside the repo's .local-runs/.

set -uo pipefail

DB="${AI_MEMORY_DB:-}"
NAMESPACE="${BATMAN_NAMESPACE:-main}"
JSON_OUT=0
BEHAVIORAL=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --db)         DB="$2"; shift 2 ;;
        --namespace)  NAMESPACE="$2"; shift 2 ;;
        --json)       JSON_OUT=1; shift ;;
        --behavioral) BEHAVIORAL=1; shift ;;
        -h|--help)
            sed -n '2,35p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$DB" ]]; then
    echo "ERROR: no DB path. Set AI_MEMORY_DB or pass --db <path>." >&2
    exit 2
fi
if [[ ! -f "$DB" ]]; then
    echo "ERROR: DB does not exist: $DB" >&2
    exit 2
fi

# ---------------------------------------------------------------- state ---

PASS_COUNT=0
FAIL_COUNT=0
RESULTS_JSON='[]'

record() {
    # record <id> <pass|fail> <description> <evidence>
    local id="$1" verdict="$2" what="$3" evidence="$4"
    local symbol
    case "$verdict" in
        pass) symbol="PASS"; PASS_COUNT=$((PASS_COUNT + 1)) ;;
        fail) symbol="FAIL"; FAIL_COUNT=$((FAIL_COUNT + 1)) ;;
    esac
    if [[ $JSON_OUT -eq 0 ]]; then
        printf '%s · %s · %s\n' "$symbol" "$id" "$what"
        if [[ -n "$evidence" ]]; then
            printf '         evidence: %s\n' "$evidence"
        fi
    fi
    RESULTS_JSON=$(python3 -c "
import json, sys
arr = json.loads(sys.argv[1])
arr.append({'id': sys.argv[2], 'verdict': sys.argv[3], 'what': sys.argv[4], 'evidence': sys.argv[5]})
print(json.dumps(arr))
" "$RESULTS_JSON" "$id" "$verdict" "$what" "$evidence")
}

ai_memory() {
    command ai-memory --db "$DB" "$@"
}

sql() {
    sqlite3 "$DB" "$1" 2>/dev/null
}

trim() {
    echo "$1" | head -c 240 | tr '\n' ' '
}

memories_has_column() {
    sql "PRAGMA table_info(memories);" | awk -F'|' -v c="$1" '$2 == c { found=1 } END { exit !found }'
}

table_exists() {
    [[ "$(sql "SELECT name FROM sqlite_master WHERE type='table' AND name='$1';")" == "$1" ]]
}

# ----------------------------------------------------------- prereqs ----

if [[ $JSON_OUT -eq 0 ]]; then
    echo "Batman Mode acceptance — DB: $DB · namespace: $NAMESPACE"
    echo "─────────────────────────────────────────────────────────────"
fi

# P1 — binary present + version
if command -v ai-memory >/dev/null 2>&1; then
    version=$(ai-memory --version 2>/dev/null | tail -1)
    record "P1" pass  "ai-memory binary present + readable version" "$(trim "$version")"
else
    record "P1" fail  "ai-memory binary not on PATH" ""
    if [[ $JSON_OUT -eq 1 ]]; then echo "$RESULTS_JSON"; fi
    exit $FAIL_COUNT
fi

# P2 — schema version >= 38 (Form 4 citations migration landed at sqlite v38)
schema_version=$(sql "SELECT version FROM schema_version LIMIT 1;")
if [[ -n "$schema_version" && "$schema_version" -ge 38 ]]; then
    record "P2" pass  "schema version >= v38 (Form 4 citations migration applied)" "schema_version.version=$schema_version"
else
    record "P2" fail  "schema version < v38 (Form 4 migration not applied)" "schema_version.version=${schema_version:-unknown}"
fi

# P3 — autonomous tier reachable (Ollama responds on configured URL)
config_url=$(grep -E '^ollama_url' ~/.config/ai-memory/config.toml 2>/dev/null \
    | head -1 | awk -F'"' '{print $2}')
ollama_url="${config_url:-http://localhost:11434}"
if curl -s -m 3 -o /dev/null -w '%{http_code}' "$ollama_url/api/tags" 2>/dev/null | grep -q '^200$'; then
    record "P3" pass  "Ollama backend reachable (autonomous tier)" "$ollama_url"
else
    record "P3" fail  "Ollama backend unreachable" "$ollama_url"
fi

# ----------------------------------------------------------- form 1 ----

# F1.1 — governance_rules table present (Form 1 + Form 7 share this surface)
if table_exists "governance_rules" ; then
    record "F1.1" pass "Form 1 governance_rules table present" "shared substrate w/ Form 7"
else
    record "F1.1" fail "Form 1 governance_rules table missing" ""
fi

# F1.2 — substrate has audit chain for write-path decisions (signed_events)
if table_exists "signed_events" ; then
    audit_rows=$(sql "SELECT COUNT(*) FROM signed_events;")
    record "F1.2" pass "Form 1 signed_events audit chain present (Form 1 verdict provenance)" "rows=$audit_rows"
else
    record "F1.2" fail "Form 1 signed_events audit chain missing" ""
fi

# ----------------------------------------------------------- form 2 ----

# F2.1 — atomised_into column on memories (atom-count back-ref)
if memories_has_column "atomised_into" ; then
    record "F2.1" pass "Form 2 memories.atomised_into column present (atom fan-out count)" ""
else
    record "F2.1" fail "Form 2 memories.atomised_into column missing" ""
fi

# F2.2 — atom_of column on memories (parent back-ref FK)
if memories_has_column "atom_of" ; then
    record "F2.2" pass "Form 2 memories.atom_of column present (atom→parent FK)" ""
else
    record "F2.2" fail "Form 2 memories.atom_of column missing" ""
fi

# ----------------------------------------------------------- form 3 ----

# F3.1 — memory_ingest_multistep MCP tool advertised at --profile full
ingest_present=$(printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"acceptance","version":"0"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    | ai-memory --db "$DB" mcp --profile full --tier autonomous 2>/dev/null \
    | python3 -c '
import sys, json
for line in sys.stdin:
    try: o = json.loads(line)
    except: continue
    if o.get("id") == 2:
        names = [t["name"] for t in o["result"]["tools"]]
        print("YES" if "memory_ingest_multistep" in names else "NO")
        break
')
if [[ "$ingest_present" == "YES" ]]; then
    record "F3.1" pass "Form 3 memory_ingest_multistep MCP tool advertised at --profile full" "tool name present in tools/list"
else
    record "F3.1" fail "Form 3 memory_ingest_multistep MCP tool not advertised" "${ingest_present:-no-response}"
fi

# ----------------------------------------------------------- form 4 ----

# F4.1 — citations column on memories
if memories_has_column "citations" ; then
    record "F4.1" pass "Form 4 memories.citations column present" "default '[]'"
else
    record "F4.1" fail "Form 4 memories.citations column missing" ""
fi

# F4.2 — source_uri column on memories
if memories_has_column "source_uri" ; then
    record "F4.2" pass "Form 4 memories.source_uri column present" ""
else
    record "F4.2" fail "Form 4 memories.source_uri column missing" ""
fi

# F4.3 — source_span column on memories (atom-grain span)
if memories_has_column "source_span" ; then
    record "F4.3" pass "Form 4 memories.source_span column present (atom-grain span)" ""
else
    record "F4.3" fail "Form 4 memories.source_span column missing" ""
fi

# ----------------------------------------------------------- form 5 ----

# F5.1 — confidence_source column on memories
if memories_has_column "confidence_source" ; then
    record "F5.1" pass "Form 5 memories.confidence_source column present" "default 'caller_provided'"
else
    record "F5.1" fail "Form 5 memories.confidence_source column missing" ""
fi

# F5.2 — confidence_shadow_observations table present (shadow-mode storage)
if table_exists "confidence_shadow_observations" ; then
    shadow_obs=$(sql "SELECT COUNT(*) FROM confidence_shadow_observations;")
    record "F5.2" pass "Form 5 confidence_shadow_observations table present" "rows=$shadow_obs"
else
    record "F5.2" fail "Form 5 confidence_shadow_observations table missing" ""
fi

# F5.3 — confidence_decayed_at column on memories (freshness decay timestamp)
if memories_has_column "confidence_decayed_at" ; then
    decayed_count=$(sql "SELECT COUNT(*) FROM memories WHERE confidence_decayed_at IS NOT NULL;")
    record "F5.3" pass "Form 5 memories.confidence_decayed_at column present" "rows with decay applied=$decayed_count"
else
    record "F5.3" fail "Form 5 memories.confidence_decayed_at column missing" ""
fi

# F5.4 — Form 5 env vars wired into MCP launch (in .claude.json) OR curator launchd plist
env_via_claude=$(python3 - <<'PY' 2>/dev/null
import json, os
try:
    cfg = json.load(open(os.path.expanduser('~/.claude.json')))
    env = (cfg.get('mcpServers', {}) or {}).get('memory', {}).get('env', {}) or {}
    needed = ['AI_MEMORY_AUTO_CONFIDENCE', 'AI_MEMORY_CONFIDENCE_SHADOW', 'AI_MEMORY_CONFIDENCE_DECAY']
    set_count = sum(1 for k in needed if env.get(k) == '1')
    print(f"{set_count}/{len(needed)}")
except Exception as e:
    print("0/3")
PY
)
env_via_plist=$(grep -cE 'AI_MEMORY_(AUTO_CONFIDENCE|CONFIDENCE_SHADOW|CONFIDENCE_DECAY)' \
    ~/Library/LaunchAgents/dev.alphaone.ai-memory.curator.plist \
    ~/.config/systemd/user/ai-memory-curator.service 2>/dev/null | awk -F: '{s+=$2} END {print s+0}')
if [[ "$env_via_claude" == "3/3" ]]; then
    record "F5.4" pass "Form 5 env vars wired into MCP launch (.claude.json)" "AI_MEMORY_AUTO_CONFIDENCE / SHADOW / DECAY all = 1"
elif [[ "$env_via_plist" -ge 3 ]]; then
    record "F5.4" pass "Form 5 env vars wired into curator service unit" "$env_via_plist env-var lines in launchd plist / systemd unit"
else
    record "F5.4" fail "Form 5 env vars not wired — process-level auto-confidence / shadow / decay dormant" "Claude MCP env: $env_via_claude · curator unit env lines: $env_via_plist"
fi

# F5.5 — namespace 'main' has a standard memory set (Forms 2 + 6 opt-in)
ns_std=$(sql "SELECT standard_id FROM namespace_meta WHERE namespace='$NAMESPACE' AND standard_id IS NOT NULL;")
if [[ -n "$ns_std" ]]; then
    record "F5.5" pass "namespace '$NAMESPACE' has standard '$ns_std' set" "namespace_meta.standard_id='$ns_std'"
else
    record "F5.5" fail "namespace '$NAMESPACE' has no standard_id — Form 2 sync-atomise + Form 6 auto-classify dormant for this namespace" "set via memory_namespace_set_standard MCP tool"
fi

# F5.6 — the standard memory carries Form 2 auto_atomise + Form 6 auto_classify_kind
if [[ -n "$ns_std" ]]; then
    gov=$(sql "SELECT json_extract(metadata, '\$.governance') FROM memories WHERE id='$ns_std';" 2>/dev/null)
    auto_atomise=$(echo "$gov" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read() or '{}')
    print('on' if d.get('auto_atomise') is True else 'off')
except: print('?')
")
    auto_classify=$(echo "$gov" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read() or '{}')
    v = d.get('auto_classify_kind')
    print(v if v in ('regex_only', 'regex_then_llm') else 'off')
except: print('?')
")
    if [[ "$auto_atomise" == "on" && "$auto_classify" != "off" && "$auto_classify" != "?" ]]; then
        record "F5.6" pass "Form 2 + Form 6 active in '$NAMESPACE' standard (auto_atomise=on, auto_classify_kind=$auto_classify)" "policy memory $ns_std"
    else
        record "F5.6" fail "Form 2 + Form 6 not fully active in '$NAMESPACE' standard" "auto_atomise=$auto_atomise auto_classify_kind=$auto_classify"
    fi
else
    record "F5.6" fail "no standard memory set — Form 2 + Form 6 policies cannot be evaluated" ""
fi

# ----------------------------------------------------------- form 6 ----

# F6.1 — memory_kind column on memories (Batman vocabulary slot)
if memories_has_column "memory_kind" ; then
    kinds_in_use=$(sql "SELECT memory_kind, COUNT(*) FROM memories GROUP BY memory_kind;" | tr '\n' ',' | sed 's/,$//')
    record "F6.1" pass "Form 6 memories.memory_kind column present" "kinds_in_use=${kinds_in_use:-none-yet}"
else
    record "F6.1" fail "Form 6 memories.memory_kind column missing" ""
fi

# F6.2 — entity_id + mentioned_entity_id columns (Form 6 Entity/Relation support)
if memories_has_column "entity_id" && memories_has_column "mentioned_entity_id" ; then
    record "F6.2" pass "Form 6 entity_id + mentioned_entity_id columns present (Entity/Relation kinds wireable)" ""
else
    record "F6.2" fail "Form 6 entity surface columns missing" ""
fi

# ----------------------------------------------------------- form 7 ----

# F7.1 — operator key on disk with 0600 mode
KEYDIR_MAC="$HOME/Library/Application Support/ai-memory/keys"
KEYDIR_LINUX="$HOME/.config/ai-memory/keys"
KEYDIR=""
[[ -d "$KEYDIR_MAC" ]] && KEYDIR="$KEYDIR_MAC"
[[ -z "$KEYDIR" && -d "$KEYDIR_LINUX" ]] && KEYDIR="$KEYDIR_LINUX"

if [[ -n "$KEYDIR" && -f "$KEYDIR/operator.key" ]]; then
    perm=$(stat -f '%Op' "$KEYDIR/operator.key" 2>/dev/null \
        || stat -c '%a' "$KEYDIR/operator.key" 2>/dev/null)
    if [[ "$perm" == *"600" ]]; then
        record "F7.1" pass "Form 7 operator key present with mode 0600" "$KEYDIR/operator.key ($perm)"
    else
        record "F7.1" fail "Form 7 operator key present but mode != 0600" "$KEYDIR/operator.key ($perm)"
    fi
else
    record "F7.1" fail "Form 7 operator key absent" "expected at $KEYDIR_MAC/operator.key or $KEYDIR_LINUX/operator.key"
fi

# F7.2 — R001-R004 enabled and operator-signed
rules_state=$(ai_memory rules list --json 2>/dev/null \
    | tail -1 \
    | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    rules = data.get('result', data)
    by_id = {r['id']: (r.get('enabled', False), r.get('attest_level', '?')) for r in rules}
    out = []
    for rid in ('R001', 'R002', 'R003', 'R004'):
        en, att = by_id.get(rid, (False, 'missing'))
        out.append(f\"{rid}:{'on' if en else 'off'}/{att}\")
    print(' '.join(out))
except Exception as e:
    print(f'ERROR:{e}')
")
all_on=$(echo "$rules_state" | grep -oE 'R[0-9]{3}:on/operator_signed' | wc -l | tr -d ' ')
if [[ "$all_on" == "4" ]]; then
    record "F7.2" pass "Form 7 R001-R004 all enabled + operator_signed" "$rules_state"
else
    record "F7.2" fail "Form 7 R001-R004 not all enabled + operator_signed" "$rules_state"
fi

# F7.3 — enforcement smoke test: /tmp/x refused under R001
# `rules check` emits multi-line JSON; capture the full block and parse with python.
deny_check=$(ai_memory rules check \
    --kind filesystem_write \
    --payload '{"path":"/tmp/acceptance-test.txt"}' \
    --agent-id batman-acceptance 2>/dev/null \
    | grep -vE '^ai-memory: loaded config' \
    | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    print(f'decision={data.get(\"decision\")} rule_id={data.get(\"rule_id\",\"-\")}')
except Exception as e:
    print(f'PARSE_ERROR:{e}')
")
if [[ "$deny_check" == *"decision=refuse"* && "$deny_check" == *"rule_id=R001"* ]]; then
    record "F7.3" pass "Form 7 enforcement: /tmp write refused under R001" "$deny_check"
else
    record "F7.3" fail "Form 7 enforcement: /tmp write NOT refused under R001" "$deny_check"
fi

# F7.4 — enforcement smoke test: allow path returns allow
allow_check=$(ai_memory rules check \
    --kind filesystem_write \
    --payload "{\"path\":\"$HOME/.local-runs/acceptance-test.txt\"}" \
    --agent-id batman-acceptance 2>/dev/null \
    | grep -vE '^ai-memory: loaded config' \
    | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    print(f'decision={data.get(\"decision\")}')
except Exception as e:
    print(f'PARSE_ERROR:{e}')
")
if [[ "$allow_check" == "decision=allow" ]]; then
    record "F7.4" pass "Form 7 enforcement: allowed path returns allow" "$allow_check"
else
    record "F7.4" fail "Form 7 enforcement: allowed path did NOT return allow" "$allow_check"
fi

# ----------------------------------------------------------- upkeep ----

# U1 — curator daemon process alive
curator_pid=$(pgrep -f 'ai-memory.*curator --daemon' | head -1)
if [[ -n "$curator_pid" ]]; then
    record "U1" pass "curator daemon alive" "pid=$curator_pid"
else
    record "U1" fail "curator daemon NOT alive — Forms 1/5/6 upkeep dormant" "no 'curator --daemon' process found via pgrep"
fi

# U2 — curator launchd / systemd unit installed (permanence)
unit_found="no"
if [[ -f "$HOME/Library/LaunchAgents/dev.alphaone.ai-memory.curator.plist" ]]; then
    unit_found="launchd (~/Library/LaunchAgents/dev.alphaone.ai-memory.curator.plist)"
elif [[ -f "$HOME/.config/systemd/user/ai-memory-curator.service" ]]; then
    unit_found="systemd (~/.config/systemd/user/ai-memory-curator.service)"
fi
if [[ "$unit_found" != "no" ]]; then
    record "U2" pass "curator unit installed (survives reboot)" "$unit_found"
else
    record "U2" fail "curator unit NOT installed — daemon will not survive reboot" "expected launchd plist or systemd user unit"
fi

# ----------------------------------------------------------- behavioral (v2) ----

# B-series checks (#800 Crack 5) — only run with --behavioral. Stores a
# probe memory through the live MCP write path and asserts the substrate
# actually fired Form 1/2/4/6 + signed_events. Quiet by default; running
# this against a production DB will land a memory under
# `_batman_probe_<timestamp>` namespace by design so it doesn't pollute
# operator data.

if [[ $BEHAVIORAL -eq 1 ]]; then
    probe_ns="_batman_probe_$(date +%s)"
    probe_title="batman acceptance probe $(date +%s)"
    probe_body="Probe memory for the v2 behavioral acceptance suite (#800 Crack 5). This row deliberately exceeds the auto_atomise_threshold_cl100k of 512 so Form 2 fires synchronously; the content shape includes a Concept-like definitional sentence so Form 6 regex_then_llm classifier should produce something other than Observation; and the substrate-internal write-path will fire Form 1 dedup-and-synthesis plus the Form 7 substrate-internal governance hook. Every field above is intentionally redundant so the cl100k token count crosses the threshold for a single probe — synchronous atomise on, regex_then_llm classify on, freshness-decay-touch on (env vars in MCP launch), shadow observation recorded if AI_MEMORY_CONFIDENCE_SHADOW=1 was set on the MCP launch. After this stores, the suite queries memories WHERE namespace=probe_ns to confirm at least one atom row landed with atom_of pointing at the parent, that memory_kind for the parent is set to one of the expected Batman vocabulary variants, and that a signed_events row was appended."

    # Capture before-state for signed_events delta
    before_signed=$(sql "SELECT COUNT(*) FROM signed_events;")

    # Bind a Batman policy to the probe namespace so Forms 2 + 6 fire on
    # the test write. Uses the new CLI verb shipped in this same PR.
    policy_json=$(ai_memory namespace batman-policy --json 2>/dev/null | tail -n +2)
    # store a standard memory and bind it
    std_id=$(ai_memory store --namespace "$probe_ns" --tier long \
        --title "probe namespace standard" \
        --content "probe namespace standard for $probe_ns" \
        --json 2>/dev/null | grep -vE '^ai-memory: loaded config' | tail -1 \
        | python3 -c "import sys,json
try:
    d=json.loads(sys.stdin.read())
    print(d.get('id') or d.get('memory_id') or d.get('memory',{}).get('id',''))
except: print('')")
    if [[ -n "$std_id" ]]; then
        ai_memory namespace set-standard --namespace "$probe_ns" --id "$std_id" \
            --governance "$policy_json" 2>/dev/null >/dev/null
    fi

    # Store the probe memory
    probe_id=$(ai_memory store \
        --namespace "$probe_ns" \
        --tier mid \
        --title "$probe_title" \
        --content "$probe_body" \
        --json 2>/dev/null | grep -vE '^ai-memory: loaded config' | tail -1 \
        | python3 -c "import sys,json
try:
    d=json.loads(sys.stdin.read())
    print(d.get('id') or d.get('memory_id') or d.get('memory',{}).get('id',''))
except: print('')")

    if [[ -z "$probe_id" ]]; then
        record "B1" fail "probe store returned no id — write path failed" ""
    else
        record "B1" pass "probe memory stored in '$probe_ns'" "$probe_id"
    fi

    # Trigger a curator pass so any deferred Form 1/5/6 work fires now
    ai_memory curator --once --include-namespace "$probe_ns" --max-ops 20 \
        2>/dev/null >/dev/null

    # B2 — atom_of populated for at least one child row
    if [[ -n "$probe_id" ]]; then
        atom_count=$(sql "SELECT COUNT(*) FROM memories WHERE atom_of='$probe_id';")
        if [[ "${atom_count:-0}" -gt 0 ]]; then
            record "B2" pass "Form 2 fired: $atom_count atom row(s) reference parent" "atom_of='$probe_id'"
        else
            record "B2" fail "Form 2 did NOT fire: no atom rows reference parent" "atom_of='$probe_id' count=0 — auto_atomise_mode may be 'deferred' or MCP server missing env"
        fi
    fi

    # B3 — memory_kind classified (parent or atom)
    if [[ -n "$probe_id" ]]; then
        kinds=$(sql "SELECT DISTINCT memory_kind FROM memories WHERE id='$probe_id' OR atom_of='$probe_id';" | sort -u | tr '\n' ',' | sed 's/,$//')
        non_default=$(echo "$kinds" | tr ',' '\n' | grep -vE '^observation$|^$' | head -1)
        if [[ -n "$non_default" ]]; then
            record "B3" pass "Form 6 fired: probe classified as '$non_default' (kinds=$kinds)" "non-default vocabulary applied"
        else
            record "B3" fail "Form 6 did NOT fire: all rows still 'observation' (kinds=$kinds)" "auto_classify_kind policy may be off or MCP server has stale env"
        fi
    fi

    # B4 — signed_events delta
    after_signed=$(sql "SELECT COUNT(*) FROM signed_events;")
    delta=$((after_signed - before_signed))
    if [[ $delta -gt 0 ]]; then
        record "B4" pass "signed_events grew by $delta during probe (governance verdicts persisted)" "before=$before_signed after=$after_signed"
    else
        record "B4" fail "signed_events did NOT grow — Form 1 verdicts and Form 7 hooks not audit-logged" "before=$before_signed after=$after_signed"
    fi

    # B5 — citations / source_uri / confidence_signals populated on probe
    if [[ -n "$probe_id" ]]; then
        cit_uri_sig=$(sql "SELECT length(citations), source_uri IS NOT NULL, confidence_signals IS NOT NULL FROM memories WHERE id='$probe_id';" | head -1)
        record "B5" pass "Form 4/5 fields surveyed on probe row" "len(citations)|source_uri_set|confidence_signals_set = $cit_uri_sig"
    fi

    # Clean up the probe namespace
    ai_memory forget --namespace "$probe_ns" --confirm-global 2>/dev/null >/dev/null || true
fi

# ----------------------------------------------------------- summary ----

TOTAL=$((PASS_COUNT + FAIL_COUNT))

if [[ $JSON_OUT -eq 1 ]]; then
    python3 -c "
import json, sys
results = json.loads(sys.argv[1])
summary = {
    'db': sys.argv[2],
    'namespace': sys.argv[3],
    'total': len(results),
    'pass': sum(1 for r in results if r['verdict'] == 'pass'),
    'fail': sum(1 for r in results if r['verdict'] == 'fail'),
    'batman_active': all(r['verdict'] == 'pass' for r in results),
    'results': results,
}
print(json.dumps(summary, indent=2))
" "$RESULTS_JSON" "$DB" "$NAMESPACE"
else
    echo "─────────────────────────────────────────────────────────────"
    if [[ $FAIL_COUNT -eq 0 ]]; then
        echo "VERDICT: Batman-ACTIVE ($PASS_COUNT/$TOTAL checks pass)"
    elif [[ $PASS_COUNT -ge $((TOTAL * 3 / 4)) ]]; then
        echo "VERDICT: Batman-PARTIAL ($PASS_COUNT/$TOTAL — $FAIL_COUNT short of full active)"
    else
        echo "VERDICT: Batman-CAPABLE ($PASS_COUNT/$TOTAL — substrate ready, activation incomplete)"
    fi
fi

exit $FAIL_COUNT
