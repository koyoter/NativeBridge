# install1.ps1 — copy Rust build artifacts to the install tree.
#
# Args: $install_dir $lib_src
# Called by build.ps1 (via &) after a successful build. Because build1.ps1 was
# dot-sourced in the same function scope, $rust_target, $target_os, $target_cpu
# are all visible here through PowerShell scope inheritance.
#
# Output layout:
#   Windows:  install_dir/bin/NativeBridge.dll
#   Linux/Android: install_dir/lib/libNativeBridge.so
#   macOS:    install_dir/lib/libNativeBridge.dylib
#   iOS/tvOS: install_dir/lib/libNativeBridge.a   (static archive)

param(
    $install_dir,
    $lib_src
)

function fail($msg) { throw "nativebridge install: $msg" }

if (-not $target_os) { fail 'target_os not in scope' }
if (-not $target_cpu) { fail 'target_cpu not in scope' }
if (-not $rust_target) { fail 'rust_target not in scope (build1.ps1 did not set it)' }

$release_dir = Join-Path $lib_src "target/$rust_target/release"
Write-Host "nativebridge install: target=$rust_target release_dir=$release_dir"

$use_static = ($target_os -eq 'ios' -or $target_os -eq 'tvos')
if ($use_static) {
    $out_subdir = 'lib'
    $src_name = 'libNativeBridge.a'
}
elseif ($target_os -eq 'win32') {
    $out_subdir = 'bin'
    $src_name = 'NativeBridge.dll'
}
elseif ($target_os -eq 'osx') {
    $out_subdir = 'lib'
    $src_name = 'libNativeBridge.dylib'
}
else {
    $out_subdir = 'lib'
    $src_name = 'libNativeBridge.so'
}

$src_path = Join-Path $release_dir $src_name
if (-not (Test-Path $src_path -PathType Leaf)) {
    fail "build artifact not found: $src_path"
}

$dst_dir = Join-Path $install_dir $out_subdir
if (-not (Test-Path $dst_dir -PathType Container)) {
    New-Item $dst_dir -ItemType Directory -Force | Out-Null
}

$dst_path = Join-Path $dst_dir $src_name
Copy-Item -Path $src_path -Destination $dst_path -Force
Write-Host "nativebridge install: copied $src_path -> $dst_path"

if ($target_os -eq 'win32') {
    $lib_src_path = Join-Path $release_dir 'NativeBridge.dll.lib'
    if (Test-Path $lib_src_path -PathType Leaf) {
        Copy-Item -Path $lib_src_path -Destination (Join-Path $dst_dir 'NativeBridge.lib') -Force
        Write-Host "nativebridge install: copied import lib"
    }
    $pdb_src = Join-Path $release_dir 'NativeBridge.pdb'
    if (Test-Path $pdb_src -PathType Leaf) {
        Copy-Item -Path $pdb_src -Destination (Join-Path $dst_dir 'NativeBridge.pdb') -Force
    }
}
