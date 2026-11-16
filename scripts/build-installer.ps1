<#
.SYNOPSIS
    Builds the MixerCraft Windows installer (per-user MSI), its SHA-256 and
    the release feed manifest (latest.json) for the in-app updater.

.DESCRIPTION
    1. cargo build --release of mixercraft-gui, mixercraftd and mixercraft
       (with -Feed, the update feed URL is compiled into the app).
    2. WiX Toolset 5.0.2 from the repository's dotnet tool manifest
       (.config/dotnet-tools.json; MS-RL, free) with the UI and Util
       extensions.
    3. target\installer\MixerCraft-<version>-x64.msi, .msi.sha256 and
       latest.json.

    Needs: Rust (MSVC toolchain), .NET SDK 6 or newer. Runs without
    administrator rights. Nothing is signed (see ADR 0006 / 0007).

.PARAMETER Version
    MSI version (default: workspace version from Cargo.toml). Must be
    major.minor.patch; MSI compares only these three numbers.

.PARAMETER Feed
    Update feed URL compiled into the app (MIXERCRAFT_UPDATE_FEED). Default:
    the latest GitHub release of the public MixerCraft repository. Pass ""
    for a build without a feed (users can still set one in updates.toml).

.PARAMETER AssetUrl
    URL of the MSI written into latest.json (default: the file name, i.e.
    relative to latest.json, which works for GitHub Releases and folders).

.PARAMETER Notes
    Short release notes for latest.json.

.PARAMETER NotesUrl
    Release notes page (https) for latest.json.

.PARAMETER SkipCargo
    Package the existing target\release binaries without rebuilding.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Feed = "https://github.com/JustinSchweiger/MixerCraft/releases/latest/download/latest.json",
    [string]$AssetUrl = "",
    [string]$Notes = "",
    [string]$NotesUrl = "",
    [switch]$SkipCargo
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Invoke-Checked([string]$what, [scriptblock]$cmd) {
    & $cmd
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)" }
}

$cargoVersion = $null
$inWorkspace = $false
foreach ($line in Get-Content (Join-Path $root "Cargo.toml")) {
    if ($line -match '^\[workspace\.package\]') { $inWorkspace = $true; continue }
    if ($line -match '^\[') { $inWorkspace = $false }
    if ($inWorkspace -and $line -match '^version\s*=\s*"([^"]+)"') { $cargoVersion = $Matches[1]; break }
}
if (-not $Version) { $Version = $cargoVersion }
elseif ($Version -ne $cargoVersion) {
    Write-Warning "MSI version $Version differs from the programs' version $cargoVersion (Cargo.toml); only use this for tests."
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Version '$Version' must be major.minor.patch (MSI ignores pre-release tags)"
}

$out = Join-Path $root "target\installer"
$work = Join-Path $out "work"
New-Item -ItemType Directory -Force -Path $work | Out-Null

# 1. Binaries
$bin = Join-Path $root "target\release"
if (-not $SkipCargo) {
    $env:MIXERCRAFT_UPDATE_FEED = $Feed
    Invoke-Checked "cargo build" { cargo build --release --locked -p mixercraft-gui -p mixercraft-engine -p mixercraft-cli }
    Remove-Item Env:\MIXERCRAFT_UPDATE_FEED -ErrorAction SilentlyContinue
}
foreach ($exe in "mixercraft-gui.exe", "mixercraftd.exe", "mixercraft.exe") {
    if (-not (Test-Path (Join-Path $bin $exe))) { throw "missing $bin\$exe (build first or drop -SkipCargo)" }
}

# 2. WiX (pinned in .config/dotnet-tools.json) and its extensions, cached in
#    target\installer\work\.wix.
Invoke-Checked "dotnet tool restore" { dotnet tool restore }
Push-Location $work
try {
    foreach ($ext in "WixToolset.UI.wixext/5.0.2", "WixToolset.Util.wixext/5.0.2") {
        $name = $ext.Split("/")[0]
        if (-not (Test-Path (Join-Path $work ".wix\extensions\$name"))) {
            Invoke-Checked "wix extension add $ext" { dotnet wix extension add $ext }
        }
    }
} finally { Pop-Location }

