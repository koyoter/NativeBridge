# native/ — NativeBridge 库投喂目录

把打包好的 native 库拷到这里，C# 测试工程运行前会自动把它复制到测试输出目录。
文件按标准命名（P/Invoke 的 `LIBNAME = "NativeBridge"` 靠默认探测规则解析）：

| 平台 | 文件名 | 构建产物位置（本机） |
|------|--------|---------------------|
| Windows x64 | `NativeBridge.dll` | `install_win32_x64/nativebridge/bin/NativeBridge.dll` |
| Linux x64 | `libNativeBridge.so` | `install_linux_x64/nativebridge/lib/libNativeBridge.so` |
| macOS | `libNativeBridge.dylib` | `install_osx_x64/nativebridge/lib/libNativeBridge.dylib` |

- 目录内容已 gitignore，二进制不入库。
- 跑哪个平台就放哪个平台的库；同目录混放多个平台也没问题，测试运行器按当前系统挑选。
