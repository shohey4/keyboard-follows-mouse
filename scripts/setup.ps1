<#
.SYNOPSIS
keyboard-follows-mouse: Windows setup check. Read-only apart from building into .\target.

.DESCRIPTION
  powershell -ExecutionPolicy Bypass -File scripts\setup.ps1
  powershell -ExecutionPolicy Bypass -File scripts\setup.ps1 -Help

Checks the toolchain, runs `cargo build --release`, then `kfm-config doctor`. It never installs
anything (no Rust, no Visual Studio Build Tools), and never changes the firewall, permissions,
startup items or your settings. Works from a folder whose path has spaces and from a ZIP download
(no .git needed). Windows PowerShell 5.1 or newer.
Exit status: 0 = ready (doctor found no problem), 1 = needs attention (a prerequisite is missing
or doctor reported problems), 2 = the build failed.
#>
param([switch]$Help)

# Native tools (cargo) write progress to stderr; check $LASTEXITCODE instead of letting that stop the script.
$ErrorActionPreference = 'Continue'

if ($Help -or ($args -contains '--help') -or ($args -contains '-h')) {
    Get-Help -Full $MyInvocation.MyCommand.Path | Out-String | Write-Host
    exit 0
}

$Root = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path -LiteralPath (Join-Path $Root 'Cargo.toml'))) {
    Write-Host "not the repository folder: $Root (Cargo.toml missing)"
    exit 1
}

Write-Host 'keyboard-follows-mouse setup check'
Write-Host ''
Write-Host 'Status and limits: this is an experimental hobby project, provided as is. Only Mac -> Windows has run'
Write-Host 'on real hardware, and only in one session; Mac -> Mac real key injection and the Windows -> Mac'
Write-Host 'source mode have never run on hardware (that mode captures EVERY keyboard on the PC while forwarding),'
Write-Host "and Windows -> Windows is not a target. See the status table in README.md ('Status - read this first')"
Write-Host 'before relying on it. Unofficial: not affiliated with Logitech.'
Write-Host ''
Write-Host "This script only checks and builds into: $(Join-Path $Root 'target')"
Write-Host '(the build downloads Rust crates from crates.io; nothing else on this PC is changed)'
Write-Host ''

$arch = $env:PROCESSOR_ARCHITECTURE
Write-Host ("system: Windows {0} {1}" -f [System.Environment]::OSVersion.Version, $arch)
if ($env:OS -ne 'Windows_NT') {
    Write-Host 'This script is for Windows. On a Mac use: sh scripts/setup.sh'
    exit 1
}

# cargo may be installed but not on PATH in this session (rustup puts it in %USERPROFILE%\.cargo\bin).
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    $local = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (Test-Path -LiteralPath $local) {
        $env:PATH = (Join-Path $env:USERPROFILE '.cargo\bin') + ';' + $env:PATH
        Write-Host "note: using cargo from $(Join-Path $env:USERPROFILE '.cargo\bin') (not on PATH in this session)"
        $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    }
}
if (-not $cargo -or -not (Get-Command rustc -ErrorAction SilentlyContinue)) {
    Write-Host 'MISSING: Rust (cargo / rustc) is not installed. Nothing was installed by this script.'
    Write-Host '  next: install it yourself with rustup (https://rustup.rs, rustup-init.exe), then run this script again'
    Write-Host '  undo: rustup self uninstall'
    exit 1
}
$rv = (& rustc --version) 2>$null
Write-Host "rust: $rv"
if ($rv -match '^rustc 1\.(\d+)\.' -and [int]$Matches[1] -lt 85) {
    Write-Host 'TOO OLD: this project needs Rust 1.85 or newer (edition 2024).'
    Write-Host '  next: rustup update stable'
    exit 1
}

# The default Rust target on Windows (MSVC) needs the Visual Studio C++ build tools linker.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$hasMsvc = $false
if (Test-Path -LiteralPath $vswhere) {
    $found = & $vswhere -latest -products '*' -requires 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' -property installationPath 2>$null
    if ($found) { $hasMsvc = $true }
}
if ($hasMsvc) {
    Write-Host 'Visual Studio C++ build tools: found'
} else {
    Write-Host 'Visual Studio C++ build tools: NOT detected (the build may fail at the link step).'
    Write-Host "  if it does: install 'Build Tools for Visual Studio' with the 'Desktop development with C++' workload yourself, then run this again."
}
Write-Host ''

Write-Host 'building: cargo build --release'
Push-Location -LiteralPath $Root
try {
    & cargo build --release
    $buildExit = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($buildExit -ne 0) {
    Write-Host 'BUILD FAILED. Read the first error above; kfm-config doctor was not run.'
    Write-Host "  undo: nothing to undo (only $(Join-Path $Root 'target') was written; delete it to start clean)"
    exit 2
}
Write-Host ''

$kfm = Join-Path $Root 'target\release\kfm-config.exe'
if (-not (Test-Path -LiteralPath $kfm)) {
    Write-Host "build finished but $kfm is missing"
    exit 2
}
& $kfm doctor
$status = $LASTEXITCODE
Write-Host ''
if ($status -eq 0) {
    Write-Host "setup check: ready. Next: & '$kfm' show   (or ask your assistant to continue, see AGENTS.md)."
} else {
    Write-Host 'setup check: the build succeeded; doctor listed what still needs attention above (each item says what to do next and how to undo).'
    Write-Host "On a fresh download the first item is normally 'settings do not exist yet': create them with"
    Write-Host "  & '$kfm' init --role receiver      (or windows-source, experimental)"
}
exit $status
