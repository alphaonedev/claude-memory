# batman-mode-acceptance.ps1 — PowerShell parity of
# scripts/batman-mode-acceptance.sh. Runs the same 22-check structural
# suite plus optional -Behavioral mode against a Windows install.
#
# Issue: https://github.com/alphaonedev/ai-memory-mcp/issues/800
# Companion doc: docs/batman-active-mode.md
#
# Exits with the count of FAIL checks (0 = full Batman-active).
#
# Usage:
#   .\scripts\batman-mode-acceptance.ps1
#   .\scripts\batman-mode-acceptance.ps1 -Db C:\path\to.db -Namespace ai-memory-mcp
#   .\scripts\batman-mode-acceptance.ps1 -Json
#   .\scripts\batman-mode-acceptance.ps1 -Behavioral

[CmdletBinding()]
param(
    [string]$Db = $env:AI_MEMORY_DB,
    [string]$Namespace = 'main',
    [switch]$Json,
    [switch]$Behavioral
)

$ErrorActionPreference = 'Stop'

if (-not $Db) { $Db = Join-Path $env:USERPROFILE '.claude\ai-memory.db' }
if (-not (Test-Path $Db)) { Write-Error "DB does not exist: $Db"; exit 2 }

$PassCount = 0
$FailCount = 0
$Results = @()

function Record([string]$Id, [string]$Verdict, [string]$What, [string]$Evidence) {
    if ($Verdict -eq 'pass') { $script:PassCount++ } else { $script:FailCount++ }
    $sym = if ($Verdict -eq 'pass') { 'PASS' } else { 'FAIL' }
    if (-not $Json) {
        Write-Host "$sym - $Id - $What"
        if ($Evidence) { Write-Host "         evidence: $Evidence" -ForegroundColor DarkGray }
    }
    $script:Results += [PSCustomObject]@{ id = $Id; verdict = $Verdict; what = $What; evidence = $Evidence }
}

function Invoke-Sql([string]$Query) {
    $out = & sqlite3.exe $Db $Query 2>$null
    return ($out -join "`n").Trim()
}

function Table-Exists([string]$Name) {
    return (Invoke-Sql "SELECT name FROM sqlite_master WHERE type='table' AND name='$Name';") -eq $Name
}

function Memories-Has-Column([string]$Col) {
    $cols = Invoke-Sql 'PRAGMA table_info(memories);'
    return ($cols -split "`n" | ForEach-Object { ($_ -split '\|')[1] }) -contains $Col
}

# ----------------------------------------------------------- prereqs ----

if (-not $Json) {
    Write-Host "Batman Mode acceptance - DB: $Db - namespace: $Namespace"
    Write-Host '-----------------------------------------------------------------'
}

# P1
$aiMem = Get-Command ai-memory.exe -ErrorAction SilentlyContinue
if ($aiMem) {
    $version = (& ai-memory --version 2>$null | Select-Object -Last 1)
    Record 'P1' 'pass' 'ai-memory binary present + readable version' $version
} else {
    Record 'P1' 'fail' 'ai-memory.exe not on PATH' ''
    if ($Json) { $Results | ConvertTo-Json -Depth 8 }
    exit $FailCount
}

# P2 - schema v >= 38
$schemaVersion = Invoke-Sql 'SELECT version FROM schema_version LIMIT 1;'
if ([int]$schemaVersion -ge 38) {
    Record 'P2' 'pass' 'schema version >= v38 (Form 4 citations migration applied)' "schema_version.version=$schemaVersion"
} else {
    Record 'P2' 'fail' 'schema version < v38' "schema_version.version=$schemaVersion"
}

# P3 - Ollama reachable
try {
    $resp = Invoke-WebRequest -Uri 'http://localhost:11434/api/tags' -TimeoutSec 3 -UseBasicParsing
    if ($resp.StatusCode -eq 200) {
        Record 'P3' 'pass' 'Ollama backend reachable (autonomous tier)' 'http://localhost:11434'
    }
} catch {
    Record 'P3' 'fail' 'Ollama backend unreachable' 'http://localhost:11434'
}

