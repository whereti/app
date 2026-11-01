# Builds the whereti installer: dist\whereti-setup-<version>.exe
#
#   .\scripts\package.ps1             development build: nothing is signed,
#                                     no token is needed; it goes to
#                                     dist\dev\whereti-setup-<version>-dev.exe
#                                     and never touches a signed one in dist
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
    # from the repository (rust-toolchain.toml), and back where the caller was
    Push-Location $repoRoot
    try { & $Command 2>&1 | ForEach-Object { "$_" } | Out-Host } finally { $ErrorActionPreference = $saved; Pop-Location }
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}

$version = (Select-String -Path (Join-Path $repoRoot "Cargo.toml") -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
# a development build has a folder of its own: dist keeps the signed release
$dist = if ($Release) { Join-Path $repoRoot "dist" } else { Join-Path $repoRoot "dist\dev" }
$payload = Join-Path $dist "payload"
$programs = @("whereti.exe", "whereti-service.exe", "whereti-extract.exe", "wt.exe")

if ($Release) {
    # A signed installer must answer for a revision anyone can check out.
    $dirty = git -C $repoRoot status --porcelain
    if ($dirty) { throw "The working tree has uncommitted changes. Commit them first: a signed release is built from a commit." }
    $tag = & { $ErrorActionPreference = "Continue"; git -C $repoRoot describe --exact-match --tags HEAD 2>$null }
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
    # Business goes to the website on purpose (the PC count is chosen there)
    if ($links -notmatch 'BUY_PERSONAL: &str = "https://whereti\.lemonsqueezy\.com/checkout/buy/') { throw "links.rs has no Lemon Squeezy checkout for Personal. Paste the link." }
    }    # Found now, not after ten minutes of compiling.
    $thumbprint = if ($env:WHERETI_SIGN_THUMBPRINT) { $env:WHERETI_SIGN_THUMBPRINT } else { "5DF397C236D87DA3DB0137E9CBFDF29319241E4A" }
    if (-not (Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Thumbprint -eq $thumbprint })) {
        throw "The Authenticode certificate $thumbprint is not known to this machine. Plug the token in once with proCertum CardManager closed so Windows registers it."
    }
}

# Paths of the PC that builds (the user folder with cargo's sources, the
# repository) end up in the programs through panic messages and generated
# code. They are renamed, so a published file says nothing about that PC, and
# every file is checked for them before it is signed.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
# This replaces the rustflags of .cargo\config.toml, so the C runtime is
# asked to be built in here too (see there).
$env:CARGO_ENCODED_RUSTFLAGS = @("-Ctarget-feature=+crt-static", "--remap-path-prefix=$cargoHome=cargo", "--remap-path-prefix=$repoRoot=whereti") -join [char]0x1f

# The DLLs a program or DLL loads at start (its import table).
function Get-DllImports([string]$File) {
    $b = [IO.File]::ReadAllBytes($File)
    $pe = [BitConverter]::ToInt32($b, 0x3C)
    $sections = [BitConverter]::ToUInt16($b, $pe + 6)
    $optional = $pe + 24
    $optionalSize = [BitConverter]::ToUInt16($b, $pe + 20)
    # data directory 1 (imports); PE32+ has it at 120, PE32 at 104
    $dirs = $optional + $(if ([BitConverter]::ToUInt16($b, $optional) -eq 0x20b) { 112 } else { 96 })
    $importRva = [BitConverter]::ToUInt32($b, $dirs + 8)
    $table = $optional + $optionalSize
    $toOffset = {
        param([uint32]$rva)
        for ($i = 0; $i -lt $sections; $i++) {
            $s = $table + 40 * $i
            $va = [BitConverter]::ToUInt32($b, $s + 12)
            $size = [Math]::Max([BitConverter]::ToUInt32($b, $s + 8), [BitConverter]::ToUInt32($b, $s + 16))
            if ($rva -ge $va -and $rva -lt $va + $size) { return $rva - $va + [BitConverter]::ToUInt32($b, $s + 20) }
        }
        throw "$File is not a Windows program this script can read"
    }
    if ($importRva -eq 0) { return @() }
    $names = @()
    for ($d = & $toOffset $importRva; [BitConverter]::ToUInt32($b, $d + 12) -ne 0; $d += 20) {
        $n = & $toOffset ([BitConverter]::ToUInt32($b, $d + 12))
        $end = [Array]::IndexOf($b, [byte]0, [int]$n)
        $names += [Text.Encoding]::ASCII.GetString($b, $n, $end - $n)
    }
    $names
}
# Our programs carry the C runtime (see .cargo\config.toml): one that needs
# the Visual C++ runtime would not start on a PC without it.
function Assert-NoVcRuntime([string]$File) {
    $needs = Get-DllImports $File | Where-Object { $_ -match '^(vcruntime|msvcp|concrt)\d' }
    if ($needs) { throw "$File needs $($needs -join ', '): it would not start on a Windows without the Visual C++ runtime." }
}

