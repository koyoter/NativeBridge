# NativeBridge

Unity / C# 用的自包含原生库：静态链接 curl HTTP/1.1 + HTTP/2 + HTTP/3 全家桶
（curl + BoringSSL + nghttp2/nghttp3/ngtcp2 + zlib），每个平台输出**单一**动态库
（iOS/tvOS 为静态库），暴露 P/Invoke 友好的 C ABI（`curlw_*` / `vfs_*` / `dlmgr_*`），
配套 C# 封装与 NUnit 测试。从 [1kiss](https://github.com/simdsoft/1kiss) 工程独立而来，
构建机器（`1k/`）vendored 自上游。

## Layout

```
native/    Rust crate = 唯一权威实现（curlw / vfs / dlmgr 模块）+ 1k 本地模块配置
csharp/
  NativeBridge/        C# 封装（Curlw.cs / Vfs.cs / DownloadManager.cs + asmdef + csproj）
  NativeBridge.Tests/  NUnit 测试（dotnet test）
deps/      6 个 C 依赖库的 1k 构建配置（源码由构建时自动拉取，不入库）
1k/        跨平台构建机器（工具链安装、NDK/Xcode/MSVC 适配），vendored
pack/      dist1.sh — 全平台产物 -> Unity 包组装
build.ps1  构建入口；pack.ps1 本地一键（当前宿主平台）
docs/      vfs/dlmgr 设计文档与 tickets
```

## Build

```powershell
# 本地一键（当前宿主平台 win32/osx/linux，x64）：
./pack.ps1
# 指定平台：
./pack.ps1 -platform win32 -arch arm64
```

CI（`.github/workflows/build.yml`）：push `v*` tag 触发全平台构建（win32 x64/arm64、
linux x64/arm64、android×4、osx x64/arm64、ios/tvos device+sim），并在 `dist` job
组装 `NativeBridge-<ver>.zip` 发布到该 tag 的 release。包内含：

- `Plugins/*`（各平台 native 库，全部带显式 PluginImporter 平台/CPU 设置的 .meta）
- `Runtime/*.cs` + `.asmdef`（同样带确定性 GUID 的 .meta，升级包引用不丢）
- `package.json`：同时是标准 UPM 包，可在 Package Manager 里 "Add package from disk" 安装

.meta 的 GUID 由包内相对路径哈希生成（确定性），重复打包/升级版本 GUID 不变；
xcframework 按 Unity 约定作为单一插件资产（内部不生成 meta）。

## Versioning

- 版本唯一来源：`native/build.yml` 的 `ver:`。`native/patch1.ps1` staging 时自动
  同步 Cargo.toml（`nativebridge_version()` 返回的运行时版本串来自 `CARGO_PKG_VERSION`）。
- 发版流程：改 `native/build.yml` `ver:` → commit → `git tag v<ver>` → push tag。
  CI 会校验 tag 与 `ver:` 一致，不一致直接失败。
- ABI 常量在 Rust 模块内：`CURLW_ABI_VERSION` / `VFS_ABI_VERSION` / `DLMGR_ABI_VERSION`，
  签名/布局变更时 bump；纯新增导出不 bump。
- 锁定组件版本：curl 8.21.0 / boringssl 0.20260803.0 / nghttp2 1.70.0 /
  nghttp3 1.18.0 / ngtcp2 1.25.0 / zlib 1.3.2（见 `deps/*/build.yml`）。

## Tests

```bash
# 先把对应平台的 native 库放进 csharp/NativeBridge.Tests/native/Plugins/
# （目录结构与 Unity 包一致，见其中的 README），然后：
dotnet test csharp/NativeBridge.Tests
```

## Unity 侧

| Unity 平台 | 产物 | `DllImport` 名 |
| --- | --- | --- |
| Windows x64 / arm64 | `Plugins/Windows/*/NativeBridge.dll` | `NativeBridge` |
| Android 4 ABI | `Plugins/Android/*/libNativeBridge.so` | `NativeBridge` |
| iOS / tvOS（device+sim） | `Plugins/{iOS,tvOS}/NativeBridge.xcframework` | `__Internal` |
| macOS（universal） | `Plugins/macOS/NativeBridge.dylib` | `NativeBridge` |
| Linux x64 / arm64 | `Plugins/Linux/*/libNativeBridge.so` | `NativeBridge` |

> Native callbacks（`CurlwWriteDataDelegate` 等）在 IL2CPP/AOT 下必须是 `static`
> 方法并标 `[MonoPInvokeCallback]`。`curlw_perform` 会阻塞，请在非主线程调用。

## License

Apache-2.0（见 LICENSE）。`1k/` 构建机器来自 [1kiss](https://github.com/simdsoft/1kiss)；
各 C 依赖库遵循其各自的开源协议。
