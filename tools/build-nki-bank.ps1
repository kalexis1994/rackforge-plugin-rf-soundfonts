[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$SourceDirectory,

    [Parameter(Mandatory = $true)]
    [string]$OutputFile,

    [string]$Name = "Kontakt instrument library",
    [string]$Id = "kontakt-library",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$source = (Resolve-Path -LiteralPath $SourceDirectory).Path
if (-not (Test-Path -LiteralPath $source -PathType Container)) {
    throw "SourceDirectory is not a directory: $source"
}

$output = [System.IO.Path]::GetFullPath($OutputFile)
$outputParent = [System.IO.Path]::GetDirectoryName($output)
if ([string]::IsNullOrWhiteSpace($outputParent)) {
    throw "OutputFile must have a parent directory."
}
[System.IO.Directory]::CreateDirectory($outputParent) | Out-Null

$sourceWithSeparator = $source.TrimEnd('\', '/') + [System.IO.Path]::DirectorySeparatorChar
if ($output.StartsWith($sourceWithSeparator, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputFile must be outside SourceDirectory so it cannot package itself."
}
if ((Test-Path -LiteralPath $output) -and -not $Force) {
    throw "OutputFile already exists. Pass -Force to replace it: $output"
}

$supported = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
@('.nki', '.wav', '.wave', '.flac', '.tga') | ForEach-Object { [void]$supported.Add($_) }
$files = Get-ChildItem -LiteralPath $source -File -Recurse |
    Where-Object { $supported.Contains($_.Extension) } |
    Sort-Object FullName
$maps = @($files | Where-Object { $_.Extension -ieq '.nki' })
$samples = @($files | Where-Object { $_.Extension -in @('.wav', '.wave', '.flac') })
if ($maps.Count -eq 0) {
    throw "No .nki instruments were found under $source"
}
if ($samples.Count -eq 0) {
    throw "No WAV, WAVE, or FLAC samples were found under $source"
}

Add-Type -AssemblyName System.IO.Compression
$mode = if ($Force) {
    [System.IO.FileMode]::Create
} else {
    [System.IO.FileMode]::CreateNew
}
$stream = [System.IO.File]::Open($output, $mode, [System.IO.FileAccess]::ReadWrite)
try {
    $archive = [System.IO.Compression.ZipArchive]::new(
        $stream,
        [System.IO.Compression.ZipArchiveMode]::Create,
        $false
    )
    try {
        $manifest = [ordered]@{
            schema_version = 1
            id = $Id
            name = $Name
            instrument_count = $maps.Count
            created_by = "RF-Soundfonts build-nki-bank"
        } | ConvertTo-Json
        $manifestEntry = $archive.CreateEntry(
            'bank.json',
            [System.IO.Compression.CompressionLevel]::Optimal
        )
        $manifestStream = $manifestEntry.Open()
        try {
            $writer = [System.IO.StreamWriter]::new(
                $manifestStream,
                [System.Text.UTF8Encoding]::new($false)
            )
            try {
                $writer.Write($manifest)
            } finally {
                $writer.Dispose()
            }
        } finally {
            $manifestStream.Dispose()
        }

        foreach ($file in $files) {
            $relative = [System.IO.Path]::GetRelativePath($source, $file.FullName).Replace('\', '/')
            $entry = $archive.CreateEntry(
                $relative,
                [System.IO.Compression.CompressionLevel]::Optimal
            )
            $entryStream = $entry.Open()
            $sourceStream = [System.IO.File]::OpenRead($file.FullName)
            try {
                $sourceStream.CopyTo($entryStream)
            } finally {
                $sourceStream.Dispose()
                $entryStream.Dispose()
            }
        }
    } finally {
        $archive.Dispose()
    }
} finally {
    $stream.Dispose()
}

$result = Get-Item -LiteralPath $output
[pscustomobject]@{
    Output = $result.FullName
    Bytes = $result.Length
    Instruments = $maps.Count
    Samples = $samples.Count
}
