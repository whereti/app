# Try a change without the installer.
#
#   scripts\dev.ps1            build, then run the app from target\release
#   scripts\dev.ps1 -AppOnly   the same, never touching the service
#
# The app runs straight from target\release, against the installed service.
# When the service, the extractor or wt changed (they differ from the copies
# in Program Files), they are swapped in place with ONE administrator prompt
# (scripts\dev-swap.ps1): the service is stopped, the files replaced, the
# service started again. The app is copied there too, so a restart of
# Windows starts the same build.
#
# The installer (scripts\package.ps1) stays for the end of a stage and for
# testing the update itself.

param([switch]$AppOnly)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$target = Join-Path $root "target\release"
$installed = "C:\Program Files\whereti"

Push-Location $root
try {
    cargo build --release -p whereti-app -p whereti-service -p whereti-extract -p whereti-cli
    if ($LASTEXITCODE -ne 0) { throw "the build failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }

# the app first: it holds target\release\whereti.exe and the installed one
Get-Process whereti -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300

function Same([string]$name) {
    $a = Join-Path $target $name; $b = Join-Path $installed $name
    (Test-Path $b) -and ((Get-FileHash $a).Hash -eq (Get-FileHash $b).Hash)
}
$changed = @("whereti-service.exe", "whereti-extract.exe", "wt.exe") | Where-Object { -not (Same $_) }

if ($changed -and -not $AppOnly) {
    if (-not (Test-Path $installed)) { throw "whereti is not installed: run the installer once first" }
    "Changed: $($changed -join ', ') - swapping them in (one administrator prompt)"
    $log = Join-Path $env:TEMP "whereti-dev-swap.log"
    Remove-Item $log -ErrorAction SilentlyContinue
    $swap = Join-Path $PSScriptRoot "dev-swap.ps1"
    $argList = "-NoProfile -ExecutionPolicy Bypass -File `"$swap`" -From `"$target`" -To `"$installed`" -Log `"$log`""
    $p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList $argList
    if (Test-Path $log) { Get-Content $log }
    if ($p.ExitCode -ne 0) { throw "the swap failed (exit $($p.ExitCode))" }
} elseif ($changed) {
    "Not swapped (-AppOnly): $($changed -join ', ')"
} else {
    "Service unchanged: no administrator prompt"
}

Start-Process (Join-Path $target "whereti.exe")
"whereti started from $target"
