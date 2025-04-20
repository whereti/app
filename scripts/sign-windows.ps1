# Applies the company's Authenticode signature to one file.
#
# The single place that knows which certificate signs whereti and which
# timestamp server it uses (the same Certum code-signing certificate and
# setup as SilentSilo). scripts/package.ps1 calls it for each program
# before packing them into the installer, then for the installer itself.
#
# The certificate lives on a hardware token and cannot be exported, so this
# only works on a machine with the token plugged in and its middleware
# running. That is deliberate: signing takes a physical act. It is used for
# production releases only; development builds stay unsigned.
#
# Timestamping is not optional. Without it the signature dies with the
# certificate, and installers already in people's hands start reading as
# unsigned when it expires.

param(
    [Parameter(Mandatory = $true)]
    [string]$Path
)

$ErrorActionPreference = "Stop"

# By thumbprint, not by name: `/n` matches a substring, and this machine
# holds a second code-signing certificate. Update on renewal.
$thumbprint = if ($env:WHERETI_SIGN_THUMBPRINT) {
    $env:WHERETI_SIGN_THUMBPRINT
} else {
    "5DF397C236D87DA3DB0137E9CBFDF29319241E4A"
}
$timestampUrl = if ($env:WHERETI_TIMESTAMP_URL) { $env:WHERETI_TIMESTAMP_URL } else { "http://time.certum.pl" }

# signtool answers both of these with "SignerSign() failed" and a status
# code, so they are checked here where they can be named.
$cert = Get-ChildItem Cert:\CurrentUser\My -ErrorAction SilentlyContinue |
    Where-Object { $_.Thumbprint -eq $thumbprint }
if (-not $cert) {
    throw "Certificate $thumbprint is not in the personal store. Plug the token in with proCertum CardManager closed and let Windows register it; do not install it by hand."
}
if (-not $cert.HasPrivateKey) {
    throw "Certificate $thumbprint has no usable key. Unplug and replug the token with proCertum CardManager closed."
}
$provider = (certutil -user -store My $thumbprint 2>&1 | Select-String -Pattern "Provider =").ToString()
if ($provider -match "crypto3 CSP") {
    throw "Certificate $thumbprint is bound to the legacy crypto3 CSP, which cannot sign with SHA-256. Remove it with 'certutil -user -delstore My $thumbprint', then replug the token so the card registers it through the minidriver."
}

# signtool ships with the Windows SDK and is usually not on PATH. Picking the
# newest version found rather than a fixed one, because the SDK version on a
# machine changes with every Visual Studio update.
function Resolve-SignTool {
    $onPath = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }

    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "${env:ProgramFiles}\Windows Kits\10\bin"
    ) | Where-Object { Test-Path $_ }

    foreach ($root in $roots) {
        $found = Get-ChildItem $root -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\' } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }

    throw "signtool.exe not found. Install the Windows SDK, or put signtool on PATH."
}

$signtool = Resolve-SignTool

& $signtool sign /fd SHA256 /tr $timestampUrl /td SHA256 /sha1 $thumbprint $Path
if ($LASTEXITCODE -ne 0) {
    throw "signtool failed for $Path (exit $LASTEXITCODE). Is the token plugged in and unlocked?"
}
