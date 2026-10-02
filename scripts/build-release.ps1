# Release build: fetches missing third-party pieces, builds and packages
# (scripts\package.ps1, into dist\NyaRemoteControl), then writes into release\:
#   NyaRemoteControl_<version>_x64-setup.exe     installer (NSIS)
#   NyaRemoteControl_<version>_windows_x64.zip   portable zip
# each with a .sha256 next to it.
#
#   .\scripts\build-release.ps1                 # local build of the current VERSION
#   .\scripts\build-release.ps1 -Tag v0.7.0     # CI: also check the tag matches VERSION
param(
    [string]$Tag = '',
    [switch]$SkipInstaller
)
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $repo '..\common\scripts\release-lib.ps1')
. (Join-Path $PSScriptRoot 'versions.ps1')

$version = Test-NyaVersion $repo $NyaTomls $Tag
Write-Host "== NyaRemoteControl $version" -ForegroundColor Cyan
Initialize-NyaThirdParty -Vigem
& (Join-Path $PSScriptRoot 'package.ps1')

New-NyaReleaseFiles -Repo $repo -Name 'NyaRemoteControl' -Version $version `
    -Stage (Join-Path $repo 'dist\NyaRemoteControl') -Nsi (Join-Path $repo 'installer\NyaRemoteControl.nsi') `
    -Icon (Resolve-Path (Join-Path $repo '..\common\assets\client.ico')).Path -SkipInstaller:$SkipInstaller
