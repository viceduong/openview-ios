#Requires -Version 5.1
<#
.SYNOPSIS
    Build the OpenView Windows release artifacts.

.DESCRIPTION
    1. cargo build --release --locked -p ovgui -p ovhost -p ovview -p ovrelay
    2. THIRD_PARTY_LICENSES.txt (cargo-about when installed, otherwise a
       `cargo metadata` based listing with the license texts found in each
       crate, including the bundled C libraries libopus and zstd)
    3. dist\OpenView-<ver>-windows-x64-portable.zip  (with portable.txt, so
       data stays in <folder>\data)
    4. dist\OpenView-<ver>-setup.exe  (Inno Setup 6, if ISCC.exe is found)
    5. dist\SHA256SUMS

    The version comes from [workspace.package] in Cargo.toml. Honors
    CARGO_TARGET_DIR.

.PARAMETER SkipBuild
    Package the binaries already in <target>\release.

.PARAMETER NoInstaller
    Do not build the installer even if Inno Setup is installed.

.PARAMETER OutDir
    Output folder (default: <repo>\dist).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\package.ps1
    (Windows PowerShell 5.1 or PowerShell 7)
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoInstaller,
    [string]$OutDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Root = Split-Path -Parent $PSScriptRoot
$Packaging = Join-Path $Root 'packaging\windows'
if (-not $OutDir) { $OutDir = Join-Path $Root 'dist' }
$OutDir = [IO.Path]::GetFullPath($OutDir)
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
$BinDir = Join-Path $TargetDir 'release'
$Exes = 'OpenView.exe', 'ovhost.exe', 'ovview.exe', 'ovrelay.exe'

function Step([string]$msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

function Get-WorkspaceVersion {
    $toml = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
    $m = [regex]::Match($toml, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $m.Success) { throw 'version not found in [workspace.package] of Cargo.toml' }
    $m.Groups[1].Value
}

function Find-Iscc {
    $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )
    $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}

# ---------------------------------------------------------------- licenses

$LicenseFilePattern = '^(LICEN[CS]E|COPYING|NOTICE|COPYRIGHT|UNLICENSE)([-._].*)?$'

function Get-LicenseFiles([string]$dir) {
    if (-not (Test-Path $dir)) { return @() }
    Get-ChildItem -LiteralPath $dir -File |
        Where-Object { $_.Name -match $LicenseFilePattern } |
        Sort-Object Name
}

