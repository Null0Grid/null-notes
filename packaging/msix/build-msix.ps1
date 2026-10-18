<#
.SYNOPSIS
  Build the Null Notes MSIX for the Microsoft Store (or a signed sideload test package).

.DESCRIPTION
  1. Builds the release exe with the Tauri CLI (no NSIS/MSI bundle).
  2. Stages exe + AppxManifest.xml (version from tauri.conf.json) + Assets.
  3. Generates resources.pri so the scaled logos resolve.
  4. Packs NullNotes_<version>_x64.msix with makeappx.
  5. Optionally signs it for local install testing.

  Store upload: run without -Sign and upload the .msix in Partner Center. The
  Store re-signs it with your reserved identity, so -Publisher must be the
  Partner Center "Package/Identity/Publisher" value (the default below is the
  one from the 0.3.1 package).

  Local test: create a self-signed cert whose subject equals -Publisher, trust
  it (Local Machine > Trusted People), then:
    .\build-msix.ps1 -Sign -CertPath .\test.pfx -CertPassword (Read-Host -AsSecureString)

.NOTES
  Needs: Rust, `cargo install tauri-cli --version "^2"`, Windows SDK
  (makeappx.exe, makepri.exe, signtool.exe).
#>
param(
  [string]$Publisher = "CN=8685350B-5C4C-4422-9CDF-8AEB87F50B2F",
  [switch]$Sign,
  [string]$CertPath,
  [SecureString]$CertPassword,
  [switch]$SkipBuild
)
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$root = Resolve-Path "$here\..\.."
$tauri = Join-Path $root "src-tauri"

function Find-SdkTool([string]$name) {
  $kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
  $hit = Get-ChildItem $kits -Recurse -Filter $name -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending | Select-Object -First 1
  if (-not $hit) { throw "$name not found. Install the Windows 10/11 SDK." }
  $hit.FullName
}
$makeappx = Find-SdkTool "makeappx.exe"
$makepri  = Find-SdkTool "makepri.exe"

# Version: tauri.conf.json "0.5.0" -> MSIX "0.5.0.0" (the Store requires the 4th part to be 0)
$conf = Get-Content (Join-Path $tauri "tauri.conf.json") -Raw | ConvertFrom-Json
$v = ($conf.version -split '[-+]')[0].Split('.')
while ($v.Count -lt 3) { $v += "0" }
$version = "$($v[0]).$($v[1]).$($v[2]).0"
Write-Host "Null Notes $version"

if (-not $SkipBuild) {
  Push-Location $tauri
  try { cargo tauri build --no-bundle } finally { Pop-Location }
}
$exe = Join-Path $tauri "target\release\null-notes.exe"
if (-not (Test-Path $exe)) { throw "Release exe not found at $exe" }

# Stage
$stage = Join-Path $here "stage"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null
Copy-Item $exe $stage
$loader = Join-Path $tauri "target\release\WebView2Loader.dll"
if (Test-Path $loader) { Copy-Item $loader $stage }
Copy-Item (Join-Path $here "Assets") (Join-Path $stage "Assets") -Recurse
(Get-Content (Join-Path $here "AppxManifest.xml") -Raw).
  Replace("{{VERSION}}", $version).Replace("{{PUBLISHER}}", $Publisher) |
  Set-Content (Join-Path $stage "AppxManifest.xml") -Encoding utf8

# resources.pri (scale-qualified logos)
$pricfg = Join-Path $here "priconfig.xml"
& $makepri createconfig /cf $pricfg /dq en-US /pv 10.0.0 /o | Out-Null
& $makepri new /pr $stage /cf $pricfg /mn (Join-Path $stage "AppxManifest.xml") /of (Join-Path $stage "resources.pri") /o
if ($LASTEXITCODE) { throw "makepri failed" }
Remove-Item $pricfg

# Pack
$out = Join-Path $here "NullNotes_${version}_x64.msix"
& $makeappx pack /d $stage /p $out /o
if ($LASTEXITCODE) { throw "makeappx failed" }

if ($Sign) {
  if (-not $CertPath) { throw "-Sign needs -CertPath" }
  $signtool = Find-SdkTool "signtool.exe"
  $pw = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($CertPassword))
  & $signtool sign /fd SHA256 /a /f $CertPath /p $pw /tr http://timestamp.digicert.com /td SHA256 $out
  if ($LASTEXITCODE) { throw "signtool failed" }
}
Write-Host "Built $out"