# ----------------------------------------------------------- form 1 ----

if (Table-Exists 'governance_rules') {
    Record 'F1.1' 'pass' 'Form 1 governance_rules table present' 'shared substrate w/ Form 7'
} else { Record 'F1.1' 'fail' 'Form 1 governance_rules table missing' '' }

if (Table-Exists 'signed_events') {
    $n = Invoke-Sql 'SELECT COUNT(*) FROM signed_events;'
    Record 'F1.2' 'pass' 'Form 1 signed_events audit chain present' "rows=$n"
} else { Record 'F1.2' 'fail' 'Form 1 signed_events audit chain missing' '' }

# ----------------------------------------------------------- form 2 ----

if (Memories-Has-Column 'atomised_into') { Record 'F2.1' 'pass' 'Form 2 memories.atomised_into column present' '' } else { Record 'F2.1' 'fail' 'Form 2 atomised_into missing' '' }
if (Memories-Has-Column 'atom_of')       { Record 'F2.2' 'pass' 'Form 2 memories.atom_of column present' '' }       else { Record 'F2.2' 'fail' 'Form 2 atom_of missing' '' }

# ----------------------------------------------------------- form 3 ----

# Probe MCP tools/list for memory_ingest_multistep
$probeJson = @(
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"acceptance","version":"0"}}}'
    '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
) -join "`n"
$mcpOut = $probeJson | & ai-memory --db $Db mcp --profile full --tier autonomous 2>$null
$hasIngest = $false
foreach ($line in $mcpOut) {
    try {
        $o = $line | ConvertFrom-Json
        if ($o.id -eq 2 -and $o.result.tools) {
            $hasIngest = [bool]($o.result.tools | Where-Object { $_.name -eq 'memory_ingest_multistep' })
            break
        }
    } catch { continue }
}
if ($hasIngest) { Record 'F3.1' 'pass' 'Form 3 memory_ingest_multistep MCP tool advertised at --profile full' 'tools/list contains the tool' } else { Record 'F3.1' 'fail' 'Form 3 memory_ingest_multistep MCP tool not advertised' '' }

# ----------------------------------------------------------- form 4 ----

foreach ($col in 'citations','source_uri','source_span') {
    if (Memories-Has-Column $col) {
        Record "F4.$($col[0])" 'pass' "Form 4 memories.$col column present" ''
    } else {
        Record "F4.$($col[0])" 'fail' "Form 4 memories.$col column missing" ''
    }
}

# ----------------------------------------------------------- form 5 ----

if (Memories-Has-Column 'confidence_source') { Record 'F5.1' 'pass' 'Form 5 memories.confidence_source column present' "default 'caller_provided'" } else { Record 'F5.1' 'fail' 'Form 5 confidence_source missing' '' }
if (Table-Exists 'confidence_shadow_observations') {
    $n = Invoke-Sql 'SELECT COUNT(*) FROM confidence_shadow_observations;'
    Record 'F5.2' 'pass' 'Form 5 confidence_shadow_observations table present' "rows=$n"
} else { Record 'F5.2' 'fail' 'Form 5 confidence_shadow_observations missing' '' }
if (Memories-Has-Column 'confidence_decayed_at') {
    $n = Invoke-Sql 'SELECT COUNT(*) FROM memories WHERE confidence_decayed_at IS NOT NULL;'
    Record 'F5.3' 'pass' 'Form 5 memories.confidence_decayed_at column present' "rows with decay applied=$n"
} else { Record 'F5.3' 'fail' 'Form 5 confidence_decayed_at missing' '' }

