#!/usr/bin/env bash
# install-batman-active.sh — one-shot Batman Mode activator (#800 Crack 2).
#
# Closes the 7-manual-step gap by running the full activation recipe:
#
#   1. Generate Ed25519 operator keypair (with the keygen↔enable path
#      workaround applied).
#   2. Sign the four seed rules R001-R004.
#   3. Enable R001-R004 (--sign).
#   4. Smoke-test enforcement.
#   5. Write the macOS launchd plist for the curator daemon (with
#      Form 5 env vars baked in) and load it.
#   6. Add Form 5 env vars to ~/.claude.json (mcpServers.memory.env)
#      with a backup of the prior file.
#   7. Create + bind a Batman-active namespace standard memory via the
#      new `ai-memory namespace set-standard` verb (#800 Crack 1).
#
# Idempotent. Re-running detects existing state and skips:
#   - operator key already present → step 1 skipped
#   - seed rules already enabled → step 3 marks them already-on
#   - launchd plist already loaded → step 5 reloads
#   - .claude.json env vars already set → step 6 no-op
#   - namespace already has a standard → step 7 honored with --reset to
#     overwrite, otherwise skipped
#
# Usage:
#   scripts/install-batman-active.sh                    # default namespace = main
#   scripts/install-batman-active.sh --namespace ai-memory-mcp
#   scripts/install-batman-active.sh --db /path/to.db
#   scripts/install-batman-active.sh --dry-run          # print what would happen
#   scripts/install-batman-active.sh --reset            # overwrite existing standard
#
# Companion doc: docs/batman-active-mode.md (per-step rationale).
# Companion test: scripts/batman-mode-acceptance.sh (verifies the result).

set -uo pipefail

DB="${AI_MEMORY_DB:-$HOME/.claude/ai-memory.db}"
NAMESPACE="${BATMAN_NAMESPACE:-main}"
DRY_RUN=0
RESET=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --db)        DB="$2"; shift 2 ;;
        --namespace) NAMESPACE="$2"; shift 2 ;;
        --dry-run)   DRY_RUN=1; shift ;;
        --reset)     RESET=1; shift ;;
        -h|--help)
            sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

step()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
info()  { printf '    %s\n' "$*"; }
ok()    { printf '    \033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '    \033[33m⚠\033[0m %s\n' "$*"; }
err()   { printf '    \033[31m✗\033[0m %s\n' "$*"; }
run()   {
    if [[ $DRY_RUN -eq 1 ]]; then
        printf '    [dry-run] %s\n' "$*"
        return 0
    fi
    eval "$@"
}

# ---------------------------------------------------------------- prereqs ---

step "Prereqs"
if ! command -v ai-memory >/dev/null 2>&1; then
    err "ai-memory not on PATH"; exit 2
fi
VERSION=$(ai-memory --version 2>/dev/null | tail -1)
ok "ai-memory present: $VERSION"
if [[ ! -f "$DB" ]]; then
    err "DB does not exist: $DB"; exit 2
fi
ok "DB present: $DB"
PLATFORM=$(uname -s)
case "$PLATFORM" in
    Darwin)  KEYDIR="$HOME/Library/Application Support/ai-memory/keys" ;;
    Linux)   KEYDIR="$HOME/.config/ai-memory/keys" ;;
    *)       err "unsupported platform: $PLATFORM"; exit 2 ;;
esac
mkdir -p "$KEYDIR"
ok "key directory: $KEYDIR"

# ----------------------------------------------------------- step 1 ----

step "Step 1 — Operator keypair"
PARENT="$(dirname "$KEYDIR")"
# Mirror logic. Use cp/chmod directly (not via run/eval) so paths with
# spaces — e.g. macOS "Application Support" — survive shell expansion.
mirror_key() {
    local src_dir="$1" dst_dir="$2"
    if [[ -f "$src_dir/operator.key" && ! -f "$dst_dir/operator.key" ]]; then
        if [[ $DRY_RUN -eq 1 ]]; then
            info "[dry-run] cp \"$src_dir/operator.key\" \"$dst_dir/\""
            info "[dry-run] cp \"$src_dir/operator.key.pub\" \"$dst_dir/\""
        else
            cp "$src_dir/operator.key"     "$dst_dir/operator.key"
            cp "$src_dir/operator.key.pub" "$dst_dir/operator.key.pub"
            chmod 0600 "$dst_dir/operator.key"
            chmod 0644 "$dst_dir/operator.key.pub"
        fi
        ok "mirrored operator.key: $src_dir -> $dst_dir"
    fi
}

