# patch1.ps1 — stage the nativebridge crate into the build dir.
#
# For a normal (fetched) lib, build.ps1 would clone/extract sources and fetch.ps1
# writes the `_1kiss` sentry. nativebridge is a `local: true` module: build.ps1
# only creates an empty buildsrc/nativebridge dir, so here we copy our in-repo
# crate into it and write the sentry ourselves (its "ver:" line drives
# build.ps1's up-to-date cache check).
param(
    $lib_src,
    $ver
)

# The crate lives directly in native/ next to this script and build.yml.
$src_root = $PSScriptRoot

Write-Host "nativebridge patch1: staging crate '$src_root' -> '$lib_src'"

if (!(Test-Path $lib_src)) {
    New-Item $lib_src -ItemType Directory -Force | Out-Null
}

Copy-Item -Path (Join-Path $src_root 'Cargo.toml') -Destination $lib_src -Force
# native/build.yml `ver:` is the single version source; sync the staged
# Cargo.toml so CARGO_PKG_VERSION (returned by nativebridge_version()) matches.
$cargo_manifest = Join-Path $lib_src 'Cargo.toml'
(Get-Content $cargo_manifest -Raw) -replace '(?m)^version = ".*"', "version = `"$ver`"" |
    Set-Content $cargo_manifest -NoNewline
Copy-Item -Path (Join-Path $src_root 'Cargo.lock') -Destination $lib_src -Force
Copy-Item -Path (Join-Path $src_root 'build.rs') -Destination $lib_src -Force
Copy-Item -Path (Join-Path $src_root 'src') -Destination $lib_src -Recurse -Force

$sentry = Join-Path $lib_src '_1kiss'
[System.IO.File]::WriteAllText($sentry, "ver: $ver")
