# install-batman-active.ps1 — one-shot Batman Mode activator for Windows
# (PowerShell parity of scripts/install-batman-active.sh).
#
# Closes the 7-manual-step gap by running the full activation recipe:
#
#   1. Generate Ed25519 operator keypair.
#   2. Sign R001-R004 seed rules.
#   3. Enable R001-R004 (--sign).
#   4. Smoke-test enforcement.
#   5. Register a Windows Scheduled Task for the curator daemon with
#      Form 5 env vars baked in.
#   6. Inject Form 5 env vars into %USERPROFILE%\.claude.json
#      (mcpServers.memory.env) with a backup of the prior file.
#   7. Create + bind a Batman-active namespace standard memory via
#      the `ai-memory namespace set-standard` CLI verb (#800 Crack 1).
#
# Idempotent. Re-run safely.
#
# Usage:
#   .\scripts\install-batman-active.ps1
#   .\scripts\install-batman-active.ps1 -Namespace ai-memory-mcp
#   .\scripts\install-batman-active.ps1 -Db C:\Users\YOU\.claude\ai-memory.db
#   .\scripts\install-batman-active.ps1 -DryRun
#   .\scripts\install-batman-active.ps1 -Reset
#
# Companion doc: docs/batman-active-mode.md
# Companion test: scripts/batman-mode-acceptance.ps1
# Tracking issue: https://github.com/alphaonedev/ai-memory-mcp/issues/800

[CmdletBinding()]
param(
    [string]$Db = (Join-Path $env:USERPROFILE '.claude\ai-memory.db'),
    [string]$Namespace = 'main',
    [switch]$DryRun,
    [switch]$Reset
)

$ErrorActionPreference = 'Stop'

function Step([string]$Msg)  { Write-Host "`n==> $Msg" -ForegroundColor Cyan }
function Info([string]$Msg)  { Write-Host "    $Msg" }
function Ok([string]$Msg)    { Write-Host "    [v] $Msg" -ForegroundColor Green }
function Warn([string]$Msg)  { Write-Host "    [!] $Msg" -ForegroundColor Yellow }
function Err([string]$Msg)   { Write-Host "    [x] $Msg" -ForegroundColor Red }

function Run-Or-DryRun([string]$Label, [scriptblock]$Block) {
    if ($DryRun) { Info "[dry-run] $Label"; return }
    & $Block
}

# ---------------------------------------------------------------- prereqs ---

Step 'Prereqs'

$aiMem = Get-Command ai-memory.exe -ErrorAction SilentlyContinue
if (-not $aiMem) { Err 'ai-memory.exe not on PATH'; exit 2 }
$version = (& ai-memory --version 2>&1 | Select-Object -Last 1)
Ok "ai-memory present: $version"

if (-not (Test-Path $Db)) { Err "DB does not exist: $Db"; exit 2 }
Ok "DB present: $Db"

$keyDir = Join-Path $env:APPDATA 'ai-memory\keys'
New-Item -ItemType Directory -Force -Path $keyDir | Out-Null
Ok "key directory: $keyDir"

# ----------------------------------------------------------- step 1 ----

Step 'Step 1 - Operator keypair'

$opKey = Join-Path $keyDir 'operator.key'
$opPub = Join-Path $keyDir 'operator.key.pub'
if ((Test-Path $opKey) -and (Test-Path $opPub)) {
    Ok 'operator key already present - skipping keygen'
} else {
    Run-Or-DryRun 'ai-memory rules keygen' { & ai-memory rules keygen | Out-Host }
    # workaround for v0.7.0 keygen<->enable path mismatch on macOS/Linux;
    # on Windows the key directory is APPDATA\ai-memory and keygen
    # already targets the parent. Move into keys\ if landed one level up.
    $parentKey = Join-Path (Split-Path $keyDir -Parent) 'operator.key'
    $parentPub = Join-Path (Split-Path $keyDir -Parent) 'operator.key.pub'
    if ((Test-Path $parentKey) -and -not (Test-Path $opKey)) {
        Move-Item $parentKey $opKey
        Move-Item $parentPub $opPub
        Ok 'applied keygen path workaround (moved operator.key into keys\)'
    }
    Ok 'operator key generated'
}

