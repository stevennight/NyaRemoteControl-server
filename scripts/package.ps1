# Build a release and collect everything needed on the target machine into dist\nya-server.
$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
$ffmpeg = if ($env:NYA_FFMPEG_DIR) { $env:NYA_FFMPEG_DIR } else { Join-Path $repo '..\third_party\ffmpeg' }
Push-Location $repo
try {
    # cargo writes progress to stderr; Windows PowerShell treats that as an error under 'Stop'.
    $ErrorActionPreference = 'Continue'
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    $ErrorActionPreference = 'Stop'
} finally { Pop-Location }
$out = Join-Path $repo 'dist\nya-server'
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item (Join-Path $repo '..\target\release\nya-server.exe') $out
foreach ($dll in 'avcodec-62.dll', 'avutil-60.dll', 'swresample-6.dll') {
    Copy-Item (Join-Path $ffmpeg "bin\$dll") $out
}
Copy-Item (Join-Path $repo 'README.md') $out
$vigem = Join-Path $repo '..\third_party\ViGEmClient\LICENSE'
if (Test-Path $vigem) { Copy-Item $vigem (Join-Path $out 'LICENSE-ViGEmClient.txt') }
Write-Host "packaged to $out"