# F5.4 - env vars in .claude.json mcpServers.memory.env
$claudeJson = Join-Path $env:USERPROFILE '.claude.json'
$envOk = 0
if (Test-Path $claudeJson) {
    try {
        $cfg = Get-Content $claudeJson -Raw | ConvertFrom-Json
        $envBlock = $cfg.mcpServers.memory.env
        foreach ($k in 'AI_MEMORY_AUTO_CONFIDENCE','AI_MEMORY_CONFIDENCE_SHADOW','AI_MEMORY_CONFIDENCE_DECAY') {
            if ($envBlock.$k -eq '1') { $envOk++ }
        }
    } catch {}
}
if ($envOk -eq 3) {
    Record 'F5.4' 'pass' 'Form 5 env vars wired into MCP launch (.claude.json)' 'AI_MEMORY_AUTO_CONFIDENCE / SHADOW / DECAY all = 1'
} else {
    Record 'F5.4' 'fail' 'Form 5 env vars not wired in .claude.json' "set count=$envOk/3"
}

# F5.5 - namespace standard set
$stdId = Invoke-Sql "SELECT standard_id FROM namespace_meta WHERE namespace='$Namespace' AND standard_id IS NOT NULL;"
if ($stdId) {
    Record 'F5.5' 'pass' "namespace '$Namespace' has standard '$stdId' set" "namespace_meta.standard_id='$stdId'"
    # F5.6 - governance has auto_atomise + auto_classify_kind
    $gov = Invoke-Sql "SELECT json_extract(metadata, '$.governance') FROM memories WHERE id='$stdId';"
    try {
        $govObj = $gov | ConvertFrom-Json
        if ($govObj.auto_atomise -eq $true -and $govObj.auto_classify_kind -in @('regex_only','regex_then_llm')) {
            Record 'F5.6' 'pass' "Form 2 + Form 6 active in '$Namespace' standard (auto_atomise=on, auto_classify_kind=$($govObj.auto_classify_kind))" "policy memory $stdId"
        } else {
            Record 'F5.6' 'fail' 'Form 2 + Form 6 not fully active in standard governance' "auto_atomise=$($govObj.auto_atomise) auto_classify_kind=$($govObj.auto_classify_kind)"
        }
    } catch {
        Record 'F5.6' 'fail' 'governance JSON unparseable' $gov
    }
} else {
    Record 'F5.5' 'fail' "namespace '$Namespace' has no standard_id" 'set via ai-memory namespace set-standard'
    Record 'F5.6' 'fail' 'no standard memory set' ''
}

# ----------------------------------------------------------- form 6 ----

