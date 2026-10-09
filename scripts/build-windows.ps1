param(
    [ValidateSet('check','test','build','clippy')][string]$Action = 'build',
    [switch]$Release,
    [string]$TargetDir
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root

# Ignore a stale system-wide compiler cache wrapper; it is not a project dependency.
$env:RUSTC_WRAPPER = ''
if ($TargetDir) { $env:CARGO_TARGET_DIR = $TargetDir }

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path -LiteralPath $vswhere) {
    $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($vs) {
        Import-Module (Join-Path $vs 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll')
        Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation -DevCmdArguments '-arch=x64 -host_arch=x64' | Out-Null
    }
}

$env:RUSTC_WRAPPER = ''
$cargoArgs = @($Action, '--locked')
if ($Release) { $cargoArgs += '--release' }
if ($Action -eq 'clippy') { $cargoArgs += @('--all-targets', '--', '-D', 'warnings') }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "cargo $Action failed ($LASTEXITCODE)" }

if ($Action -eq 'build') {
    $profile = if ($Release) { 'release' } else { 'debug' }
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $targetRoot "$profile\vhdxdock.exe") -Destination (Join-Path $dist 'VhdxDock.exe')
    Write-Host "Output: $dist\VhdxDock.exe"
}
