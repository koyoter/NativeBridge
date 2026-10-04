#!/usr/bin/env pwsh
# build.ps1 — build the NativeBridge native library (Rust) and its locked C
# dependency chain (zlib, boringssl, nghttp3, ngtcp2, nghttp2, curl) for one
# target platform. Dependency build configs live under deps/, the nativebridge
# module itself under native/ (a `local: true` 1k module).
param(
    $platform,
    $arch,
    $sdk = '',
    [switch]$rebuild
)

$target_os = $platform
$target_cpu = $arch

Set-Alias println Write-Host

function mkdirs($path) {
    if (!(Test-Path $path -PathType Container)) {
        New-Item $path -ItemType Directory 1>$null 2>$null
    }
}

function sremove($path) {
    if (Test-Path $path) { Remove-Item $path -Recurse -Force } 
}

# Fixed dependency chain; nativebridge builds last so every dependency's
# $<lib>_install_dir global is already set for its build1.ps1.
$libs = @(
    'zlib'
    'boringssl'
    'nghttp3'
    'ngtcp2'
    'nghttp2'
    'curl'
    'nativebridge'
)
Write-Output "building $($libs.Count) libs ...", $libs

$_1k_root = $PSScriptRoot
println "1kiss: _1k_root=$_1k_root"

println "1kiss: env:NO_DLL=$env:NO_DLL"

if ($target_cpu -eq 'amd64_arm64') {
    $target_cpu = 'arm64'
}

$1k_script = Join-Path "$_1k_root" "1k/1kiss.ps1"
$fetch_script = Join-Path "$_1k_root" "1k/fetch.ps1"
$build_src = Join-Path $_1k_root "buildsrc"
$install_path = "install_${target_os}"

if ($target_cpu -ne '*') {
    $install_path = "${install_path}_$target_cpu"
}
if ($sdk.StartsWith('sim')) { $install_path += '_sim' }
$install_root = Join-Path $_1k_root $install_path

# Create buildsrc tmp dir for build libs
mkdirs $build_src

# import yaml parser
if ((Get-Module -ListAvailable -Name powershell-yaml) -eq $null) {
    Install-Module -Name powershell-yaml -Force -Repository PSGallery -Scope CurrentUser
}

$forward_args = @{}
if ($rebuild) {
    $forward_args['rebuild'] = $true
}
if ($sdk) {
    $forward_args['sdk'] = $sdk
}

if ($target_os -eq 'osx') {
    $forward_args['minsdk'] = '10.13'
}

. $1k_script -p $target_os -a $target_cpu @forward_args -setupOnly -ndkOnly
setup_nasm

if ($IsWin) {
    #relocate powershell.exe to opensource edition pwsh.exe to solve angle gclient execute issues:
    # Get-FileHash is not recognized as a name of a cmdlet
    $pwshPath = $(Get-Command pwsh).Path
    $pwshDir = Split-Path -Path $pwshPath

    $powershell_ver = $(powershell -Command { $PSVersionTable.PSVersion.ToString(); } | Out-String)
    if ([System.VersionEx]$powershell_ver -lt [System.VersionEx]'7.0.0') {

        $eap = $ErrorActionPreference
        $ErrorActionPreference = 'SilentlyContinue'
        Start-Process powershell -ArgumentList '-Command', "Copy-Item '$pwshDir\pwsh.exe' '$pwshDir\powershell.exe'" -WindowStyle Hidden -Wait -Verb runas
        $ErrorActionPreference = $eap

        $env:Path = "$pwshPath;$env:Path"
        $powershell_ver = $(powershell -Command { $PSVersionTable.PSVersion.ToString(); } | Out-String)
    }

    echo "powershell.exe version was relocated to $powershell_ver"
}