if [[ -f "$KEYDIR/operator.key" && -f "$KEYDIR/operator.key.pub" ]]; then
    ok "operator key already present in keys/ — skipping keygen"
else
    if [[ $DRY_RUN -eq 1 ]]; then
        info "[dry-run] ai-memory rules keygen"
    else
        ai-memory rules keygen 2>&1 | sed 's/^/    /' || true
    fi
    ok "operator key generated"
fi

# Workaround for v0.7.0 keygen↔enable path mismatch: keygen writes to
# <config-dir>/operator.key, sign-seed reads it from the same place,
# but rules enable/disable/remove look in <config-dir>/keys/. Mirror in
# both directions so every verb finds its expected path.
mirror_key "$PARENT" "$KEYDIR"
mirror_key "$KEYDIR" "$PARENT"

# ----------------------------------------------------------- step 2 ----

step "Step 2 — Sign seed rules R001-R004"
if [[ $DRY_RUN -eq 1 ]]; then
    info "[dry-run] ai-memory --db ... rules sign-seed"
    SIGN_OUT='{"signed_now": 0}'
else
    SIGN_OUT=$(ai-memory --db "$DB" rules sign-seed 2>&1 | grep -vE '^ai-memory: loaded config')
fi
if echo "$SIGN_OUT" | grep -q '"signed_now"'; then
    SIGNED_NOW=$(echo "$SIGN_OUT" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('signed_now',0))")
    if [[ "$SIGNED_NOW" -gt 0 ]]; then
        ok "signed $SIGNED_NOW seed rule(s) → attest_level=operator_signed"
    else
        ok "seed rules already signed (no-op)"
    fi
else
    warn "sign-seed unexpected output: $(echo "$SIGN_OUT" | head -3)"
fi

# ----------------------------------------------------------- step 3 ----

step "Step 3 — Enable R001-R004"
if [[ $DRY_RUN -eq 1 ]]; then
    for r in R001 R002 R003 R004; do
        info "[dry-run] ai-memory --db ... rules enable --id $r --sign"
    done
else
    for r in R001 R002 R003 R004; do
        EN=$(ai-memory --db "$DB" rules enable --id "$r" --sign 2>&1 | grep -vE '^ai-memory: loaded config')
        if echo "$EN" | grep -q '"enabled": true'; then
            ok "$r enabled"
        elif echo "$EN" | grep -qE 'already enabled|enabled.*true'; then
            ok "$r already enabled"
        else
            warn "$r enable: $(echo "$EN" | head -2)"
        fi
    done
fi

# ----------------------------------------------------------- step 4 ----

step "Step 4 — Smoke-test Form 7 enforcement"
if [[ $DRY_RUN -eq 1 ]]; then
    info "[dry-run] ai-memory --db ... rules check --kind filesystem_write --payload '{\"path\":\"/tmp/x\"}' --agent-id install-batman"
    info "[dry-run] (every rules-check emits a signed_events audit row, so this is gated under dry-run too)"
