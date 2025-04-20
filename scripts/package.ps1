# Builds the whereti installer: dist\whereti-setup-<version>.exe
#
#   .\scripts\package.ps1             development build: nothing is signed,
#                                     no token is needed
#   .\scripts\package.ps1 -Release    production: from a clean, tagged tree,
#                                     every program signed with the Certum
#                                     certificate (scripts\sign-windows.ps1)
#
# The installer carries the four programs inside it (packed when it is
# compiled). So in production the order matters: the programs are signed
# FIRST, then packed into the installer, which is signed last. Signing the
# installer alone would install unsigned programs, and SmartScreen and
# antivirus judge each of them.

param(
    [switch]$Release
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
    # Found now, not after ten minutes of compiling.
    $thumbprint = if ($env:WHERETI_SIGN_THUMBPRINT) { $env:WHERETI_SIGN_THUMBPRINT } else { "5DF397C236D87DA3DB0137E9CBFDF29319241E4A" }
    if (-not (Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Thumbprint -eq $thumbprint })) {
        throw "The Authenticode certificate $thumbprint is not known to this machine. Plug the token in once with proCertum CardManager closed so Windows registers it."
    }
}

Write-Host "`n== Programs ==" -ForegroundColor Cyan
Invoke-Native "building the programs" { cargo build --release --locked -p whereti-app -p whereti-service -p whereti-extract -p whereti-cli }

if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory -Force $payload | Out-Null
foreach ($p in $programs) {
    Copy-Item (Join-Path $repoRoot "target\release\$p") (Join-Path $payload $p)
}

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
