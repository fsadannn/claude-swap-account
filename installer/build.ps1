# Builds the release exe and packs it into dist\claude-swap-setup-<version>.exe
# Needs: Rust, and NSIS (makensis on PATH or in "C:\Program Files (x86)\NSIS"). Install NSIS: winget install NSIS.NSIS
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (-not $version) { throw "version not found in Cargo.toml" }

$makensis = (Get-Command makensis -ErrorAction SilentlyContinue).Source
if (-not $makensis) {
  foreach ($p in "C:\Program Files (x86)\NSIS\makensis.exe", "C:\Program Files\NSIS\makensis.exe") { if (Test-Path $p) { $makensis = $p; break } }
}
if (-not $makensis) { throw "makensis not found. Install it: winget install NSIS.NSIS" }

cargo build -p claude-swap --release
if ($LASTEXITCODE) { throw "cargo build failed" }

New-Item -ItemType Directory -Force dist | Out-Null
& $makensis "/DVERSION=$version" "/DEXE=$root\target\release\claude-swap.exe" "$root\installer\claude-swap.nsi"
if ($LASTEXITCODE) { throw "makensis failed" }
Write-Host "OK -> dist\claude-swap-setup-$version.exe"