if ($Global:is_android) {
    active_ndk_toolchain
    # ARMv7 needs API 21 because Rust std uses dl_iterate_phdr, which the NDK's
    # ARM libdl stub does not expose at API 19. x86 exposes it at API 19.
    $Global:android_api_level = @{arm64 = 21; x64 = 22; armv7 = 21; x86 = 19 }[$target_cpu]
}
elseif ($is_darwin_family) {
    # query xcode version
    $xcode_ver_str = xcodebuild -version | Select-Object -First 1
    if ($xcode_ver_str) {
        $matchInfo = [Regex]::Match($xcode_ver_str, '(\d+\.)+(\*|\d+)(\-[a-z]+[0-9]*)?')
        $Global:XCODE_VERSION = $matchInfo.Value
    }

    if (!$Global:XCODE_VERSION) {
        throw "1kiss: query XCODE_VERSION fail"
    }

    println "1kiss: XCODE_VERSION=$Global:XCODE_VERSION"

    # require xcutils.ps1 for xcode_get_sdkname
    . $(Join-Path $_1k_root '1k/xcutils.ps1')
}

mkdirs $install_root

# options_xxx, xxx = msw, unix, embed
$embed_family = ''
if ($is_win_family) {
    $os_family = 'msw'
    setup_msvc
}
else {
    $os_family = 'unix'
    if ($Global:is_ios -or $Global:is_tvos -or $Global:is_android) {
        $embed_family = 'embed'
    }
}

$darwin_family = ''
if ($is_darwin_family) {
    $darwin_family = 'darwin'
}

$compiler_dumped = $false