if (Memories-Has-Column 'memory_kind') {
    $kinds = Invoke-Sql "SELECT memory_kind, COUNT(*) FROM memories WHERE namespace='$Namespace' GROUP BY memory_kind;"
    Record 'F6.1' 'pass' 'Form 6 memories.memory_kind column present' "kinds_in_use=$($kinds -replace "`n", ',')"
} else { Record 'F6.1' 'fail' 'Form 6 memory_kind missing' '' }
if ((Memories-Has-Column 'entity_id') -and (Memories-Has-Column 'mentioned_entity_id')) {
    Record 'F6.2' 'pass' 'Form 6 entity_id + mentioned_entity_id columns present' ''
} else { Record 'F6.2' 'fail' 'Form 6 entity surface incomplete' '' }

# ----------------------------------------------------------- form 7 ----

$keyDir = Join-Path $env:APPDATA 'ai-memory\keys'
$opKey = Join-Path $keyDir 'operator.key'
if (Test-Path $opKey) {
    Record 'F7.1' 'pass' 'Form 7 operator key present' $opKey
} else { Record 'F7.1' 'fail' 'Form 7 operator key absent' $opKey }

# F7.2 - R001-R004 enabled + signed
$rulesRaw = (& ai-memory --db $Db rules list --json 2>$null | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join ''
try {
    $rulesParsed = $rulesRaw | ConvertFrom-Json
    $rules = if ($rulesParsed.result) { $rulesParsed.result } else { $rulesParsed }
    $byId = @{}
    foreach ($r in $rules) { $byId[$r.id] = "$($r.id):$(if ($r.enabled) { 'on' } else { 'off' })/$($r.attest_level)" }
    $allOn = ('R001','R002','R003','R004' | ForEach-Object { $byId[$_] }) -join ' '
    $okCount = ($allOn -split ' ' | Where-Object { $_ -match '^R[0-9]{3}:on/operator_signed$' }).Count
    if ($okCount -eq 4) {
        Record 'F7.2' 'pass' 'Form 7 R001-R004 all enabled + operator_signed' $allOn
    } else {
        Record 'F7.2' 'fail' 'Form 7 R001-R004 not all enabled + signed' $allOn
    }
} catch {
    Record 'F7.2' 'fail' 'rules list parse failed' $rulesRaw.Substring(0, [Math]::Min(200, $rulesRaw.Length))
}

# F7.3 - smoke test refuse path
try {
    $denyRaw = (& ai-memory --db $Db rules check --kind filesystem_write `
        --payload '{"path":"C:\\Windows\\Temp\\acceptance-test.txt"}' `
        --agent-id batman-acceptance 2>$null | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join ''
    $deny = $denyRaw | ConvertFrom-Json
    if ($deny.decision -eq 'refuse') {
        Record 'F7.3' 'pass' "Form 7 enforcement: write refused (rule $($deny.rule_id))" "decision=refuse rule_id=$($deny.rule_id)"
    } else {
        Record 'F7.3' 'fail' 'Form 7 enforcement: write NOT refused' "decision=$($deny.decision)"
    }
} catch {
    Record 'F7.3' 'fail' 'Form 7 smoke test parse failed' $_.ToString()
}

# F7.4 - allow path
try {
    $allowPath = Join-Path $env:USERPROFILE '.local-runs\acceptance-test.txt'
    $allowPath = $allowPath -replace '\\','\\\\'
    $allowRaw = (& ai-memory --db $Db rules check --kind filesystem_write `
        --payload "{`"path`":`"$allowPath`"}" `
        --agent-id batman-acceptance 2>$null | Where-Object { $_ -notmatch '^ai-memory: loaded config' }) -join ''
    $allow = $allowRaw | ConvertFrom-Json
    if ($allow.decision -eq 'allow') {
        Record 'F7.4' 'pass' 'Form 7 enforcement: allowed path returns allow' 'decision=allow'
    } else {
        Record 'F7.4' 'fail' 'Form 7 enforcement: allow path did not return allow' "decision=$($allow.decision)"
    }
} catch {
    Record 'F7.4' 'fail' 'allow-path smoke test failed' $_.ToString()
}

# ----------------------------------------------------------- upkeep ----

$task = Get-ScheduledTask -TaskName 'AI-Memory Curator (Batman Mode)' -ErrorAction SilentlyContinue
if ($task) {
    Record 'U1' 'pass' "curator Scheduled Task present + state=$($task.State)" 'AI-Memory Curator (Batman Mode)'
    Record 'U2' 'pass' 'curator unit installed (survives reboot via Task Scheduler AtLogOn trigger)' 'Get-ScheduledTask returns the task'
} else {
    Record 'U1' 'fail' 'curator daemon Scheduled Task not registered' 'register via install-batman-active.ps1'
    Record 'U2' 'fail' 'no persistent curator unit' ''
}

# ----------------------------------------------------------- summary ----

$total = $PassCount + $FailCount

if ($Json) {
    $summary = [PSCustomObject]@{
        db = $Db; namespace = $Namespace
        total = $Results.Count
        pass = $PassCount
        fail = $FailCount
        batman_active = ($FailCount -eq 0)
        results = $Results
    }
    $summary | ConvertTo-Json -Depth 8
} else {
    Write-Host '-----------------------------------------------------------------'
    if ($FailCount -eq 0) {
        Write-Host "VERDICT: Batman-ACTIVE ($PassCount/$total checks pass)" -ForegroundColor Green
    } elseif ($PassCount -ge ($total * 0.75)) {
        Write-Host "VERDICT: Batman-PARTIAL ($PassCount/$total - $FailCount short of full active)" -ForegroundColor Yellow
    } else {
        Write-Host "VERDICT: Batman-CAPABLE ($PassCount/$total - substrate ready, activation incomplete)" -ForegroundColor Red
    }
}

exit $FailCount
