# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#Requires -Version 5.1
<#
.SYNOPSIS
    Install ai-memory for Windows.
.DESCRIPTION
    Downloads the latest ai-memory release, extracts it to ~/.local/bin,
    adds it to the user PATH, and verifies the installation.
.EXAMPLE
    irm https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.ps1 | iex
#>

$ErrorActionPreference = 'Stop'

$Repo = "alphaonedev/ai-memory-mcp"
$Binary = "ai-memory.exe"
$InstallDir = if ($env:AI_MEMORY_INSTALL_DIR) { $env:AI_MEMORY_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".local\bin" }

# Detect architecture
$Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($Arch) {
    'X64'   { $archLabel = "x86_64" }
    'Arm64' { $archLabel = "x86_64"; Write-Host "Note: ARM64 detected, using x86_64 build (only Windows target available)." -ForegroundColor Yellow }
    default { Write-Host "Unsupported architecture: $Arch" -ForegroundColor Red; exit 1 }
}

$Target = "${archLabel}-pc-windows-msvc"
$Asset = "ai-memory-${Target}.zip"
$ReleaseUrl = "https://github.com/${Repo}/releases/latest/download/${Asset}"

Write-Host "Detected platform: $Target"
Write-Host "Installing to: $InstallDir"
Write-Host ""

# Create install directory
if (-not (Test-Path $InstallDir)) {
    try {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        Write-Host "Created directory: $InstallDir"
    } catch {
        Write-Host "Error: Could not create directory '$InstallDir'. Check permissions." -ForegroundColor Red
        exit 1
    }
}

# Download to temp file
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ai-memory-install-" + [System.Guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null
$ZipPath = Join-Path $TempDir $Asset

try {
    Write-Host "Downloading $Asset..."
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $wc = New-Object System.Net.WebClient
        $wc.DownloadFile($ReleaseUrl, $ZipPath)
    } catch [System.Net.WebException] {
        $status = $_.Exception.Response.StatusCode.value__
        if ($status -eq 404) {
            Write-Host ""
            Write-Host "Error: Release not found (404)." -ForegroundColor Red
            Write-Host "The pre-built binary may not be available yet." -ForegroundColor Yellow
            Write-Host "Install from source instead:" -ForegroundColor Yellow
            Write-Host "  cargo install ai-memory" -ForegroundColor Cyan
            exit 1
        }
        throw
    }

    # Validate download (file size check)
    $FileSize = (Get-Item $ZipPath).Length
    if ($FileSize -lt 1024) {
        Write-Host "Error: Downloaded file is too small ($FileSize bytes). The download may be corrupt." -ForegroundColor Red
        Write-Host "Try installing from source instead:" -ForegroundColor Yellow
        Write-Host "  cargo install ai-memory" -ForegroundColor Cyan
        exit 1
    }
    Write-Host "Downloaded $([math]::Round($FileSize / 1MB, 1)) MB"

    # Extract
    Write-Host "Extracting..."
    $ExtractDir = Join-Path $TempDir "extracted"
    Expand-Archive -Path $ZipPath -DestinationPath $ExtractDir -Force

    # Find and copy binary
    $BinarySrc = Get-ChildItem -Path $ExtractDir -Filter $Binary -Recurse | Select-Object -First 1
    if (-not $BinarySrc) {
        Write-Host "Error: Could not find $Binary in the archive." -ForegroundColor Red
        exit 1
    }

    try {
        Copy-Item -Path $BinarySrc.FullName -Destination (Join-Path $InstallDir $Binary) -Force
    } catch {
        Write-Host "Error: Could not copy binary to '$InstallDir'. Check permissions." -ForegroundColor Red
        Write-Host "You may need to close any running ai-memory processes first." -ForegroundColor Yellow
        exit 1
    }

    Write-Host ""
    Write-Host "Installed $Binary to $InstallDir" -ForegroundColor Green

    # Add to user PATH if not already present
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -split ";" -notcontains $InstallDir) {
        try {
            [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
            $env:Path = "$env:Path;$InstallDir"
            Write-Host "Added $InstallDir to user PATH."
            Write-Host "Note: Restart your terminal for PATH changes to take effect in new sessions." -ForegroundColor Yellow
        } catch {
            Write-Host "Warning: Could not update PATH automatically." -ForegroundColor Yellow
            Write-Host "Add this directory to your PATH manually: $InstallDir" -ForegroundColor Yellow
        }
    } else {
        Write-Host "$InstallDir is already in PATH."
    }

    # Verify installation
    Write-Host ""
    $BinaryPath = Join-Path $InstallDir $Binary
    try {
        $output = & $BinaryPath stats 2>&1
        if ($LASTEXITCODE -eq 0) {
            Write-Host "Verification passed: ai-memory is working." -ForegroundColor Green
        } else {
            Write-Host "Warning: ai-memory exited with code $LASTEXITCODE (this is OK for first run)." -ForegroundColor Yellow
        }
    } catch {
        Write-Host "Warning: Could not verify installation. The binary may still work." -ForegroundColor Yellow
    }

    # Success message
    Write-Host ""
    Write-Host "========================================" -ForegroundColor Cyan
    Write-Host "  ai-memory installed successfully!" -ForegroundColor Cyan
    Write-Host "========================================" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "Next steps:"
    Write-Host "  1. Restart your terminal (for PATH update)"
    Write-Host "  2. Run:  ai-memory --help"
    Write-Host "  3. Configure your AI client's MCP settings:"
    Write-Host ""
    Write-Host '     {' -ForegroundColor Gray
    Write-Host '       "mcpServers": {' -ForegroundColor Gray
    Write-Host '         "memory": {' -ForegroundColor Gray
    Write-Host '           "command": "ai-memory",' -ForegroundColor Gray
    Write-Host '           "args": ["mcp", "--tier", "smart"]' -ForegroundColor Gray
    Write-Host '         }' -ForegroundColor Gray
    Write-Host '       }' -ForegroundColor Gray
    Write-Host '     }' -ForegroundColor Gray
    Write-Host ""

} finally {
    # Cleanup temp directory
    Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}
