# Builds the whereti installer: dist\whereti-setup-<version>.exe
#
#   .\scripts\package.ps1             development build: nothing is signed,
#                                     no token is needed
#   .\scripts\package.ps1 -Release    production: from a clean, tagged tree,
#                                     every program signed with the Certum
#                                     certificate (scripts\sign-windows.ps1)
#   .\scripts\package.ps1 -Release -Beta
#                                     the same, for testers (0.9.x): signed
#                                     and updatable, but the supporter key and
#                                     checkout links may still be the
#                                     development ones; no winget manifest
#
# The installer carries the four programs inside it (packed when it is
# compiled). So in production the order matters: the programs are signed
# FIRST, then packed into the installer, which is signed last. Signing the
# installer alone would install unsigned programs, and SmartScreen and
# antivirus judge each of them.

param(
    [switch]$Release,
    [switch]$Beta
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

# cargo and git write progress to stderr; Windows PowerShell 5 would stop on
# it. Native programs are judged by their exit code only.
function Invoke-Native([string]$What, [scriptblock]$Command) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try { & $Command 2>&1 | ForEach-Object { "$_" } | Out-Host } finally { $ErrorActionPreference = $saved }
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}
Set-Location $repoRoot

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$dist = Join-Path $repoRoot "dist"
$payload = Join-Path $dist "payload"
$programs = @("whereti.exe", "whereti-service.exe", "whereti-extract.exe", "wt.exe")

if ($Release) {
    # A signed installer must answer for a revision anyone can check out.
    $dirty = git status --porcelain
    if ($dirty) { throw "The working tree has uncommitted changes. Commit them first: a signed release is built from a commit." }
    $tag = & { $ErrorActionPreference = "Continue"; git describe --exact-match --tags HEAD 2>$null }
    if (-not $tag) { throw "HEAD is not tagged. Tag the release (e.g. git tag v$version) and build from the tag." }
    if ($tag -ne "v$version") { throw "The tag $tag does not match the version $version in Cargo.toml." }
    # The supporter keys: a release must carry the production public key and
    # real checkout links, never the development ones (license.rs, links.rs).
    if ($Beta) {
        Write-Host "Beta: the supporter key and checkout links are not checked (not for 1.0)." -ForegroundColor Yellow
    } else {
    $license = Get-Content (Join-Path $repoRoot "crates\whereti-app\src\license.rs") -Raw
    $public = [regex]::Match($license, 'pub const PUBLIC_KEY_HEX: &str = "([0-9a-f]{64})"').Groups[1].Value
    $dev = [regex]::Match($license, 'pub const DEV_PUBLIC_KEY_HEX: &str = "([0-9a-f]{64})"').Groups[1].Value
    if (-not $public -or $public -eq $dev) { throw "license.rs still has the development public key. Run whereti.licenses/scripts/new-key.mjs and paste the production key." }
    $links = Get-Content (Join-Path $repoRoot "crates\whereti-app\src\links.rs") -Raw
    if ($links -match 'BUY_(PERSONAL|BUSINESS): &str = "https://whereti\.com/supporter/#') { throw "links.rs still has placeholder checkout links. Paste the Lemon Squeezy links." }
    }    # Found now, not after ten minutes of compiling.
    $thumbprint = if ($env:WHERETI_SIGN_THUMBPRINT) { $env:WHERETI_SIGN_THUMBPRINT } else { "5DF397C236D87DA3DB0137E9CBFDF29319241E4A" }
    if (-not (Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Thumbprint -eq $thumbprint })) {
        throw "The Authenticode certificate $thumbprint is not known to this machine. Plug the token in once with proCertum CardManager closed so Windows registers it."
    }
}

Write-Host "`n== Programs ==" -ForegroundColor Cyan
Invoke-Native "building the programs" { cargo build --release --locked -p whereti-app -p whereti-service -p whereti-extract -p whereti-cli }