Foreach ($lib_name in $libs) {
    $lib_info = $lib_name.Split(':')
    $lib_name = $lib_info[0]
    $cb_target = $lib_info[1]

    # Per-lib build config dir: deps/<lib> for the C dependency chain,
    # native/ for the local nativebridge module.
    $lib_root = if ($lib_name -eq 'nativebridge') { Join-Path $_1k_root 'native' } else { Join-Path $_1k_root "deps/$lib_name" }

    $build_conf_path = Join-Path $lib_root 'build.yml'
    $build_conf = ConvertFrom-Yaml -Yaml (Get-Content $build_conf_path -raw)
    if ($build_conf.targets -and !$build_conf.targets.contains($target_os)) {
        println "Skip build $lib_name which is not allow for target: $target_os"
        continue
    }
    
    if ($build_conf.archs -and !$build_conf.archs.contains($target_cpu)) {
        println "Skip build $lib_name which is not allow for arch: $target_cpu"
        continue
    }
    
    # fetch repo, return variable: $lib_src
    $rel_script = Join-Path $lib_root 'rel1.ps1'
    $version = $build_conf.ver
    $revision = $null # commit_hash
    if (Test-Path $rel_script -PathType Leaf) {
        $version, $revision = &$rel_script $build_conf.ver
    }
    else {
        $revision = "$($build_conf.tag_prefix)$version"
        if ($build_conf.tag_dot2ul) {
            $revision = $revision.Replace('.', '_')
        }
    }

    # Local source modules (nativebridge) have no upstream repo: they ship their
    # sources in-repo (native/) and are staged by patch1.ps1. Skip all
    # repo/fetch handling for them.
    $is_local = [bool]$build_conf.local

    $is_gn = (!$is_local) -and ($build_conf.cb_tool -eq 'gn')
    if ($is_gn) {
        setup_gclient
    }
    if (!$is_local) {
        if ($build_conf.repo.contains('$ver')) {
            $repo_url = $build_conf.repo -replace '\$ver', $version
        } else {
            $repo_url = $build_conf.repo
        }
    }

    # Skip libs that a restored cache already built. The install dir carries a
    # '_1kiss' sentry (copied from the source tree after a successful build) whose
    # first line is "ver: <version>". If it matches the version we're about to
    # build, the cached artifacts are current and we can skip fetch+configure+
    # compile entirely. Correctness of "current" is guaranteed by the CI cache
    # key, which hashes every input that affects the artifacts (build.yml,
    # patch1.ps1, *.patch, build.ps1, 1k/**); any change misses the cache, leaves
    # the install dir empty, and forces a full rebuild. -rebuild bypasses this.
    $install_dir = Join-Path $install_root $lib_name
    if (!$rebuild) {
        $cached_sentry = Join-Path $install_dir '_1kiss'
        if (Test-Path $cached_sentry -PathType Leaf) {
            $cached_ver = ((Get-Content $cached_sentry -First 1) -replace '^ver:\s*', '').Trim()
            if ($cached_ver -eq "$version") {
                println "Skip build ${lib_name} ${version}: up-to-date artifacts found in $install_dir (cache hit)"
                Set-Variable -Name "${lib_name}_install_dir" -Value ($install_dir -replace '\\', '/') -Scope Global
                continue
            }
            println "Rebuilding ${lib_name}: cached version '$cached_ver' != target '$version'"
        }
    }

    if ($is_local) {
        # Stage the local source dir and expose $lib_src / $<lib>_src like
        # fetch.ps1 would; patch1.ps1 copies src/<lib>/native/* into it.
        $lib_src = Join-Path $build_src $lib_name
        if ($rebuild) { sremove $lib_src }
        mkdirs $lib_src
        Set-Variable -Name "${lib_name}_src" -Value $lib_src -Scope Global
    }
    else {
        if (!$repo_url.EndsWith('.git') -and $rebuild) {
            $sentry_file = Join-Path $build_src "$lib_name/_1kiss"
            if (Test-Path $sentry_file -PathType Leaf) {
                println "Deleting sentry file: $sentry_file"
                Remove-Item $sentry_file -Force
            }
        }
        . $fetch_script -uri $repo_url -ver $version -rev $revision -prefix $build_src -name $lib_name
    }

    # preprocess $build_conf.options
    if ($build_conf.options) {
        $build_conf.options = (eval $build_conf.options).Split(' ')
    }
    else {
        $build_conf.options = @()
    }

    if (!$is_host_target -and $build_conf.options_cross) {
        $build_conf.options += (eval $build_conf.options_cross).Split(' ')
    }
    
    if ($build_conf."options_$os_family") {
        $build_conf.options += (eval $build_conf."options_$os_family") -split ' '
    }
    if ($build_conf."options_$embed_family") {
        $build_conf.options += (eval $build_conf."options_$embed_family") -split ' '
    }
    if ($build_conf."options_$darwin_family") {
        $build_conf.options += (eval $build_conf."options_$darwin_family") -split ' '
    }
    if ($build_conf."options_$target_os") {
        $build_conf.options += (eval $build_conf."options_$target_os") -split ' '
    }

    # BoringSSL assembly can't be built in two of our configurations, so fall
    # back to its portable C implementations (we only consume the static libs,
    # so the perf cost is acceptable):
    #   * Windows: the CMake build uses enable_language(ASM) yet feeds the ASM
    #     target the full, un-filtered perlasm source list, including
    #     x86_64/apple .S files (e.g. aes-gcm-avx2-x86_64-apple.S). MSVC's
    #     assembler can't build those, the .obj is never produced and crypto.lib
    #     fails to link (LNK1181). Seen on both arm64 and x64 under VS2026.
    #   * wasm/wasm64: emcc is detected as the ASM compiler but rejects the
    #     GCC-style '-Wa,-g' passed for the x86_64 .S files (and x86_64 asm can't
    #     target wasm anyway).
    if ($lib_name -eq 'boringssl' -and ($is_win_family -or $is_wasm)) {
        $build_conf.options += '-DOPENSSL_NO_ASM=ON'
    }

    # curl on Windows links against our static nghttp2/nghttp3/ngtcp2 libs, but
    # their public headers default to __declspec(dllimport) on WIN32 unless the
    # matching *_STATICLIB macro is defined. Without it, curl's ngtcp2 backend
    # references __imp_* symbols that the static libs don't export, failing to
    # link libcurl.dll (LNK2019 / LNK1120). Define the macros so the headers
    # declare plain symbols. Passed as one token to survive the option split.
    if ($lib_name -eq 'curl' -and $is_win_family) {
        $build_conf.options += '-DCMAKE_C_FLAGS=/DNGHTTP2_STATICLIB /DNGHTTP3_STATICLIB /DNGTCP2_STATICLIB'
    }

    # ngtcp2 on WinRT/UWP: the WindowsStore project template enables /sdl, which
    # promotes C4703 (potentially uninitialized local pointer) to an error and
    # breaks ngtcp2's own sources (ngtcp2_rob.c, ngtcp2_acktr.c). Turn /sdl off;
    # we only consume the static lib.
    if ($lib_name -eq 'ngtcp2' -and $is_winrt) {
        $build_conf.options += '-DCMAKE_C_FLAGS=/sdl-'
    }

    println "Building $lib_name in $lib_src..."
    println "build_conf.options: $($build_conf.options)"
    # patch before build
    $patch_script = Join-Path $lib_root 'patch1.ps1'
    if (Test-Path $patch_script -PathType Leaf) {
        println "execute custom patch script '$patch_script'"
        &$patch_script $lib_src $build_conf.ver
    }
    else {
        if (!(Test-Path (Join-Path $lib_src '.git') -PathType Container)) {
            mkdirs (Join-Path $lib_src '.git/objects')
            mkdirs (Join-Path $lib_src '.git/refs')
            Write-Output "ref: refs/heads/master" >(Join-Path $lib_src '.git/HEAD')
        }
        $patches = Get-ChildItem (Split-Path $patch_script -Parent) -Filter '*.patch'
        foreach ($patch_file in $patches) {
            println "apply patch: $patch_file"
            git -C $lib_src apply --verbose --ignore-whitespace $patch_file
        }
    }

    if (!$is_local -and $build_conf.repo.EndsWith('.git') -and $rebuild) {
        # gclent manage submodules manually, so don't do git clean
        if (!$is_gn) {
          git -C $lib_src clean -dfx -e _1kiss
        }
    }

    $install_script = Join-Path $lib_root 'install1.ps1'
    $has_custom_install = (Test-Path $install_script)

    # build
    Push-Location $lib_src
    $install_dir = Join-Path $install_root $lib_name
    mkdirs $install_dir
    # Expose as $<lib>_install_dir for build.yml option substitution (eval). Use
    # forward slashes: these values get spliced into CMake command strings and,
    # on Windows, a backslash path like D:\a\...\ssl.lib makes downstream
    # check_symbol_exists() TryCompile projects choke on "Invalid character
    # escape '\a'". Forward slashes are valid on every platform CMake supports.
    Set-Variable -Name "${lib_name}_install_dir" -Value ($install_dir -replace '\\', '/') -Scope Global

    if (!$cb_target) {
        $cb_target = $build_conf.cb_target
    }
    if ($build_conf.cb_tool -ne 'custom') {
        $_config_options = $build_conf.options
        if ($build_conf.cb_tool -eq 'cmake') {
            if ($is_winrt) {
                $_config_options += "-DCMAKE_VS_WINDOWS_TARGET_PLATFORM_MIN_VERSION=$env:VS_DEPLOYMENT_TARGET"
            }
            
            $_config_options += "-DCMAKE_INSTALL_PREFIX=$install_dir"
            $evaluated_args = @()
            if ($cb_target) {
                $evaluated_args += '-t', $cb_target
            }
            if (!$has_custom_install) {
                $evaluated_args += '-i'
            }
            if (!$compiler_dumped) {
                $evaluated_args += '-dm'
                $compiler_dumped = $true
            }

            &$1k_script -p $target_os -a $target_cpu -O3 -xc $_config_options @forward_args @evaluated_args @args
        }
        elseif ($is_gn) {
            &$1k_script -p $target_os -a $target_cpu -xc $_config_options -xt 'gn' -t "$($cb_target)" @forward_args @args
        }
        else {
            throw "Unsupported cross build tool: $($build_conf.cb_tool)"
        }
    }
    else {
        $custom_build_script = Join-Path $lib_root 'build1.ps1'
        . $custom_build_script $target_os $target_cpu $install_dir @forward_args
    }
    Pop-Location
    if ($LASTEXITCODE) {
        throw "Build $lib_name failed"
    }

    # custom install step
    if ($has_custom_install) {
        &$install_script $install_dir $lib_src
    }
    # clean unnecessary files
    $clean_script = Join-Path $lib_root 'clean1.ps1'
    if (Test-Path $clean_script -PathType Leaf) {
        &$clean_script $install_dir
    }

    # install version file
    $version_file = Join-Path $lib_src '_1kiss'
    if (Test-Path $version_file -PathType Leaf) {
        Copy-Item $version_file $install_dir
    }

    # delete lib_src if run in github ci
    if ($is_gh_act) {
        println "Deleting $lib_src"
        Remove-Item $lib_src -Recurse -Force
    }
}

# Export INSTALL_ROOT for uploading
if ($is_gh_act) {
    Write-Output "install_path=$install_path" >> $env:GITHUB_ENV
}
