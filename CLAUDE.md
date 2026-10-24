# NativeBridge 编码规则（Rust 侧 native/）

本库是 Unity 经 P/Invoke 加载的 cdylib：Rust 侧一次 panic = 宿主进程（编辑器/游戏）直接终止。
测试代码（tests.rs、#[cfg(test)]）不受第 1 条的 unwrap/expect 限制（native/clippy.toml 已豁免）。

1. 非 测试代码禁 panic：禁 unwrap()、expect()、panic!、unreachable!、todo!、unimplemented!。
   错误经 ?、ok_or(..)?、let .. else、unwrap_or* 传播（回退仅在真正确时用，不得静默损坏数据）；
   FFI 导出返回错误码/错误回调。唯一例外：可证不失败的位点用
   `#[allow(clippy::expect_used)]` + `.expect("为什么不会失败")` 标注。
   强制手段：native/Cargo.toml `[lints.clippy]` deny + CI `cargo clippy --all-targets -- -D warnings`
   （2026-10-24 起 -D warnings 生效：21 条存量默认组警告已清零）。
2. unsafe 纪律（本库即 FFI，无法禁用）：unsafe 限定在 native/src/modules/ 下（curlw/dlmgr/vfs/glue）；
   每个 unsafe 块必须有 SAFETY: 注释，`undocumented_unsafe_blocks = deny` 拦新增（2026-10-24 全库补齐）；
   唯一例外：lib.rs 的 nativebridge_version（文档化例外，见其 # Safety 注记）。
   未启用的终态目标 `unsafe_op_in_unsafe_fn = deny`：约 272 处 unsafe-fn 体内裸操作
   （93% 在 curlw/mod.rs 镜像层），启用前需先决断镜像层是否按 bindings.rs 先例豁免。
3. 递归必须带深度限制或 seen-set（当前代码库无递归；此条防新增路径/结构遍历裸奔）。
4. Mutex/Condvar 一律 `.unwrap_or_else(PoisonError::into_inner)`（全库已采用）。
   `panic = "abort"` 下锁不会中毒，此写法是为 lint 合规与 profile 无关性；
   后台线程 join 用 `let _ =`（abort 下 join 不会 Err）。
5. `panic = "abort"` 是有意设计（dlmgr/mod.rs、vfs/mod.rs 头注释）：FFI 边界不做 catch_unwind，
   防 panic 逃逸的手段是第 1 条本身。若未来改 unwind：每个导出面包 catch_unwind 返回错误码，
   并保留第 4 条写法。
6. 崩溃修复必须配一个修复前会 panic 的合成回归测试；std 无法注入失败的调用（如 thread::spawn）
   除外，在 PR 描述里说明理由。