# Fallback when cargo-about is not installed: list every crate linked into
# the four binaries (normal dependencies, Windows x64) with its license, and
# append each crate's license files (identical texts printed once).
function Write-ThirdPartyFallback([string]$outFile, [string]$version) {
    $json = & cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc `
        --manifest-path (Join-Path $Root 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $meta = ($json -join "`n") | ConvertFrom-Json
    $pkgById = @{}
    foreach ($p in $meta.packages) { $pkgById[$p.id] = $p }
    $nodeById = @{}
    foreach ($n in $meta.resolve.nodes) { $nodeById[$n.id] = $n }
    $members = [Collections.Generic.HashSet[string]]::new([string[]]$meta.workspace_members)

    $roots = $meta.packages | Where-Object { $_.name -in 'ovgui', 'ovhost', 'ovview', 'ovrelay' -and $members.Contains($_.id) }
    $seen = [Collections.Generic.HashSet[string]]::new()
    $queue = [Collections.Generic.Queue[string]]::new()
    foreach ($r in $roots) { $queue.Enqueue($r.id) }
    while ($queue.Count -gt 0) {
        $id = $queue.Dequeue()
        if (-not $seen.Add($id)) { continue }
        foreach ($d in $nodeById[$id].deps) {
            $normal = $d.dep_kinds | Where-Object { $null -eq $_.kind }
            if ($normal) { $queue.Enqueue($d.pkg) }
        }
    }
    $deps = $seen | Where-Object { -not $members.Contains($_) } | ForEach-Object { $pkgById[$_] } |
        Sort-Object name, version

    $sb = [Text.StringBuilder]::new()
    $null = $sb.AppendLine("Third-party software in OpenView $version for Windows")
    $null = $sb.AppendLine('=' * 60)
    $null = $sb.AppendLine()
    $null = $sb.AppendLine('OpenView itself is MIT licensed (see LICENSE). The Windows binaries')
    $null = $sb.AppendLine('(OpenView.exe, ovhost.exe, ovview.exe, ovrelay.exe) include the')
    $null = $sb.AppendLine('following open-source components. License texts follow the list.')
    $null = $sb.AppendLine()
    $null = $sb.AppendLine('Slint (https://slint.dev) is used under the Slint Royalty-free')
    $null = $sb.AppendLine('Desktop, Mobile, and Web Applications License 2.0; see')
    $null = $sb.AppendLine('https://slint.dev/terms-and-conditions. "Made with Slint" is shown on')
    $null = $sb.AppendLine("OpenView's About page.")
    $null = $sb.AppendLine()
    $null = $sb.AppendLine('Bundled C libraries (compiled from source into the binaries):')
    $null = $sb.AppendLine('  libopus  (via opusic-sys)  BSD-3-Clause  https://opus-codec.org')
    $null = $sb.AppendLine('  zstd     (via zstd-sys)    BSD-3-Clause  https://facebook.github.io/zstd')
    $null = $sb.AppendLine()
    $null = $sb.AppendLine('Rust crates')
    $null = $sb.AppendLine('-' * 60)
    foreach ($p in $deps) {
        $lic = if ($p.license) { $p.license } elseif ($p.license_file) { "see $($p.license_file)" } else { 'unknown' }
        $null = $sb.AppendLine(('{0} {1}  [{2}]  {3}' -f $p.name, $p.version, $lic, $p.repository))
    }
    $null = $sb.AppendLine()
    $null = $sb.AppendLine('License texts')
    $null = $sb.AppendLine('-' * 60)

    $textOwner = @{}
    $sha = [Security.Cryptography.SHA256]::Create()
    foreach ($p in $deps) {
        $dir = Split-Path -Parent $p.manifest_path
        $files = @(Get-LicenseFiles $dir)
        # -sys crates vendor their C library one folder down (opus\COPYING,
        # zstd\LICENSE).
        if ($p.name -like '*-sys') {
            foreach ($sub in Get-ChildItem -LiteralPath $dir -Directory) {
                $files += @(Get-LicenseFiles $sub.FullName)
            }
        }
        if ($files.Count -eq 0) { continue }
        foreach ($f in $files) {
            $text = (Get-Content -LiteralPath $f.FullName -Raw -Encoding utf8)
            if ($null -eq $text) { continue }
            $text = $text.Trim()
            $rel = $f.FullName.Substring($dir.TrimEnd('').Length + 1)
            $hash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes(($text -replace '\s+', ' '))))
            $null = $sb.AppendLine()
            $null = $sb.AppendLine("--- $($p.name) $($p.version): $rel")
            if ($textOwner.ContainsKey($hash)) {
                $null = $sb.AppendLine("(same text as $($textOwner[$hash]))")
            } else {
                $textOwner[$hash] = "$($p.name) $($p.version): $rel"
                $null = $sb.AppendLine($text)
            }
        }
    }
    [IO.File]::WriteAllText($outFile, ($sb.ToString() -replace "`r?`n", "`r`n"), [Text.UTF8Encoding]::new($false))
    Write-Host "    $($deps.Count) crates listed (cargo metadata fallback)"
}