else
    DENY=$(ai-memory --db "$DB" rules check --kind filesystem_write \
        --payload '{"path":"/tmp/install-batman-test.txt"}' \
        --agent-id install-batman 2>&1 | grep -vE '^ai-memory: loaded config' \
        | python3 -c "import sys,json;
try:
    d=json.loads(sys.stdin.read());print(f\"{d.get('decision')} {d.get('rule_id','-')}\")
except:print('PARSE_ERROR')")
    if [[ "$DENY" == "refuse R001" ]]; then
        ok "/tmp write refused under R001 ✓"
    else
        err "/tmp write NOT refused — Form 7 not active. Got: $DENY"
    fi
fi

# ----------------------------------------------------------- step 5 ----

step "Step 5 — Curator daemon under launchd (macOS) or systemd (Linux)"
if [[ "$PLATFORM" == "Darwin" ]]; then
    PLIST="$HOME/Library/LaunchAgents/dev.alphaone.ai-memory.curator.plist"
    if [[ -f "$PLIST" ]]; then
        ok "plist already present at $PLIST — reloading"
        run launchctl bootout "gui/$(id -u)/dev.alphaone.ai-memory.curator" 2>/dev/null || true
    else
        mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/ai-memory"
        AI_MEMORY_BIN=$(command -v ai-memory)
        cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.alphaone.ai-memory.curator</string>
    <key>ProgramArguments</key>
    <array>
        <string>$AI_MEMORY_BIN</string>
        <string>--db</string>
        <string>$DB</string>
        <string>curator</string>
        <string>--daemon</string>
        <string>--interval-secs</string>
        <string>300</string>
        <string>--max-ops</string>
        <string>100</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
        <key>HOME</key>
        <string>$HOME</string>
        <key>AI_MEMORY_AUTO_CONFIDENCE</key>
        <string>1</string>
        <key>AI_MEMORY_CONFIDENCE_SHADOW</key>
        <string>1</string>
        <key>AI_MEMORY_CONFIDENCE_DECAY</key>
        <string>1</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key><false/>
        <key>Crashed</key><true/>
    </dict>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>StandardOutPath</key>
    <string>$HOME/Library/Logs/ai-memory/curator.log</string>
    <key>StandardErrorPath</key>
    <string>$HOME/Library/Logs/ai-memory/curator.log</string>
    <key>ProcessType</key><string>Background</string>
    <key>Nice</key><integer>5</integer>
</dict>
</plist>
PLIST_EOF
        ok "wrote $PLIST"
    fi
    run launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>&1 | sed 's/^/    /'
    sleep 1
    if pgrep -f 'ai-memory.*curator --daemon' >/dev/null 2>&1; then
        ok "curator daemon active (pid $(pgrep -f 'ai-memory.*curator --daemon' | head -1))"
    else
        warn "curator did not start; check ~/Library/Logs/ai-memory/curator.log"
    fi
elif [[ "$PLATFORM" == "Linux" ]]; then
    UNIT="$HOME/.config/systemd/user/ai-memory-curator.service"
    mkdir -p "$(dirname "$UNIT")" "$HOME/.local/state/ai-memory/log"
    AI_MEMORY_BIN=$(command -v ai-memory)
    if [[ ! -f "$UNIT" ]]; then
        cat > "$UNIT" <<UNIT_EOF
[Unit]
Description=ai-memory autonomous curator (Batman Mode)
After=network.target

[Service]
Type=simple
ExecStart=$AI_MEMORY_BIN --db $DB curator --daemon --interval-secs 300 --max-ops 100
Environment=PATH=$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin
Environment=AI_MEMORY_AUTO_CONFIDENCE=1
Environment=AI_MEMORY_CONFIDENCE_SHADOW=1
Environment=AI_MEMORY_CONFIDENCE_DECAY=1
StandardOutput=append:$HOME/.local/state/ai-memory/log/curator.log
StandardError=append:$HOME/.local/state/ai-memory/log/curator.log
Restart=on-failure
RestartSec=30s
Nice=5

[Install]
WantedBy=default.target
UNIT_EOF
        ok "wrote $UNIT"
    fi
    run systemctl --user daemon-reload
    run systemctl --user enable --now ai-memory-curator.service
    ok "curator unit loaded (systemctl --user status ai-memory-curator)"
fi

# ----------------------------------------------------------- step 6 ----

step "Step 6 — Form 5 env vars in ~/.claude.json"
CLAUDE_JSON="$HOME/.claude.json"
if [[ -f "$CLAUDE_JSON" ]]; then
    if [[ $DRY_RUN -eq 1 ]]; then
        info "[dry-run] would patch $CLAUDE_JSON mcpServers.memory.env"
    else
        python3 - <<PY
import json, os, shutil, time
path = os.path.expanduser('~/.claude.json')
backup = path + '.bak-batman-' + time.strftime('%Y%m%d-%H%M%S')
shutil.copyfile(path, backup)
cfg = json.load(open(path))
mem = cfg.setdefault('mcpServers', {}).setdefault('memory', {})
env = mem.setdefault('env', {})
already = all(env.get(k) == '1' for k in ('AI_MEMORY_AUTO_CONFIDENCE','AI_MEMORY_CONFIDENCE_SHADOW','AI_MEMORY_CONFIDENCE_DECAY'))
env.update({
    'AI_MEMORY_AUTO_CONFIDENCE': '1',
    'AI_MEMORY_CONFIDENCE_SHADOW': '1',
    'AI_MEMORY_CONFIDENCE_DECAY': '1',
})
with open(path, 'w') as f:
    json.dump(cfg, f, indent=2)
print(f"    backup: {backup}")
print("    " + ("env vars already set — no change" if already else "env vars added — restart Claude Code to apply"))
PY
        ok ".claude.json env vars wired"
    fi
else
    warn "$CLAUDE_JSON not found — set AI_MEMORY_AUTO_CONFIDENCE / SHADOW / DECAY on your MCP launch manually"
fi

# ----------------------------------------------------------- step 7 ----

step "Step 7 — Namespace standard for '$NAMESPACE'"
# Read the full multi-line JSON output from get-standard --json. The
# CLI emits a pretty-printed JSON block; concatenate all post-banner
# lines and parse.
EXISTING=$(ai-memory --db "$DB" --json namespace get-standard --namespace "$NAMESPACE" 2>/dev/null \
    | grep -vE '^ai-memory: loaded config' \
    | python3 -c "
import sys, json
raw = sys.stdin.read().strip()
if not raw:
    print('')
else:
    try:
        d = json.loads(raw)
        print(d.get('standard_id') or '')
    except Exception:
        print('')
")
if [[ -n "$EXISTING" && $RESET -eq 0 ]]; then
    ok "namespace '$NAMESPACE' already bound to standard $EXISTING — pass --reset to overwrite"
else
    if [[ $DRY_RUN -eq 1 ]]; then
        info "[dry-run] would create a Batman-active standard memory and bind it to '$NAMESPACE'"
    else
        # Capture the full multi-line pretty-printed JSON from
        # `batman-policy`, stripping only the loaded-config banner.
        # `tail -1` would grab only `}` which is not parseable.
        POLICY_JSON=$(ai-memory namespace batman-policy --json 2>/dev/null \
            | grep -vE '^ai-memory: loaded config' \
            | python3 -c "import sys,json; print(json.dumps(json.loads(sys.stdin.read())))")
        STORE_OUT=$(ai-memory --db "$DB" store \
            --namespace "$NAMESPACE" \
            --title "batman-active standard for $NAMESPACE" \
            --content "Namespace standard for the $NAMESPACE namespace: Form 2 synchronous atomise-before-embed + Form 6 auto-classify (regex_then_llm). Issue #800. Generated by install-batman-active.sh." \
            --tier long --priority 10 \
            --json 2>&1 | grep -vE '^ai-memory: loaded config' | tail -1)
        STD_ID=$(echo "$STORE_OUT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(d.get('id') or d.get('memory_id') or d.get('memory', {}).get('id', ''))
except: print('')
")
        if [[ -z "$STD_ID" ]]; then
            err "could not capture stored memory id; output was: $(echo "$STORE_OUT" | head -3)"
        else
            BIND_OUT=$(ai-memory --db "$DB" namespace set-standard \
                --namespace "$NAMESPACE" \
                --id "$STD_ID" \
                --governance "$POLICY_JSON" 2>&1 | grep -vE '^ai-memory: loaded config')
            if echo "$BIND_OUT" | grep -qE "standard_id='?$STD_ID|\"standard_id\":\\s*\"$STD_ID\""; then
                ok "bound '$NAMESPACE' → $STD_ID (Forms 2 + 6 active)"
            else
                warn "bind output: $(echo "$BIND_OUT" | head -3)"
            fi
        fi
    fi
fi

# ----------------------------------------------------------- summary ----

step "Done"
ok "Batman Mode installation complete."
info ""
info "Verify with:"
info "  scripts/batman-mode-acceptance.sh --db \"$DB\" --namespace \"$NAMESPACE\""
info ""
info "Restart Claude Code (or your MCP client) to pick up the new Form 5 env vars."
info "The curator daemon is already running and persists across reboot."
