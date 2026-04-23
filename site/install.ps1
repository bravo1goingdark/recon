# recon installer for Windows (PowerShell)
# Usage: irm https://mcprecon.pages.dev/install.ps1 | iex
# Or with explicit version: $env:VERSION="v0.1.0"; irm ... | iex

$ErrorActionPreference = "Stop"

$CDN = "https://mcprecon.pages.dev"
$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { "$env:LOCALAPPDATA\recon\bin" }

# Resolve version
$version = $env:VERSION
if (-not $version) {
    $version = (Invoke-RestMethod "$CDN/latest.json").version
    if (-not $version) {
        Write-Error "Could not determine latest version. Set `$env:VERSION=vX.Y.Z and re-run."
        exit 1
    }
}

$url = "$CDN/releases/$version/recon-x86_64-pc-windows-msvc.zip"

Write-Host "Installing recon $version..."
Write-Host "  From: $url"
Write-Host "  To:   $InstallDir\recon.exe"
Write-Host ""

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$tmp = Join-Path $env:TEMP "recon-$version.zip"
Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
Expand-Archive -Path $tmp -DestinationPath $InstallDir -Force
Remove-Item $tmp

Write-Host "Installed recon to $InstallDir\recon.exe"
Write-Host ""

# Add to user PATH if not already present
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