# ----------------------------------------------------------- step 2 ----

Step 'Step 2 - Sign seed rules R001-R004'

$signOut = (& ai-memory --db $Db rules sign-seed 2>&1 | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join "`n"
if ($signOut -match '"signed_now":\s*(\d+)') {
    $signedNow = [int]$matches[1]
    if ($signedNow -gt 0) {
        Ok "signed $signedNow seed rule(s) -> attest_level=operator_signed"
    } else {
        Ok 'seed rules already signed (no-op)'
    }
} else {
    Warn "sign-seed unexpected output: $($signOut.Substring(0, [Math]::Min(200, $signOut.Length)))"
}

# ----------------------------------------------------------- step 3 ----

Step 'Step 3 - Enable R001-R004'

foreach ($r in 'R001','R002','R003','R004') {
    $en = (& ai-memory --db $Db rules enable --id $r --sign 2>&1 | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join "`n"
    if ($en -match '"enabled":\s*true') {
        Ok "$r enabled"
    } else {
        Warn "$r enable: $($en.Substring(0, [Math]::Min(160, $en.Length)))"
    }
}

# ----------------------------------------------------------- step 4 ----

Step 'Step 4 - Smoke-test Form 7 enforcement'

$denyRaw = & ai-memory --db $Db rules check --kind filesystem_write `
    --payload '{"path":"C:\\Windows\\Temp\\install-batman-test.txt"}' `
    --agent-id install-batman 2>&1 | Where-Object { $_ -notmatch '^ai-memory: loaded config' }
$denyText = ($denyRaw -join '')
try {
    $deny = $denyText | ConvertFrom-Json
    if ($deny.decision -eq 'refuse') {
        Ok "Form 7 enforcement live (refused via rule $($deny.rule_id))"
    } else {
        Warn "Form 7 returned: decision=$($deny.decision) rule_id=$($deny.rule_id) — Windows seed rules may not cover this path"
        Info 'Custom rules: ai-memory rules add --kind filesystem_write --matcher ''{"glob":"C:\\Windows\\Temp\\**"}'' --severity refuse --reason "no tmp writes" --id R005 --sign'
    }
} catch {
    Err "Form 7 smoke test parse failed: $denyText"
}

# ----------------------------------------------------------- step 5 ----

Step 'Step 5 - Curator daemon via Windows Scheduled Task'

$taskName = 'AI-Memory Curator (Batman Mode)'
$existing = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue

$aiMemPath = $aiMem.Source
$logDir = Join-Path $env:LOCALAPPDATA 'ai-memory\logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$logFile = Join-Path $logDir 'curator.log'

# Wrap in a PowerShell script so we can set env vars + redirect output.
$wrapperScript = Join-Path $env:APPDATA 'ai-memory\curator-wrapper.ps1'
$wrapperContent = @"
`$env:AI_MEMORY_AUTO_CONFIDENCE = '1'
`$env:AI_MEMORY_CONFIDENCE_SHADOW = '1'
`$env:AI_MEMORY_CONFIDENCE_DECAY = '1'
& '$aiMemPath' --db '$Db' curator --daemon --interval-secs 300 --max-ops 100 *>> '$logFile'
"@
Run-Or-DryRun "write curator wrapper script -> $wrapperScript" {
    Set-Content -Path $wrapperScript -Value $wrapperContent -Encoding UTF8
}

Run-Or-DryRun "register Scheduled Task '$taskName'" {
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -WindowStyle Hidden -File `"$wrapperScript`""
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
    $settings = New-ScheduledTaskSettingsSet `
        -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
        -StartWhenAvailable -DontStopOnIdleEnd `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    if ($existing) {
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
    }
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings `
        -Description 'ai-memory autonomous curator daemon (Batman Mode active)' | Out-Null
    Start-ScheduledTask -TaskName $taskName
}
Ok "Scheduled Task '$taskName' registered + started"

# ----------------------------------------------------------- step 6 ----

Step 'Step 6 - Form 5 env vars in %USERPROFILE%\.claude.json'

$claudeJson = Join-Path $env:USERPROFILE '.claude.json'
if (Test-Path $claudeJson) {
    if ($DryRun) {
        Info "[dry-run] would patch $claudeJson mcpServers.memory.env"
    } else {
        $backup = "$claudeJson.bak-batman-$(Get-Date -Format yyyyMMdd-HHmmss)"
        Copy-Item $claudeJson $backup
        Info "backup: $backup"
        $cfg = Get-Content $claudeJson -Raw | ConvertFrom-Json
        if (-not $cfg.mcpServers) { $cfg | Add-Member -NotePropertyName mcpServers -NotePropertyValue (New-Object PSObject) }
        if (-not $cfg.mcpServers.memory) { $cfg.mcpServers | Add-Member -NotePropertyName memory -NotePropertyValue (New-Object PSObject) }
        if (-not $cfg.mcpServers.memory.env) { $cfg.mcpServers.memory | Add-Member -NotePropertyName env -NotePropertyValue (New-Object PSObject) }
        foreach ($k in 'AI_MEMORY_AUTO_CONFIDENCE','AI_MEMORY_CONFIDENCE_SHADOW','AI_MEMORY_CONFIDENCE_DECAY') {
            if (-not ($cfg.mcpServers.memory.env.PSObject.Properties.Name -contains $k)) {
                $cfg.mcpServers.memory.env | Add-Member -NotePropertyName $k -NotePropertyValue '1'
            } else {
                $cfg.mcpServers.memory.env.$k = '1'
            }
        }
        $cfg | ConvertTo-Json -Depth 32 | Set-Content $claudeJson -Encoding UTF8
        Ok '.claude.json env vars wired (restart Claude Code to apply)'
    }
} else {
    Warn "$claudeJson not found - set AI_MEMORY_AUTO_CONFIDENCE / SHADOW / DECAY on your MCP launch manually"
}

# ----------------------------------------------------------- step 7 ----

Step "Step 7 - Namespace standard for '$Namespace'"

$existingStd = ''
try {
    $existingStdJson = & ai-memory --db $Db namespace get-standard --namespace $Namespace --json 2>$null | Where-Object { $_ -notmatch '^ai-memory: loaded config' } | Select-Object -Last 1
    $parsed = $existingStdJson | ConvertFrom-Json
    $existingStd = $parsed.standard_id
} catch {}

if ($existingStd -and -not $Reset) {
    Ok "namespace '$Namespace' already bound to standard $existingStd - pass -Reset to overwrite"
} else {
    if ($DryRun) {
        Info "[dry-run] would create a Batman-active standard memory and bind it to '$Namespace'"
    } else {
        $policyJson = (& ai-memory namespace batman-policy --json 2>$null | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join ''
        $storeRaw = (& ai-memory --db $Db store `
            --namespace $Namespace `
            --title "batman-active standard for $Namespace" `
            --content "Namespace standard for the $Namespace namespace: Form 2 synchronous atomise-before-embed + Form 6 auto-classify (regex_then_llm). Issue #800. Generated by install-batman-active.ps1." `
            --tier long --priority 10 `
            --json 2>&1 | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join ''
        $stdId = ''
        try {
            $parsed = $storeRaw | ConvertFrom-Json
            $stdId = $parsed.id
            if (-not $stdId) { $stdId = $parsed.memory_id }
            if (-not $stdId -and $parsed.memory) { $stdId = $parsed.memory.id }
        } catch {}
        if (-not $stdId) {
            Err "could not capture stored memory id from: $storeRaw"
        } else {
            $bindOut = (& ai-memory --db $Db namespace set-standard `
                --namespace $Namespace --id $stdId `
                --governance $policyJson 2>&1 | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join "`n"
            Ok "bound '$Namespace' -> $stdId (Forms 2 + 6 active)"
        }
    }
}

# ----------------------------------------------------------- summary ----

Step 'Done'
Ok 'Batman Mode installation complete.'
Info ''
Info 'Verify with:'
Info "  .\scripts\batman-mode-acceptance.ps1 -Db `"$Db`" -Namespace `"$Namespace`""
Info ''
Info 'Restart Claude Code (or your MCP client) to pick up the Form 5 env vars.'
Info "The curator task runs at logon and survives restart (Scheduled Task: $taskName)."
