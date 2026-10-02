# Make a release: set the version (VERSION + every Cargo.toml), pin the
# common commit the build uses (COMMON_REF), commit and tag v<version>.
# Pushing the tag makes GitHub Actions build the installer and publish it.
#
#   .\scripts\release.ps1 0.7.1           # then: git push origin HEAD v0.7.1
#   .\scripts\release.ps1 0.8.0-beta.1 -Push
#
# The program, the service and the command line share one version.
param(
    [Parameter(Mandatory = $true, Position = 0)][string]$Version,
    [switch]$Push
)
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $repo '..\common\scripts\release-lib.ps1')
. (Join-Path $PSScriptRoot 'versions.ps1')
Publish-NyaVersion -Repo $repo -Product 'NyaRemoteControl' -Tomls $NyaTomls -Version $Version -Push:$Push
