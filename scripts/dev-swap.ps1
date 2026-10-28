# Run by scripts\dev.ps1, as administrator: puts a development build of the
# programs in place of the installed ones. Files Windows still holds open
# (an extractor that has not exited yet) are moved aside first and deleted
# at the next swap (or by the next install).

param([string]$From, [string]$To, [string]$Log)
$ErrorActionPreference = "Stop"
function Say([string]$m) { Add-Content -Path $Log -Value $m -Encoding utf8 }

try {
    Stop-Service whereti -ErrorAction SilentlyContinue
    (Get-Service whereti).WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    Get-Process whereti-extract, whereti, wt -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($To, [StringComparison]::OrdinalIgnoreCase) } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    # the copies moved aside by the last swap
    Get-ChildItem $To -Filter "*.old*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue

    foreach ($name in "whereti-service.exe", "whereti-extract.exe", "wt.exe", "whereti.exe", "onnxruntime.dll") {
        # onnxruntime.dll is in target\release only once package.ps1 put it there
        if (-not (Test-Path (Join-Path $From $name))) { Say "  $name is not in $From, kept"; continue }
        $dst = Join-Path $To $name
        try {
            Copy-Item (Join-Path $From $name) $dst -Force
        } catch {
            # still open: a running program can be renamed, not overwritten
            $aside = "$dst.old-$([DateTime]::Now.Ticks)"
            Move-Item $dst $aside -Force
            Copy-Item (Join-Path $From $name) $dst -Force
            Say "  $name was in use: the old copy is $aside"
        }
        Say "  replaced $name"
    }

    Start-Service whereti
    (Get-Service whereti).WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
    Say "service running again"
    exit 0
} catch {
    Say "error: $($_.Exception.Message)"
    # never leave whereti without its service
    Start-Service whereti -ErrorAction SilentlyContinue
    exit 1
}
