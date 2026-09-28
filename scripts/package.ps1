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
# nya-server.exe: management (GUI / CLI). nya-server-svc.exe + FFmpeg DLLs: the host.
foreach ($exe in 'nya-server.exe', 'nya-server-svc.exe') {
    Copy-Item (Join-Path $repo "..\target\release\$exe") $out
}
foreach ($dll in 'avcodec-62.dll', 'avutil-60.dll', 'swresample-6.dll') {
    Copy-Item (Join-Path $ffmpeg "bin\$dll") $out
}
Copy-Item (Join-Path $repo 'README.md') $out
$vigem = Join-Path $repo '..\third_party\ViGEmClient\LICENSE'
if (Test-Path $vigem) { Copy-Item $vigem (Join-Path $out 'LICENSE-ViGEmClient.txt') }
$drivers = Join-Path $repo '..\third_party\drivers'
$bundle = 'ViGEmBus_1.22.0_x64_x86_arm64.exe', 'USBip-0.9.8.1-x64.exe', 'VirtualDisplayDriver-x86.Driver.Only.zip'
if (Test-Path $drivers) {
    New-Item -ItemType Directory -Force (Join-Path $out 'drivers') | Out-Null
    foreach ($f in $bundle) {
        $src = Join-Path $drivers $f
        if (Test-Path $src) { Copy-Item $src (Join-Path $out 'drivers') }
    }
    Copy-Item (Join-Path $PSScriptRoot 'drivers-README.txt') (Join-Path $out 'drivers\README.txt')
} else {
    Write-Host 'no offline drivers (run common\scripts\fetch-drivers.ps1); one-click install will download'
}
Write-Host "packaged to $out"