function Write-ThirdPartyLicenses([string]$outFile, [string]$version) {
    $about = Get-Command cargo-about -ErrorAction SilentlyContinue
    if ($about) {
        Step 'THIRD_PARTY_LICENSES.txt (cargo-about)'
        & cargo about generate --locked `
            -c (Join-Path $Root 'packaging\about.toml') `
            -o $outFile (Join-Path $Root 'packaging\about.hbs') `
            --manifest-path (Join-Path $Root 'Cargo.toml')
        if ($LASTEXITCODE -eq 0) { return }
        Write-Warning 'cargo-about failed; using the cargo metadata fallback'
    }
    Step 'THIRD_PARTY_LICENSES.txt (cargo metadata)'
    Write-ThirdPartyFallback $outFile $version
}

# ---------------------------------------------------------------- main

$Version = Get-WorkspaceVersion
Step "OpenView $Version -> $OutDir"

if (-not $SkipBuild) {
    Step 'cargo build --release'
    & cargo build --release --locked -p ovgui -p ovhost -p ovview -p ovrelay `
        --manifest-path (Join-Path $Root 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}
foreach ($e in $Exes) {
    if (-not (Test-Path (Join-Path $BinDir $e))) { throw "missing $BinDir\$e (build first or drop -SkipBuild)" }
}

$Stage = Join-Path $OutDir 'stage'
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force $Stage | Out-Null

Step 'staging files'
foreach ($e in $Exes) { Copy-Item (Join-Path $BinDir $e) $Stage }
Copy-Item (Join-Path $Root 'LICENSE') $Stage
Copy-Item (Join-Path $Packaging 'firewall.cmd') $Stage
$readme = (Get-Content (Join-Path $Packaging 'README.txt') -Raw) -replace '\{VERSION\}', $Version
[IO.File]::WriteAllText((Join-Path $Stage 'README.txt'), ($readme -replace "`r?`n", "`r`n"), [Text.UTF8Encoding]::new($false))
Write-ThirdPartyLicenses (Join-Path $Stage 'THIRD_PARTY_LICENSES.txt') $Version

# Portable zip: one top-level folder; portable.txt keeps data in .\data.
$ZipName = "OpenView-$Version-windows-x64-portable.zip"
$Zip = Join-Path $OutDir $ZipName
Step $ZipName
$PortableRoot = Join-Path $OutDir "portable"
$PortableDir = Join-Path $PortableRoot "OpenView-$Version"
if (Test-Path $PortableRoot) { Remove-Item -Recurse -Force $PortableRoot }
New-Item -ItemType Directory -Force $PortableDir | Out-Null
Copy-Item (Join-Path $Stage '*') $PortableDir
Copy-Item (Join-Path $Packaging 'portable.txt') $PortableDir
if (Test-Path $Zip) { Remove-Item -Force $Zip }
# Entries are added one by one with "/" separators: Compress-Archive and
# ZipFile.CreateFromDirectory on Windows PowerShell 5.1 write "\", which
# other unzip tools treat as part of the file name.
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$zipStream = [IO.File]::Open($Zip, [IO.FileMode]::CreateNew)
try {
    $archive = New-Object IO.Compression.ZipArchive($zipStream, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($f in Get-ChildItem -LiteralPath $PortableDir -File | Sort-Object Name) {
            $entry = "OpenView-$Version/$($f.Name)"
            $null = [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $f.FullName, $entry, [IO.Compression.CompressionLevel]::Optimal)
        }
    } finally { $archive.Dispose() }
} finally { $zipStream.Dispose() }
Remove-Item -Recurse -Force $PortableRoot

# Installer.
$Setup = Join-Path $OutDir "OpenView-$Version-setup.exe"
if (Test-Path $Setup) { Remove-Item -Force $Setup }
$Iscc = if ($NoInstaller) { $null } else { Find-Iscc }
if ($Iscc) {
    Step "OpenView-$Version-setup.exe (Inno Setup: $Iscc)"
    & $Iscc /Q "/DAppVersion=$Version" "/DSourceDir=$Stage" "/DOutputDir=$OutDir" (Join-Path $Packaging 'openview.iss')
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
} elseif ($NoInstaller) {
    Write-Host '    installer skipped (-NoInstaller)'
} else {
    Write-Warning 'Inno Setup 6 (ISCC.exe) not found; installer skipped. Install it from https://jrsoftware.org/isinfo.php'
}

# Checksums (sha256sum format).
Step 'SHA256SUMS'
$artifacts = Get-ChildItem $OutDir -File | Where-Object { $_.Name -like "OpenView-$Version-*" -and $_.Extension -in '.zip', '.exe' } | Sort-Object Name
$sums = $artifacts | ForEach-Object { '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $_.Name }
[IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS'), (($sums -join "`n") + "`n"), [Text.UTF8Encoding]::new($false))

Step 'done'
foreach ($a in $artifacts) { Write-Host ('    {0,-48} {1,8:N1} MB' -f $a.Name, ($a.Length / 1MB)) }
Write-Host "    SHA256SUMS"
