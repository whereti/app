# Builds a set of real searches for `wt rank-eval` (PLAN.md §51) from the
# files and folders this user actually opened: the shortcuts Windows keeps in
# Recent (Explorer, Office, file dialogs, most apps) and Office's own list.
#
# For each one: the first word of its name, and its first two words, as a
# person would type them, with the file as the expected result. The set holds
# your own paths: it is written next to whereti's settings, never into the
# repository.
#
#   powershell -ExecutionPolicy Bypass -File scripts\rank-set.ps1
#   wt rank-eval

param(
    [string]$Out = (Join-Path $env:LOCALAPPDATA "whereti\rank-set.tsv"),
    [int]$Max = 400
)

$ErrorActionPreference = "Stop"
$shell = New-Object -ComObject WScript.Shell
$folders = @(
    (Join-Path $env:APPDATA "Microsoft\Windows\Recent"),
    (Join-Path $env:APPDATA "Microsoft\Office\Recent")
)
$links = foreach ($f in $folders) {
    if (Test-Path $f) { Get-ChildItem $f -Filter *.lnk -File }
}
$links = $links | Sort-Object LastWriteTime -Descending | Select-Object -First $Max

$seen = @{}
$lines = New-Object System.Collections.Generic.List[string]
foreach ($l in $links) {
    try { $target = $shell.CreateShortcut($l.FullName).TargetPath } catch { continue }
    if (-not $target -or -not (Test-Path -LiteralPath $target)) { continue }
    $name = Split-Path $target -Leaf
    $stem = if ((Get-Item -LiteralPath $target) -is [System.IO.DirectoryInfo]) { $name } else { [System.IO.Path]::GetFileNameWithoutExtension($name) }
    $words = [regex]::Split($stem, '[^\p{L}\p{N}]+') | Where-Object { $_.Length -ge 3 -and $_ -notmatch '^\d+$' }
    $words = @($words)
    if ($words.Count -eq 0) { continue }
    $opened = [int64]([DateTimeOffset]$l.LastWriteTimeUtc).ToUnixTimeSeconds()
    $queries = @($words[0].ToLowerInvariant())
    if ($words.Count -ge 2) { $queries += ($words[0] + " " + $words[1]).ToLowerInvariant() }
    foreach ($q in $queries) {
        $key = "$q`t$target".ToLowerInvariant()
        if ($seen.ContainsKey($key)) { continue }
        $seen[$key] = $true
        $lines.Add("$q`t$target`t$opened")
    }
}

New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
$header = "# query`texpected path`topened (unix seconds). Add your own lines; the third column is optional."
[System.IO.File]::WriteAllLines($Out, [string[]](@($header) + $lines), (New-Object System.Text.UTF8Encoding $false))
"$($lines.Count) searches from $($links.Count) recent items -> $Out"
