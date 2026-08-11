param(
    [string]$Output,
    [string]$RackForgeRoot = $env:RACKFORGE_ROOT
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $RackForgeRoot) { $RackForgeRoot = Join-Path (Split-Path $repoRoot) 'rackforge' }
$RackForgeRoot = (Resolve-Path $RackForgeRoot).Path
if (-not $Output) { $Output = Join-Path $repoRoot 'artifacts\rf-soundfonts-0.2.0.rfplugin' }
$Output = [System.IO.Path]::GetFullPath($Output)
if (-not $Output.EndsWith('.rfplugin', [System.StringComparison]::OrdinalIgnoreCase)) { throw 'Output must end in .rfplugin' }
if (Test-Path -LiteralPath $Output) { throw "Refusing to overwrite existing package $Output" }

$sourceUrl = 'https://freepats.zenvoid.org/Piano/YDP-GrandPiano/YDP-GrandPiano-SF2-20160804.tar.bz2'
$sourceSha256 = 'D243DC3E182A60DF2A16E92828C1821CF3EB5748B45E2E2BDCFA9CF7AF056026'
$bankSha256 = '8757076AECF80ABDAD1E8F8F6168370399B06F94481796986E6E75CECA09AD21'
$tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$stage = Join-Path $tempRoot ('rf-soundfonts-portable-' + [guid]::NewGuid().ToString('N'))
$source = Join-Path $stage 'source.tar.bz2'
$extract = Join-Path $stage 'upstream'
$package = Join-Path $stage 'package'
New-Item -ItemType Directory -Path $extract -Force | Out-Null
New-Item -ItemType Directory -Path $package -Force | Out-Null

Push-Location $repoRoot
try {
    Invoke-WebRequest -Uri $sourceUrl -OutFile $source
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash -ne $sourceSha256) { throw 'YDP source archive checksum mismatch' }
    tar -xjf $source -C $extract
    if ($LASTEXITCODE -ne 0) { throw 'Could not extract the YDP source archive' }
    $bank = Join-Path $extract 'YDP-GrandPiano-SF2-20160804\YDP-GrandPiano-20160804.sf2'
    $attribution = Join-Path $extract 'YDP-GrandPiano-SF2-20160804\YDP-GrandPiano-20160804.txt'
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $bank).Hash -ne $bankSha256) { throw 'YDP SoundFont checksum mismatch' }

    cargo +stable-x86_64-pc-windows-msvc build --locked --release -p rackforge-rf-soundfonts-portable --target wasm32-unknown-unknown
    if ($LASTEXITCODE -ne 0) { throw 'WebAssembly build failed' }
    Copy-Item (Join-Path $repoRoot 'portable-plugin\package\*') $package -Recurse
    New-Item -ItemType Directory -Path (Join-Path $package 'assets') -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $package 'licenses') -Force | Out-Null
    Copy-Item $bank (Join-Path $package 'assets\ydp-grand-piano.sf2')
    Copy-Item $attribution (Join-Path $package 'licenses\YDP-Grand-Piano.txt')
    Copy-Item (Join-Path $repoRoot 'THIRD_PARTY_NOTICES.md') $package
    New-Item -ItemType Directory -Path (Split-Path $Output) -Force | Out-Null
    cargo +stable-x86_64-pc-windows-msvc run --manifest-path (Join-Path $RackForgeRoot 'Cargo.toml') --locked -p rackforge-store -- pack-wasm $package (Join-Path $repoRoot 'target\wasm32-unknown-unknown\release\rackforge_rf_soundfonts_portable.wasm') $Output
    if ($LASTEXITCODE -ne 0) { throw 'RackForge packaging failed' }
} finally {
    Pop-Location
    $resolved = [System.IO.Path]::GetFullPath($stage)
    if ($resolved.StartsWith($tempRoot, [System.StringComparison]::OrdinalIgnoreCase)) { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
Write-Output "RFPLUGIN_BUILT path=$Output"
