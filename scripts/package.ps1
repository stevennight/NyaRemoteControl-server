# Build a release and collect everything needed on the target machine into dist\NyaRemoteControl.
$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
$ffmpeg = if ($env:NYA_FFMPEG_DIR) { $env:NYA_FFMPEG_DIR } else { Join-Path $repo '..\third_party\ffmpeg' }
Push-Location $repo
try {
    # cargo writes progress to stderr; Windows PowerShell treats that as an error under 'Stop'.
    $ErrorActionPreference = 'Continue'
    cargo build --release --workspace
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    $ErrorActionPreference = 'Stop'
} finally { Pop-Location }
$out = Join-Path $repo 'dist\NyaRemoteControl'
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force $out | Out-Null
# NyaRemoteControl.exe: the program users open. nya-server-svc.exe: the
# remote-control service. nya-server.exe: command line and updater (no FFmpeg).
# The FFmpeg DLLs serve the program (decoding) and the service (encoding).
foreach ($exe in 'NyaRemoteControl.exe', 'nya-server.exe', 'nya-server-svc.exe') {
    Copy-Item (Join-Path $repo "..\target\release\$exe") $out
}
foreach ($dll in 'avcodec-62.dll', 'avutil-60.dll', 'swresample-6.dll') {
    Copy-Item (Join-Path $ffmpeg "bin\$dll") $out
}
Copy-Item (Join-Path $repo 'README.md') $out
$vigem = Join-Path $repo '..\third_party\ViGEmClient\LICENSE'
if (Test-Path $vigem) { Copy-Item $vigem (Join-Path $out 'LICENSE-ViGEmClient.txt') }
# Offline installers of the optional components (see drivers-README.txt).
$drivers = Join-Path $repo '..\third_party\drivers'
$bundle = 'ViGEmBus_1.22.0_x64_x86_arm64.exe', 'USBip-0.9.8.1-x64.exe', 'VirtualDisplayDriver-x86.Driver.Only.zip', 'usbipd-win_5.3.0_x64.msi'
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