# 3. Generated inputs: icon, license page, install marker.
$ico = Get-ChildItem -Path (Join-Path $root "crates\mixercraft-gui") -Recurse -Filter "*.ico" -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $ico) {
    # A PNG wrapped in an ICO container (supported since Windows Vista).
    $png = [IO.File]::ReadAllBytes((Join-Path $root "crates\mixercraft-gui\ui\icon.png"))
    $ico = Join-Path $work "mixercraft.ico"
    $ms = New-Object IO.MemoryStream
    $w = New-Object IO.BinaryWriter($ms)
    $w.Write([UInt16]0); $w.Write([UInt16]1); $w.Write([UInt16]1)        # header: icon, 1 image
    $w.Write([Byte]0); $w.Write([Byte]0); $w.Write([Byte]0); $w.Write([Byte]0) # 256x256, no palette
    $w.Write([UInt16]1); $w.Write([UInt16]32)                             # planes, bits per pixel
    $w.Write([UInt32]$png.Length); $w.Write([UInt32]22)                   # size, offset
    $w.Write($png)
    [IO.File]::WriteAllBytes($ico, $ms.ToArray())
}

function ConvertTo-RtfText([string]$text) {
    $sb = New-Object Text.StringBuilder
    foreach ($ch in $text.ToCharArray()) {
        $c = [int]$ch
        if ($ch -eq '\' -or $ch -eq '{' -or $ch -eq '}') { [void]$sb.Append('\').Append($ch) }
        elseif ($ch -eq "`n") { [void]$sb.Append("\par`r`n") }
        elseif ($ch -eq "`r") { }
        elseif ($c -gt 127) { [void]$sb.Append("\u$c?") }
        else { [void]$sb.Append($ch) }
    }
    $sb.ToString()
}
$licenseText = "MixerCraft is licensed under the MIT license OR the Apache License 2.0, at your option.`n" +
    "Virtual devices use the separately installed usbip-win2 driver, which has its own license.`n`n" +
    (Get-Content -Raw (Join-Path $root "LICENSE-MIT")) + "`n`n" + (Get-Content -Raw (Join-Path $root "LICENSE-APACHE"))
$rtf = "{\rtf1\ansi\deff0{\fonttbl{\f0 Segoe UI;}}\fs18 " + (ConvertTo-RtfText $licenseText) + "}"
$licenseRtf = Join-Path $work "license.rtf"
[IO.File]::WriteAllText($licenseRtf, $rtf, [Text.Encoding]::ASCII)

$marker = Join-Path $work "installed-by-msi.txt"
[IO.File]::WriteAllText($marker,
    "Installed by the MixerCraft $Version Windows installer.`r`nThe app uses this file to know it may update itself.`r`n",
    [Text.Encoding]::ASCII)

# 4. MSI
$msiName = "MixerCraft-$Version-x64.msi"
$msi = Join-Path $out $msiName
Push-Location $work
try {
    Invoke-Checked "wix build" {
        dotnet wix build (Join-Path $root "installer\windows\MixerCraft.wxs") `
            -arch x64 `
            -ext WixToolset.UI.wixext -ext WixToolset.Util.wixext `
            -d "Version=$Version" -d "BinDir=$bin" -d "IconFile=$ico" -d "Marker=$marker" -d "LicenseRtf=$licenseRtf" `
            -pdbtype none `
            -o $msi
    }
} finally { Pop-Location }

# 5. Checksum and feed manifest
$sha = (Get-FileHash -Algorithm SHA256 $msi).Hash.ToLowerInvariant()
$size = (Get-Item $msi).Length
[IO.File]::WriteAllText("$msi.sha256", "$sha  $msiName`n", [Text.Encoding]::ASCII)
if (-not $AssetUrl) { $AssetUrl = $msiName }
$manifest = [ordered]@{
    schema    = 1
    version   = $Version
    published = (Get-Date -Format "yyyy-MM-dd")
    notes     = $Notes
    notes_url = $NotesUrl
    assets    = @([ordered]@{ target = "windows-x86_64-msi"; url = $AssetUrl; sha256 = $sha; size = $size })
}
$json = ConvertTo-Json -InputObject $manifest -Depth 5
[IO.File]::WriteAllText((Join-Path $out "latest.json"), $json + "`n", (New-Object Text.UTF8Encoding($false)))

Write-Host "Installer: $msi"
Write-Host "SHA-256:   $sha"
Write-Host "Feed:      $(Join-Path $out 'latest.json')"