# ONNX Runtime for recognising what is in photos (PLAN.md §55): Microsoft's
# own build, from its GitHub release, used only if the DLL's SHA-256 and its
# Microsoft signature match. Fetched once, kept in target\onnxruntime.
$ortVersion = "1.31.0"
$ortSha256 = "0C42DC4E278F0968FDF9EBF1B1E54A74C657A9942E70803E453C91993451CF4C"
$ortDir = Join-Path $repoRoot "target\onnxruntime-$ortVersion"
$ortDll = Join-Path $ortDir "onnxruntime.dll"
if (-not (Test-Path $ortDll)) {
    Write-Host "`n== ONNX Runtime $ortVersion ==" -ForegroundColor Cyan
    New-Item -ItemType Directory -Force $ortDir | Out-Null
    $zip = Join-Path $ortDir "onnxruntime.zip"
    Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/microsoft/onnxruntime/releases/download/v$ortVersion/onnxruntime-win-x64-$ortVersion.zip" -OutFile $zip
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $entry = $archive.Entries | Where-Object { $_.FullName -like "*/lib/onnxruntime.dll" } | Select-Object -First 1
        if (-not $entry) { throw "onnxruntime.dll is not in the ONNX Runtime archive" }
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $ortDll, $true)
    } finally {
        $archive.Dispose()
    }
    Remove-Item $zip
}
if ((Get-FileHash $ortDll -Algorithm SHA256).Hash -ne $ortSha256) { Remove-Item $ortDll; throw "onnxruntime.dll does not have the expected SHA-256" }
$ortSig = Get-AuthenticodeSignature $ortDll
if ($ortSig.Status -ne "Valid" -or $ortSig.SignerCertificate.Subject -notmatch "^CN=Microsoft Corporation,") { throw "onnxruntime.dll is not signed by Microsoft" }
# beside the programs for local runs too (whereti-extract loads it from there)
Copy-Item $ortDll (Join-Path $repoRoot "target\release\onnxruntime.dll") -Force

# emptied rather than deleted: an Explorer window open on dist keeps the folder itself busy
if (Test-Path $dist) { Get-ChildItem $dist -Force | Remove-Item -Recurse -Force }
New-Item -ItemType Directory -Force $payload | Out-Null
foreach ($p in $programs) {
    Copy-Item (Join-Path $repoRoot "target\release\$p") (Join-Path $payload $p)
}
# signed by Microsoft already: copied, never re-signed
Copy-Item $ortDll (Join-Path $payload "onnxruntime.dll")

if ($Release) {
    Write-Host "`n== Signing the programs ==" -ForegroundColor Cyan
    Read-Host "Plug in the signing token and press Enter" | Out-Null
    foreach ($p in $programs) {
        & "$PSScriptRoot\sign-windows.ps1" -Path (Join-Path $payload $p)
    }
}

Write-Host "`n== Installer ==" -ForegroundColor Cyan
$env:WHERETI_PAYLOAD = $payload
try {
    Invoke-Native "building the installer" { cargo build --release --locked -p whereti-setup }
} finally {
    Remove-Item Env:\WHERETI_PAYLOAD -ErrorAction SilentlyContinue
}
$setup = Join-Path $dist "whereti-setup-$version.exe"
Copy-Item (Join-Path $repoRoot "target\release\whereti-setup.exe") $setup

if ($Release) {
    & "$PSScriptRoot\sign-windows.ps1" -Path $setup

    # An unsigned file that looks finished is the failure this catches.
    Write-Host "`n== Signatures ==" -ForegroundColor Cyan
    $all = @($setup) + ($programs | ForEach-Object { Join-Path $payload $_ })
    foreach ($path in $all) {
        $signature = Get-AuthenticodeSignature $path
        if ($signature.Status -ne "Valid") {
            throw "$(Split-Path -Leaf $path) is not validly signed: $($signature.Status). $($signature.StatusMessage)"
        }
        Write-Host ("  {0,-30} {1}" -f (Split-Path -Leaf $path), $signature.SignerCertificate.Subject)
    }
} else {
    Write-Host "Development build: not signed (use -Release for a production installer)." -ForegroundColor Yellow
}

$hash = (Get-FileHash $setup -Algorithm SHA256).Hash
$size = [math]::Round((Get-Item $setup).Length / 1MB, 1)
Write-Host "`n$setup  ($size MB)" -ForegroundColor Green
Write-Host "SHA-256 $hash"

if ($Release) {
    # The manifest for releases.whereti.com (whereti.releases: npm run
    # publish:manifest). Publish it only after the GitHub release exists.
    $manifest = [ordered]@{
        version   = $version
        notes     = "See the release notes for v$version."
        pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
        platforms = [ordered]@{
            "windows-x86_64" = [ordered]@{
                url    = "https://github.com/whereti/app/releases/download/v$version/whereti-setup-$version.exe"
                sha256 = $hash.ToLowerInvariant()
                size   = (Get-Item $setup).Length
            }
        }
    }
    $json = $manifest | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText((Join-Path $dist "latest.json"), $json, (New-Object Text.UTF8Encoding $false))
    Write-Host "dist\latest.json written: publish it with whereti.releases after the GitHub release."
    if (-not $Beta) {
        & "$PSScriptRoot\winget.ps1" -Version $version -Sha256 $hash
    }
}