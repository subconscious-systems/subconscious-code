param([string]$Target = 'x86_64-pc-windows-msvc')
$ErrorActionPreference = 'Stop'
if ($Target -ne 'x86_64-pc-windows-msvc') { throw "Unsupported Windows release target: $Target" }
$binary = Join-Path $PSScriptRoot "..\target\$Target\release\sc.exe"
$dist = Join-Path $PSScriptRoot '..\dist'
if (-not (Test-Path -LiteralPath $binary)) { throw "Build sc first: $binary" }
$reader = New-Object IO.BinaryReader([IO.File]::OpenRead($binary))
try {
    if ($reader.ReadUInt16() -ne 0x5a4d) { throw 'Missing MZ header' }
    $reader.BaseStream.Position = 0x3c
    $offset = $reader.ReadUInt32()
    $reader.BaseStream.Position = $offset
    if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw 'Expected an x64 PE executable' }
} finally { $reader.Dispose() }
New-Item -ItemType Directory -Force $dist | Out-Null
# Keep the internal Cargo target and all Unix packaging unchanged. Windows
# must never publish/install a binary named like the system service controller.
$executable = Join-Path $dist 'marathon.exe'
Copy-Item -LiteralPath $binary -Destination $executable -Force
$version = & $executable --version
if ($LASTEXITCODE -ne 0 -or $version -notmatch '^marathon ') { throw 'Marathon version smoke test failed' }
Write-Output $version
$help = & $executable --help
if ($LASTEXITCODE -ne 0 -or ($help -join "`n") -notmatch 'Usage: marathon') { throw 'Marathon help smoke test failed' }
$archive = Join-Path $dist "marathon-$Target.zip"
Compress-Archive -LiteralPath $executable -DestinationPath $archive -Force
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead($archive)
try {
    if ($zip.Entries.Count -ne 1 -or $zip.Entries[0].FullName -ne 'marathon.exe') { throw 'Expected exactly one root marathon.exe' }
} finally { $zip.Dispose() }
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText("$archive.sha256", "$hash  marathon-$Target.zip`n", (New-Object Text.UTF8Encoding $false))
Write-Output "Packaged $archive"
