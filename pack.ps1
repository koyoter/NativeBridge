#!/usr/bin/env pwsh
# pack.ps1 — local one-shot: build the full dependency chain + the nativebridge
# module for one platform, then report the produced native lib for quick local
# verification. (For the full multi-platform Unity package, push a v* tag and
# let CI run pack/dist1.sh.)
#
# Usage:
#   ./pack.ps1                        # host platform, x64
#   ./pack.ps1 -platform win32 -arch arm64
param(
    $platform = $(if ($IsWindows) { 'win32' } elseif ($IsMacOS) { 'osx' } else { 'linux' }),
    $arch = 'x64',
    [switch]$rebuild
)

$ErrorActionPreference = 'Stop'

Write-Host "nativebridge pack: building $platform/$arch"
& (Join-Path $PSScriptRoot 'build.ps1') -p $platform -a $arch -rebuild:$rebuild
if ($LASTEXITCODE) { throw "build.ps1 failed" }

# Locate the produced native lib.
$install = Join-Path $PSScriptRoot "install_${platform}_${arch}/nativebridge"
Write-Host "nativebridge pack: install dir = $install"
Get-ChildItem -Recurse $install -Include *.dll, *.so, *.dylib, *.a -ErrorAction SilentlyContinue |
    ForEach-Object { Write-Host "  produced: $($_.FullName)" }

Write-Host "nativebridge pack: to run the C# tests, copy the lib for your platform into csharp/NativeBridge.Tests/native/Plugins (see the README there), then 'dotnet test csharp/NativeBridge.Tests'."