function Assert-NoLocalPaths([string]$File) {
    $text = [IO.File]::ReadAllText($File, [Text.Encoding]::GetEncoding(28591))
    foreach ($local in @($env:USERPROFILE, $repoRoot)) {
        foreach ($form in @($local, $local.Replace("\", "/"))) {
            if ($text.IndexOf($form, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                throw "$File contains the local path $form. It would be published with the release."
            }
        }
    }
}

Write-Host "`n== Programs ==" -ForegroundColor Cyan
Invoke-Native "building the programs" { cargo build --release --locked -p whereti-app -p whereti-service -p whereti-extract -p whereti-cli }
foreach ($p in $programs) {
    Assert-NoLocalPaths (Join-Path $repoRoot "target\release\$p")
    Assert-NoVcRuntime (Join-Path $repoRoot "target\release\$p")
}

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

# onnxruntime.dll is built with the Visual C++ runtime as DLLs, which a
# clean Windows does not have. Microsoft lets a program carry them in its own
# folder: these four (what onnxruntime.dll imports) come from the Visual
# Studio that builds, signed by Microsoft, and go beside it. Windows looks
# for a DLL's DLLs in the program's folder first, so they are found there.
$vcDlls = @("vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "msvcp140_1.dll")
$ortNeeds = Get-DllImports $ortDll | Where-Object { $_ -match '^(vcruntime|msvcp|concrt)\d' } | ForEach-Object { $_.ToLowerInvariant() }
$missing = $ortNeeds | Where-Object { $vcDlls -notcontains $_ }
if ($missing) { throw "onnxruntime.dll also needs $($missing -join ', '): add it to `$vcDlls." }
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcRedist = Get-ChildItem (Join-Path $vsPath "VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT") -Directory -ErrorAction SilentlyContinue |
    Sort-Object { [version]$_.Parent.Parent.Name } -ErrorAction SilentlyContinue | Select-Object -Last 1
if (-not $vcRedist) { throw "The Visual C++ runtime DLLs were not found under $vsPath\VC\Redist\MSVC." }
foreach ($d in $vcDlls) {
    $path = Join-Path $vcRedist.FullName $d
    $sig = Get-AuthenticodeSignature $path
    if ($sig.Status -ne "Valid" -or $sig.SignerCertificate.Subject -notmatch "O=Microsoft Corporation,") { throw "$path is not signed by Microsoft" }
    Copy-Item $path (Join-Path $repoRoot "target\release\$d") -Force
}
Write-Host "Visual C++ runtime for onnxruntime.dll: $($vcRedist.FullName)"

# emptied rather than deleted: an Explorer window open on dist keeps the folder itself busy
if (Test-Path $dist) { Get-ChildItem $dist -Force | Remove-Item -Recurse -Force }
New-Item -ItemType Directory -Force $payload | Out-Null
foreach ($p in $programs) {
    Copy-Item (Join-Path $repoRoot "target\release\$p") (Join-Path $payload $p)
}
# signed by Microsoft already: copied, never re-signed
Copy-Item $ortDll (Join-Path $payload "onnxruntime.dll")
foreach ($d in $vcDlls) { Copy-Item (Join-Path $repoRoot "target\release\$d") (Join-Path $payload $d) }

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
$setup = Join-Path $dist $(if ($Release) { "whereti-setup-$version.exe" } else { "whereti-setup-$version-dev.exe" })
Copy-Item (Join-Path $repoRoot "target\release\whereti-setup.exe") $setup
Assert-NoLocalPaths $setup
Assert-NoVcRuntime $setup
# What the installer will install is what is in the payload folder (the
# signed programs in a release), byte for byte: it unpacks and compares them.
$check = Start-Process -FilePath $setup -ArgumentList "--check-payload `"$payload`"" -Wait -PassThru
if ($check.ExitCode -ne 0) { throw "The installer does not carry the programs in $payload (exit $($check.ExitCode))." }

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