# recon installer — Windows (PowerShell)
#
# Usage:  irm https://mcprecon.pages.dev/install.ps1 | iex
# Pinned: $env:VERSION = 'v0.1.0'; irm https://mcprecon.pages.dev/install.ps1 | iex
#
# Same contract as the POSIX installer:
#   1. Download the .zip + SHA256SUMS.txt
#   2. Verify the asset's SHA256 against the manifest (mandatory)
#   3. If cosign.exe is on PATH, verify the manifest's sigstore signature
#   4. Extract to $InstallDir
#
# Environment overrides:
#   $env:VERSION       pin version instead of latest
#   $env:INSTALL_DIR   target dir (default %LOCALAPPDATA%\recon\bin)
#   $env:CDN           alternate CDN base
#   $env:SKIP_COSIGN   set to 1 to skip cosign verification (SHA256 still enforced)

$ErrorActionPreference = "Stop"
# Force TLS 1.2 on older PowerShell — Invoke-WebRequest defaults vary by
# Windows version, and some enterprise hosts still advertise SSL3/TLS1.0.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13

$CDN = if ($env:CDN) { $env:CDN } else { "https://mcprecon.pages.dev" }
$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { "$env:LOCALAPPDATA\recon\bin" }

# Resolve version
$version = $env:VERSION
if (-not $version) {
    $version = (Invoke-RestMethod "$CDN/latest.json" -UseBasicParsing).version
    if (-not $version) {
        Write-Error "Could not determine latest version. Set `$env:VERSION=vX.Y.Z and re-run."
        exit 1
    }
}

$asset       = "recon-x86_64-pc-windows-msvc.zip"
$urlAsset    = "$CDN/releases/$version/$asset"
$urlSums     = "$CDN/releases/$version/SHA256SUMS.txt"
$urlSumsSig  = "$CDN/releases/$version/SHA256SUMS.txt.sig"
$urlSumsPem  = "$CDN/releases/$version/SHA256SUMS.txt.pem"

Write-Host "Installing recon $version..."
Write-Host "  From: $urlAsset"
Write-Host "  To:   $InstallDir\recon.exe"
Write-Host ""

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$tmpDir = Join-Path $env:TEMP "recon-install-$version-$([System.Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null

try {
    $tmpAsset = Join-Path $tmpDir $asset
    $tmpSums  = Join-Path $tmpDir 'SHA256SUMS.txt'

    Invoke-WebRequest -Uri $urlAsset -OutFile $tmpAsset -UseBasicParsing
    Invoke-WebRequest -Uri $urlSums  -OutFile $tmpSums  -UseBasicParsing

    # ── SHA256 verification (mandatory) ───────────────────────────────────────
    Write-Host "Verifying SHA256..."
    $expectedLine = (Get-Content $tmpSums) | Where-Object { $_ -match ("\s" + [regex]::Escape($asset) + "$") }
    if (-not $expectedLine) {
        Write-Error "Manifest SHA256SUMS.txt does not list $asset — aborting."
        exit 1
    }
    $expected = ($expectedLine -split '\s+')[0].ToLowerInvariant()
    $actual   = (Get-FileHash -Algorithm SHA256 $tmpAsset).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        Write-Error "SHA256 verification FAILED for ${asset}: expected $expected got $actual"
        exit 1
    }
    Write-Host "  ok: $asset matches published SHA256"

    # ── cosign verification (optional) ────────────────────────────────────────
    $skipCosign = $env:SKIP_COSIGN -eq '1'
    $cosign = if (-not $skipCosign) { Get-Command cosign -ErrorAction SilentlyContinue } else { $null }
    if ($cosign) {
        Write-Host "Verifying cosign signature..."
        $tmpSig = Join-Path $tmpDir 'SHA256SUMS.txt.sig'
        $tmpPem = Join-Path $tmpDir 'SHA256SUMS.txt.pem'
        try {
            Invoke-WebRequest -Uri $urlSumsSig -OutFile $tmpSig -UseBasicParsing
            Invoke-WebRequest -Uri $urlSumsPem -OutFile $tmpPem -UseBasicParsing
        } catch {
            Write-Host "  info: signature artifacts not yet published for this version; SHA256 gate still enforced"
            $tmpSig = $null
        }
        if ($tmpSig -and (Test-Path $tmpSig) -and (Test-Path $tmpPem)) {
            $cosignArgs = @(
                'verify-blob',
                '--certificate', $tmpPem,
                '--signature',   $tmpSig,
                '--certificate-identity-regexp', '^https://github\.com/bravo1goingdark/intel/\.github/workflows/release\.yml@.*',
                '--certificate-oidc-issuer',     'https://token.actions.githubusercontent.com',
                $tmpSums
            )
            & cosign @cosignArgs 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) {
                Write-Host "  ok: cosign signature valid (GitHub Actions OIDC)"
            } else {
                Write-Warning "  cosign signature verification FAILED — installer will proceed with SHA256 only"
            }
        }
    } else {
        Write-Host "  info: cosign not installed — SHA256 gate enforced; install cosign for full provenance check"
    }

    # ── Extract ───────────────────────────────────────────────────────────────
    Expand-Archive -Path $tmpAsset -DestinationPath $InstallDir -Force
} finally {
    Remove-Item -Recurse -Force -Path $tmpDir -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "Installed recon to $InstallDir\recon.exe"
Write-Host ""

# Add to user PATH if not already present.
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("PATH", "$userPath;$InstallDir", "User")
    $env:PATH += ";$InstallDir"
    Write-Host "Added $InstallDir to your PATH."
    Write-Host ""
}

Write-Host "Next steps:"
Write-Host ""
Write-Host "  recon login <your-api-key>     # get a key at mcprecon.pages.dev/login"
Write-Host "  cd your-project"
Write-Host "  recon init --mcp cc            # index + wire Claude Code"
Write-Host "  recon init --mcp cursor        # or Cursor"
Write-Host "  recon init --mcp windsurf      # or Windsurf"
Write-Host "  recon init --mcp oc            # or OpenCode"
Write-Host ""
