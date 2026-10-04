# clean1.ps1 — trim the nativebridge install tree and strip symbols to shrink
# the shipped binary. Rust already produces LTO-optimized, dead-code-stripped
# binaries via Cargo.toml profile settings (lto=true, codegen-units=1, panic=abort);
# here we run an additional platform strip on .so/.dylib/.a to drop debug info.

$install_dir = $args[0]

function sremove($path) {
    if (Test-Path $path) { Remove-Item $path -Recurse -Force }
}

function invoke_strip($tool, $stripArgs, $file) {
    $prog = Get-Command $tool -ErrorAction SilentlyContinue
    if (!$prog) { return $false }
    $before = (Get-Item $file).Length
    & $prog.Source @stripArgs $file 2>$null
    if ($LASTEXITCODE -eq 0) {
        $after = (Get-Item $file).Length
        Write-Output ("nativebridge clean: stripped {0} ({1:N0} -> {2:N0} bytes)" -f (Split-Path $file -Leaf), $before, $after)
        return $true
    }
    return $false
}

function strip_file($file) {
    $ext = [System.IO.Path]::GetExtension($file)
    $tools = @('llvm-strip', 'strip')
    foreach ($t in $tools) {
        if ($ext -eq '.a') {
            if (invoke_strip $t @('-x') $file) { return }
        }
        else {
            if (invoke_strip $t @('--strip-unneeded') $file) { return }
            if (invoke_strip $t @('-x') $file) { return }
        }
    }
    Write-Output "nativebridge clean: no usable strip tool for $(Split-Path $file -Leaf), skipped"
}

if ((Test-Path $install_dir -PathType Container)) {
    Write-Output "Cleaning ${install_dir}..."

    sremove "$install_dir/include"
    sremove "$install_dir/share"

    foreach ($sub in @('lib', 'bin')) {
        $dir = Join-Path $install_dir $sub
        if (Test-Path $dir -PathType Container) {
            Get-ChildItem $dir -File -Include '*.so', '*.dylib', '*.a' -Recurse | ForEach-Object {
                strip_file $_.FullName
            }
        }
    }
}
