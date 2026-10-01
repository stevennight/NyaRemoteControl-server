# Make a host release: set the version (VERSION + Cargo.toml files), pin the
# common commit the build uses (COMMON_REF), commit and tag v<version>.
# Pushing the tag makes GitHub Actions build the installer and publish it.
#
#   .\scripts\release.ps1 0.2.1           # then: git push origin HEAD v0.2.1
#   .\scripts\release.ps1 0.3.0-beta.1 -Push
#
# The client is versioned separately (client\scripts\release.ps1).
param(
    [Parameter(Mandatory = $true, Position = 0)][string]$Version,
    [switch]$Push
)
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $repo '..\common\scripts\release-lib.ps1')
Publish-NyaVersion -Repo $repo -Product 'NyaRemoteControl Server' -Tomls @('Cargo.toml', 'core/Cargo.toml', 'manager/Cargo.toml') -Version $Version -Push:$Push
