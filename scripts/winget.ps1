# Writes the winget manifests for a release into dist\winget\<version>\
# (called by package.ps1 -Release; can also be run on its own):
#
#   .\scripts\winget.ps1 -Version 0.1.0 -Sha256 <hash of whereti-setup-0.1.0.exe>
#
# Submitting: after the GitHub release exists, open a pull request to
# github.com/microsoft/winget-pkgs with the three files under
# manifests\s\SoftwareHive\whereti\<version>\ (or `wingetcreate submit <folder>`).
# Then anyone can run:  winget install SoftwareHive.whereti

param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Sha256
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$out = Join-Path $repoRoot "dist\winget\$Version"
New-Item -ItemType Directory -Force $out | Out-Null
$id = "SoftwareHive.whereti"
$schema = "1.9.0"
$url = "https://github.com/whereti/whereti/releases/download/v$Version/whereti-setup-$Version.exe"

$manifestVersion = @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.version.$schema.schema.json
PackageIdentifier: $id
PackageVersion: $Version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $schema
"@

# The installer asks for elevation itself (one UAC prompt) and accepts /S.
# ProductCode is the key it writes under ...\CurrentVersion\Uninstall.
$manifestInstaller = @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.installer.$schema.schema.json
PackageIdentifier: $id
PackageVersion: $Version
MinimumOSVersion: 10.0.17763.0
InstallerType: exe
Scope: machine
ElevationRequirement: elevatesSelf
InstallModes:
- interactive
- silent
- silentWithProgress
InstallerSwitches:
  Silent: /S
  SilentWithProgress: /S
UpgradeBehavior: install
ProductCode: whereti
AppsAndFeaturesEntries:
- DisplayName: whereti
  Publisher: Software Hive S.R.L.
  ProductCode: whereti
Installers:
- Architecture: x64
  InstallerUrl: $url
  InstallerSha256: $($Sha256.ToUpperInvariant())
ManifestType: installer
ManifestVersion: $schema
"@

$manifestLocale = @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.defaultLocale.$schema.schema.json
PackageIdentifier: $id
PackageVersion: $Version
PackageLocale: en-US
Publisher: Software Hive S.R.L.
PublisherUrl: https://whereti.com/who/
PublisherSupportUrl: https://whereti.com/faq/
PrivacyUrl: https://whereti.com/privacy/
Author: Software Hive S.R.L.
PackageName: whereti
PackageUrl: https://whereti.com/
License: MIT
LicenseUrl: https://github.com/whereti/whereti/blob/main/LICENSE
Copyright: Copyright (C) 2026 Software Hive S.R.L.
ShortDescription: Find any file on your PC as you type, by its name or by the words inside it.
Description: |-
  whereti searches the names of every file on your drives and the words inside your documents
  (PDF, Word, Excel, PowerPoint, e-mails, scans and photos), and opens apps and Windows settings.
  Results appear while you type. Nothing about your files leaves your PC.
Moniker: whereti
Tags:
- search
- file-search
- desktop-search
- everything
- find
- ocr
- open-source
ReleaseNotesUrl: https://github.com/whereti/whereti/releases/tag/v$Version
ManifestType: defaultLocale
ManifestVersion: $schema
"@

$utf8 = New-Object Text.UTF8Encoding $false
[IO.File]::WriteAllText((Join-Path $out "$id.yaml"), $manifestVersion + "`n", $utf8)
[IO.File]::WriteAllText((Join-Path $out "$id.installer.yaml"), $manifestInstaller + "`n", $utf8)
[IO.File]::WriteAllText((Join-Path $out "$id.locale.en-US.yaml"), $manifestLocale + "`n", $utf8)
Write-Host "winget manifests in $out"
