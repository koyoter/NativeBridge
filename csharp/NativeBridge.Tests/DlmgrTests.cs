//
// DlmgrTests.cs — DlmgrDLL / DlVfs 对真 native 库的下载管理测试。
//
// 真实走 curl → 127.0.0.1 HttpTestServer，覆盖：文件 sink 下载回环、
// dual-URL 回退语义（range 源不支持 Range → 切 full 源）、crc 闸失败、
// 运行中取消、断连续传与退避计时、限速三态、10Hz 节流与字节对账（T5 验收矩阵）、
// 以及 dlmgr + VFS sink 的端到端胶水（T6 场景）。
//
// 已知 e2e 缺口：真 TLS + CA blob 握手链路无人覆盖——HttpTestServer 为纯 HTTP，
// dlmgr_set_ca_blob 仅参数级验证；"喂 CA 后真 HTTPS 下载成功"需真 URL 或本地
// TLS 服务器实测（审查报告 P1-1 的遗留建议，改码前后均未执行过）。
//
using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Threading;
using NUnit.Framework;

namespace NativeBridgeF.Tests
{
    [TestFixture]
    public class DlmgrTests
    {
        private const int TerminalMs = 60_000;

        private HttpTestServer _server;
        private IntPtr _mgr;

        // 每个 taskId 最近一次上报的状态（回调在 native 线程触发）
        private sealed class Record
        {
            public int State = -1;
            public ulong Done, Total, Bps;
            public int Err = -1;
        }
        private readonly ConcurrentDictionary<ulong, Record> _records = new ConcurrentDictionary<ulong, Record>();

        // 终态 Done 到达顺序（单 worker 下 == 派发顺序；去重防终态重复上报）
        private readonly object _orderLock = new object();
        private readonly List<ulong> _doneOrder = new List<ulong>();
        private readonly HashSet<ulong> _doneSeen = new HashSet<ulong>();

        private List<ulong> DoneOrderSnapshot()
        {
            lock (_orderLock) { return new List<ulong>(_doneOrder); }
        }

        // 全局回调快照（单一 native reporter 线程写，Interlocked 保证可见性）
        private long _gActive, _gDone, _gFailed;

        private void OnGlobal(IntPtr user, uint active, uint doneCount, uint failedCount,
                              ulong bytesDone, ulong bytesTotal, ulong bps)
        {
            System.Threading.Interlocked.Exchange(ref _gActive, active);
            System.Threading.Interlocked.Exchange(ref _gDone, doneCount);
            System.Threading.Interlocked.Exchange(ref _gFailed, failedCount);
        }

        [OneTimeSetUp]
        public void StartServer()
        {
            _server = new HttpTestServer();
        }

        [OneTimeTearDown]
        public void StopServer()
        {
            _server.Dispose();
        }

        // 任务回调：记录状态 + 终态到达顺序（native 存裸指针，DlmgrDLL 侧已 keep-alive 本委托）
        private void OnTask(IntPtr user, ulong taskId, int state, ulong done, ulong total, ulong bps, int err)
        {
            var r = new Record { State = state, Done = done, Total = total, Bps = bps, Err = err };
            _records[taskId] = r;
            if (state == (int)DlmgrTaskState.Done)
            {
                lock (_orderLock)
                {
                    if (_doneSeen.Add(taskId)) _doneOrder.Add(taskId);
                }
            }
        }

        [SetUp]
        public void CreateManager()
        {
            _records.Clear();
            lock (_orderLock) { _doneOrder.Clear(); _doneSeen.Clear(); }
            _gActive = _gDone = _gFailed = 0;
            _mgr = DlmgrDLL.dlmgr_create(2, 0, 2); // 2 worker / 不限速 / 重试 2
            Assert.That(_mgr, Is.Not.EqualTo(IntPtr.Zero), "dlmgr_create 失败");
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(_mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_set_global_callback(_mgr, OnGlobal, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(_mgr), Is.EqualTo(0));
        }

        [TearDown]
        public void ShutdownManager()
        {
            // shutdown 消费句柄：cancel all + join，不会死锁
            DlmgrDLL.dlmgr_shutdown(_mgr);
            _mgr = IntPtr.Zero;
        }

        // --- helpers -------------------------------------------------------------

        private ulong Enqueue(string rangeUrl, string fullUrl, string name, ulong size, uint crc, ulong sink)
        {
            int rc = DlmgrDLL.dlmgr_enqueue(_mgr, rangeUrl, fullUrl, name, size, crc, 0, 0, sink, out ulong id);
            Assert.That(rc, Is.EqualTo(0), "dlmgr_enqueue 失败");
            return id;
        }

        private Record WaitTerminal(ulong id, int timeoutMs)
        {
            var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
            while (DateTime.UtcNow < deadline)
            {
                if (_records.TryGetValue(id, out Record r) && r.State >= (int)DlmgrTaskState.Done)
                {
                    return r;
                }
                Thread.Sleep(50);
            }
            _records.TryGetValue(id, out Record last);
            Assert.Fail("任务 " + id + " 未在限时内到达终态，最后状态: " +
                        (last == null ? "无上报" : last.State + " err=" + last.Err));
            return null;
        }

        private Record WaitState(ulong id, int state, int timeoutMs)
        {
            var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
            while (DateTime.UtcNow < deadline)
            {
                if (_records.TryGetValue(id, out Record r) && r.State == state)
                {
                    return r;
                }
                Thread.Sleep(50);
            }
            Assert.Fail("任务 " + id + " 未在限时内到达状态 " + state);
            return null;
        }

        /// <summary>等待一组任务全部到达终态（Done/Failed/Canceled）。</summary>
        private void WaitAllTerminal(IEnumerable<ulong> ids, int timeoutMs)
        {
            var pending = new HashSet<ulong>(ids);
            var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
            while (DateTime.UtcNow < deadline)
            {
                pending.RemoveWhere(id =>
                    _records.TryGetValue(id, out Record r) && r.State >= (int)DlmgrTaskState.Done);
                if (pending.Count == 0) return;
                Thread.Sleep(50);
            }
            Assert.Fail("任务未在限时内全部到达终态，剩余: " + string.Join(",", pending));
        }

        /// <summary>等待全局计数器收敛到期望值（终态上报与全局 tick 之间有滞后）。</summary>
        private void WaitGlobalCounters(long done, long failed)
        {
            var deadline = DateTime.UtcNow.AddSeconds(10);
            while (DateTime.UtcNow < deadline)
            {
                if (System.Threading.Interlocked.Read(ref _gDone) >= done &&
                    System.Threading.Interlocked.Read(ref _gFailed) == failed &&
                    System.Threading.Interlocked.Read(ref _gActive) == 0)
                {
                    return;
                }
                Thread.Sleep(100);
            }
            Assert.Fail("全局计数器未收敛: done=" + System.Threading.Interlocked.Read(ref _gDone) +
                        " failed=" + System.Threading.Interlocked.Read(ref _gFailed) +
                        " active=" + System.Threading.Interlocked.Read(ref _gActive) +
                        "，期望 done>=" + done + " failed==" + failed + " active==0");
        }

        // --- tests -----------------------------------------------------------------

        /// <summary>
        /// 测试重点：dlmgr ABI 版本守卫恒为 1，与 DownloadManager.cs 锁步。
        /// </summary>
        [Test]
        public void AbiVersion_Is1()
        {
            Assert.That(DlmgrDLL.dlmgr_abi_version(), Is.EqualTo(1));
        }

        /// <summary>
        /// Task/global callbacks must reject both self- and cross-manager shutdown,
        /// including the final callback while external shutdown has taken handles.
        /// Callback bodies use only the non-throwing try API; assertions stay here.
        /// </summary>
        [TestCase(false)]
        [TestCase(true)]
        public void ShutdownTry_ReporterCallbackRejects_ThenExternalShutdownSucceeds(bool globalCallback)
        {
            string dir = TestPaths.NewDir();
            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 0, 0);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            ulong sink = 0;
            int attempts = 0, unexpectedResult = 0;
            Action attemptShutdown = () =>
            {
                int self = DlmgrDLL.dlmgr_shutdown_try(mgr);
                int other = DlmgrDLL.dlmgr_shutdown_try(_mgr);
                if (self != -1 || other != -1) Interlocked.Exchange(ref unexpectedResult, 1);
                Interlocked.Increment(ref attempts);
            };
            try
            {
                Assert.That(DlmgrDLL.dlmgr_pause(mgr), Is.EqualTo(0));
                if (globalCallback)
                {
                    Assert.That(DlmgrDLL.dlmgr_set_global_callback(mgr,
                        (user, active, done, failed, bytesDone, bytesTotal, bps) => attemptShutdown(),
                        IntPtr.Zero), Is.EqualTo(0));
                }
                else
                {
                    Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr,
                        (user, id, state, done, total, bps, err) => attemptShutdown(),
                        IntPtr.Zero), Is.EqualTo(0));
                }
                sink = DlmgrDLL.dlmgr_sink_create_file(mgr, Path.Combine(dir, "pending.bin"), 0);
                Assert.That(sink, Is.Not.EqualTo(0UL));
                EnqueueLocal(mgr, _server.Add("shutdown-pending.bin", new byte[0]),
                    "pending.bin", new byte[0], sink);
                Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));
                Assert.That(SpinWait.SpinUntil(() => Volatile.Read(ref attempts) > 0, 10_000), Is.True);
                Assert.That(Volatile.Read(ref unexpectedResult), Is.EqualTo(0));
                // Both handles still work after the callback's rejected attempts.
                Assert.That(DlmgrDLL.dlmgr_pause(mgr), Is.EqualTo(0));
                Assert.That(DlmgrDLL.dlmgr_set_global_speed(_mgr, 0), Is.EqualTo(0));
                GC.Collect();
                GC.WaitForPendingFinalizers();
                GC.Collect();

                int beforeShutdown = Volatile.Read(ref attempts);
                int result = DlmgrDLL.dlmgr_shutdown_try(mgr);
                if (result == 0) mgr = IntPtr.Zero;
                Assert.That(result, Is.EqualTo(0));
                Assert.That(Volatile.Read(ref attempts), Is.GreaterThan(beforeShutdown),
                    "Final callback must still be rooted and reject nested shutdown during join");
                Assert.That(Volatile.Read(ref unexpectedResult), Is.EqualTo(0));
            }
            finally
            {
                // Join before releasing even if an assertion/wait failed.
                if (mgr != IntPtr.Zero) DlmgrDLL.dlmgr_shutdown(mgr);
                if (sink != 0) DlmgrDLL.dlmgr_sink_release(sink);
                try { Directory.Delete(dir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：文件 sink 下载全链路——range 源（206 + Content-Range）
        /// 正常下载到本地文件：终态 Done、done/total 等于任务 size、
        /// 文件字节逐字节一致。sink 只能在终态后释放。
        /// </summary>
        [Test]
        public void DownloadToFileSink_Roundtrip()
        {
            byte[] data = TestPaths.Content(1024 * 1024, 42);
            string url = _server.Add("ok.bin", data);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPath, (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL), "sink_create_file 失败");
            Record r;
            try
            {
                ulong id = Enqueue(url, null, "ok.bin", (ulong)data.Length, TestPaths.Crc32(data), sink);
                r = WaitTerminal(id, TerminalMs);
            }
            finally
            {
                // 纪律：终态后才能释放 sink；sink 持有文件句柄，释放前文件不可读
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            try
            {
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "应为 Done, err=" + r.Err);
                Assert.That(r.Done, Is.EqualTo((ulong)data.Length));
                Assert.That(r.Total, Is.EqualTo((ulong)data.Length));
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(data));
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：dlmgr_set_http_version 参数校验——合法透传值返回 0，
        /// 非法值与无效 mgr 返回 -1（不产生可观察副作用，纯 API 面）。
        /// </summary>
        [Test]
        public void SetHttpVersion_ArgValidation()
        {
            foreach (var v in new[] { CurlHttpVersion.None, CurlHttpVersion.V10,
                                      CurlHttpVersion.V11, CurlHttpVersion.V2,
                                      CurlHttpVersion.V2Tls, CurlHttpVersion.V2PriorKnowledge,
                                      CurlHttpVersion.V3, CurlHttpVersion.V3Only })
            {
                Assert.That(DlmgrDLL.dlmgr_set_http_version(_mgr, v), Is.EqualTo(0), "v=" + v);
            }
            Assert.That(DlmgrDLL.dlmgr_set_http_version(_mgr, (CurlHttpVersion)7), Is.EqualTo(-1));
            Assert.That(DlmgrDLL.dlmgr_set_http_version(_mgr, (CurlHttpVersion)(-1)), Is.EqualTo(-1));
            Assert.That(DlmgrDLL.dlmgr_set_http_version(IntPtr.Zero, CurlHttpVersion.V11), Is.EqualTo(-1));
        }

        /// <summary>
        /// 测试重点：版本须在 Start 前设置（worker 建柄时快照）——夹具的 _mgr 已
        /// 启动，这里自建管理器走完整 Create → SetHttpVersion → Start → 下载链，
        /// 证明 setopt 路径可用且不破坏普通 HTTP 下载。
        /// </summary>
        [Test]
        public void SetHttpVersion_V11_DownloadStillWorks()
        {
            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 0, 2);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero), "dlmgr_create 失败");
            try
            {
                Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
                Assert.That(DlmgrDLL.dlmgr_set_http_version(mgr, CurlHttpVersion.V11), Is.EqualTo(0));
                Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));
                var saved = _mgr; // 复用 DownloadToFileSink_Roundtrip 的完整断言链
                _mgr = mgr;
                try { DownloadToFileSink_Roundtrip(); }
                finally { _mgr = saved; }
            }
            finally
            {
                DlmgrDLL.dlmgr_shutdown(mgr);
            }
        }

        /// <summary>
        /// 测试重点：dlmgr_set_ca_blob 参数校验——合法 PEM bytes 接受、null 清空、
        /// 无效 mgr 返回 -1。真 TLS 握手效果不在本测试范围（见文件头缺口注记）。
        /// </summary>
        [Test]
        public void SetCaBlob_ArgValidation()
        {
            byte[] pem = System.Text.Encoding.UTF8.GetBytes(
                "-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n");
            Assert.That(DlmgrDLL.dlmgr_set_ca_blob(_mgr, pem), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_set_ca_blob(_mgr, null), Is.EqualTo(0)); // 清空
            Assert.That(DlmgrDLL.dlmgr_set_ca_blob(IntPtr.Zero, pem), Is.EqualTo(-1));
        }

        /// <summary>
        /// 测试重点：dual-URL 回退语义——range 源对 "Range: bytes=0-" 回 200
        /// （不支持 Range）时，任务必须一次性切到 full_url 从零下载；
        /// 最终文件内容必须是 full 源的（两源内容不同以区分下载自谁）。
        /// </summary>
        [Test]
        public void RangeUnsupported_FallsBackToFullUrl()
        {
            byte[] rangeContent = TestPaths.Content(256 * 1024, 1);
            byte[] fullContent = TestPaths.Content(256 * 1024, 2);
            string rangeUrl = _server.Add("no-range.bin", rangeContent, force200: true);
            string fullUrl = _server.Add("full.bin", fullContent);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPath, (ulong)fullContent.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            Record r;
            try
            {
                ulong id = Enqueue(rangeUrl, fullUrl, "fallback.bin",
                                   (ulong)fullContent.Length, TestPaths.Crc32(fullContent), sink);
                r = WaitTerminal(id, TerminalMs);
            }
            finally
            {
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            try
            {
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "回退后应 Done, err=" + r.Err);
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(fullContent),
                            "回退后文件必须来自 full_url");
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：crc 闸——两源内容都与预期 crc 不符时，range 源触发
        /// 回退、full 源耗尽重试预算，任务最终 Failed 且错误码为
        /// CrcMismatch(3)，文件不得被误标为完成。
        /// </summary>
        [Test]
        public void CrcMismatch_TaskFails()
        {
            byte[] wrong = TestPaths.Content(64 * 1024, 9);       // 服务器实际内容
            byte[] right = TestPaths.Content(64 * 1024, 8);       // 任务声明的期望内容
            string url = _server.Add("bad.bin", wrong);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPath, (ulong)wrong.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                ulong id = Enqueue(url, url, "crc-fail.bin", (ulong)wrong.Length,
                                   TestPaths.Crc32(right), sink);
                Record r = WaitTerminal(id, TerminalMs);

                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Failed), "应为 Failed");
                Assert.That(r.Err, Is.EqualTo((int)DlmgrTaskError.CrcMismatch), "错误码应为 CrcMismatch");
            }
            finally
            {
                DlmgrDLL.dlmgr_sink_release(sink);
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：运行中取消——慢速源传输期间 dlmgr_cancel，任务必须
        /// 尽快进入 Canceled(5) 终态且 err=Canceled(6)，不吞掉取消信号。
        /// </summary>
        [Test]
        public void Cancel_RunningTask()
        {
            byte[] data = TestPaths.Content(2 * 1024 * 1024, 7);
            string url = _server.Add("slow.bin", data, slowMs: 25, chunk: 16 * 1024);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPath, (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                ulong id = Enqueue(url, null, "cancel.bin", (ulong)data.Length, TestPaths.Crc32(data), sink);
                WaitState(id, (int)DlmgrTaskState.Running, 15_000);

                Assert.That(DlmgrDLL.dlmgr_cancel(_mgr, id), Is.EqualTo(0));
                Record r = WaitTerminal(id, 30_000);
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Canceled), "应为 Canceled");
                Assert.That(r.Err, Is.EqualTo((int)DlmgrTaskError.Canceled));
            }
            finally
            {
                DlmgrDLL.dlmgr_sink_release(sink);
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        // --- T5 验收矩阵：断连续传 / 退避计时 / 限速 / 10Hz 节流 / 字节对账 --------------
        // native 侧契约依据：worker.rs transition() 三道闸（非 206 / Content-Range 总长 /
        // crc）+ net_retry 退避 1s/2s/4s（第 3 次起封顶 4s）+ rate.rs 双令牌桶
        // （rate 实为字节/秒，桶初始满 = 1 秒等量突发）+ report.rs 10Hz 节流
        // （同一轮先逐任务后全局）。时间断言一律 ±40% 宽容差，防 CI 慢机误报。

        /// <summary>
        /// 测试重点：断连续传（T5）——首请求 Range: bytes=0- 传输中途被掐断，
        /// 重试必须带非零断点 Range: bytes=off- 续传；期间进度单调不回头、不超
        /// 总长、终值精确（断连续传不重复计数）；最终文件逐字节拼回完整数据。
        /// </summary>
        [Test]
        public void MidTransferDrop_ResumesWithRange_NoDoubleCount()
        {
            byte[] data = TestPaths.Content(1024 * 1024, 71);
            string url = _server.Add("resume.bin", data, dropRequests: 1);
            var (ranges, _) = _server.Log("resume.bin");
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            // 本地 manager + 记历史的回调（fixture 的 Record 只存最新值，对账需要全序列）
            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 0, 2);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            var samples = new ConcurrentQueue<(int state, ulong done)>();
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr,
                (user, id, state, done, total, bps, err) =>
                {
                    samples.Enqueue((state, done));
                    OnTask(user, id, state, done, total, bps, err);
                }, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(mgr, outPath, (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                ulong id = EnqueueLocal(mgr, url, "resume.bin", data, sink);
                Record r = WaitTerminal(id, TerminalMs);
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "续传后应 Done, err=" + r.Err);
                Assert.That(r.Done, Is.EqualTo((ulong)data.Length));

                // 服务器视角：恰好两个请求，首探测 bytes=0-，重试带非零断点
                var seen = new List<string>(ranges);
                Assert.That(seen.Count, Is.EqualTo(2), "掐断一次后应恰好两个请求");
                Assert.That(seen[0], Is.EqualTo("bytes=0-"), "首请求应探测 bytes=0-");
                Assert.That(seen[1], Does.StartWith("bytes="), "重试应带 Range 头续传");
                Assert.That(seen[1], Is.Not.EqualTo("bytes=0-"), "断点必须非零（不得从头重下）");

                // 字节对账：进度单调不回头、不超总长、终值精确 == size
                ulong prev = 0;
                foreach (var s in samples)
                {
                    Assert.That(s.done, Is.GreaterThanOrEqualTo(prev), "进度不得回退（重复计数）");
                    Assert.That(s.done, Is.LessThanOrEqualTo((ulong)data.Length), "进度不得超总长");
                    prev = s.done;
                }
                Assert.That(prev, Is.EqualTo((ulong)data.Length), "终值必须精确等于 size");
            }
            finally
            {
                DlmgrDLL.dlmgr_shutdown(mgr); // 先 join 再释放 sink（消费句柄）
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            // 文件断言在 sink 释放后：Windows 下 sink 句柄锁着文件
            try
            {
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(data), "续传拼回的数据必须完整");
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：重试退避（T5）——range 源回 200 触发不耗预算的兜底切 full_url；
        /// full_url 连续掐断 4 次（初始 + 3 重试）耗尽预算 → Failed(Network)。
        /// 服务器命中时刻实测退避间隔 ≈ 1s/2s/4s（±40% 宽容差）。
        /// </summary>
        [Test]
        public void FullUrlFailures_Backoff1s2s4s_ThenFailed()
        {
            byte[] full = TestPaths.Content(16 * 1024, 72);
            string rangeUrl = _server.Add("bk-range.bin", new byte[16], force200: true);
            string fullUrl = _server.Add("bk-full.bin", full, dropRequests: 4);
            var (_, hits) = _server.Log("bk-full.bin");
            string outDir = TestPaths.NewDir();

            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 0, 3); // 重试预算 3 → 4 次尝试 3 段退避
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(mgr, Path.Combine(outDir, "out.bin"),
                                                         (ulong)full.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                int rc = DlmgrDLL.dlmgr_enqueue(mgr, rangeUrl, fullUrl, "bk.bin",
                                                (ulong)full.Length, TestPaths.Crc32(full),
                                                0, 0, sink, out ulong id);
                Assert.That(rc, Is.EqualTo(0), "enqueue 失败");
                Record r = WaitTerminal(id, TerminalMs);
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Failed), "耗尽预算应 Failed");
                Assert.That(r.Err, Is.EqualTo((int)DlmgrTaskError.Network), "错误码应为 Network");

                var ts = new List<long>(hits);
                Assert.That(ts.Count, Is.EqualTo(4), "初始 + 3 次重试应恰好 4 个请求");
                double[] expect = { 1.0, 2.0, 4.0 };
                for (int i = 1; i < ts.Count; i++)
                {
                    double gap = (ts[i] - ts[i - 1]) / (double)Stopwatch.Frequency;
                    Assert.That(gap, Is.InRange(expect[i - 1] * 0.6, expect[i - 1] * 1.4),
                                "第 " + i + " 段退避间隔实测 " + gap.ToString("F2") + "s");
                }
            }
            finally
            {
                DlmgrDLL.dlmgr_shutdown(mgr);
                DlmgrDLL.dlmgr_sink_release(sink);
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：crc 闸兜底（T5）——range 源长度正确但内容不符（流式 crc ≠
        /// 预期），源不可信一次性切 full_url；full 源正确 → Done，文件必须来自
        /// full 源（两源内容不同以区分下载自谁）。
        /// </summary>
        [Test]
        public void RangeCrcWrong_FallsBackToFullUrl_Succeeds()
        {
            byte[] wrong = TestPaths.Content(128 * 1024, 3);
            byte[] right = TestPaths.Content(128 * 1024, 4);
            string rangeUrl = _server.Add("crc-range.bin", wrong);
            string fullUrl = _server.Add("crc-full.bin", right);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPath, (ulong)right.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            Record r;
            try
            {
                ulong id = Enqueue(rangeUrl, fullUrl, "crc-fb.bin", (ulong)right.Length,
                                   TestPaths.Crc32(right), sink);
                r = WaitTerminal(id, TerminalMs);
            }
            finally
            {
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            try
            {
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "兜底后应 Done, err=" + r.Err);
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(right), "文件必须来自 full 源");
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：Content-Range 总长闸兜底（T5）——range 源总长 ≠ 任务声明
        /// size（越界风险）→ 一次性切 full_url；full 源长度正确 → Done，
        /// 文件必须来自 full 源。
        /// </summary>
        [Test]
        public void RangeSizeMismatch_FallsBackToFullUrl_Succeeds()
        {
            byte[] shortBody = TestPaths.Content(64 * 1024, 5);  // range 源：只有声明的一半
            byte[] right = TestPaths.Content(128 * 1024, 6);     // full 源：与声明一致
            string rangeUrl = _server.Add("sz-range.bin", shortBody);
            string fullUrl = _server.Add("sz-full.bin", right);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPath, (ulong)right.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            Record r;
            try
            {
                ulong id = Enqueue(rangeUrl, fullUrl, "sz-fb.bin", (ulong)right.Length,
                                   TestPaths.Crc32(right), sink);
                r = WaitTerminal(id, TerminalMs);
            }
            finally
            {
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            try
            {
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "兜底后应 Done, err=" + r.Err);
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(right), "文件必须来自 full 源");
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：总长闸耗尽（T5）——range 源总长不符触发兜底后，full 源总长
        /// 也不符：full 阶段任何失败耗 retry，耗尽后 Failed 且错误码为
        /// SizeMismatch(2)。
        /// </summary>
        [Test]
        public void SizeMismatchBothSources_TaskFails()
        {
            byte[] shortRange = TestPaths.Content(64 * 1024, 7);
            byte[] shortFull = TestPaths.Content(64 * 1024, 8);
            byte[] expected = TestPaths.Content(128 * 1024, 9); // 任务声明的期望内容
            string rangeUrl = _server.Add("szf-range.bin", shortRange);
            string fullUrl = _server.Add("szf-full.bin", shortFull);
            string outDir = TestPaths.NewDir();

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(_mgr, Path.Combine(outDir, "out.bin"),
                                                         (ulong)expected.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                ulong id = Enqueue(rangeUrl, fullUrl, "sz-fail.bin", (ulong)expected.Length,
                                   TestPaths.Crc32(expected), sink);
                Record r = WaitTerminal(id, TerminalMs);
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Failed), "应为 Failed");
                Assert.That(r.Err, Is.EqualTo((int)DlmgrTaskError.SizeMismatch),
                            "错误码应为 SizeMismatch");
            }
            finally
            {
                DlmgrDLL.dlmgr_sink_release(sink);
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：全局限速（T5）——全局 1MB/s 单任务不限：6MB 实测 ≈5s
        /// （令牌桶初始满 = 1 秒等量突发，扣除后 (6-1)MB / 1MBps）。带宽
        /// [3s, 7s]：上限防 CI 慢机误报，下限证明限速真实生效（不限速时
        /// 本机回环远小于 1s）。
        /// </summary>
        [Test]
        public void RateLimit_GlobalBytesPerSecond()
        {
            byte[] data = TestPaths.Content(6 * 1024 * 1024, 11);
            string url = _server.Add("rate-g.bin", data);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 1024 * 1024, 0);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));
            ulong sink = DlmgrDLL.dlmgr_sink_create_file(mgr, outPath, (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                var sw = Stopwatch.StartNew();
                ulong id = EnqueueLocal(mgr, url, "rate-g.bin", data, sink);
                Record r = WaitTerminal(id, TerminalMs);
                sw.Stop();
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "限速下载应 Done, err=" + r.Err);
                double sec = sw.Elapsed.TotalSeconds;
                Assert.That(sec, Is.InRange(3.0, 7.0),
                            "6MB @1MB/s 应 ≈5s（含 1MB 突发），实测 " + sec.ToString("F2") + "s");
            }
            finally
            {
                DlmgrDLL.dlmgr_shutdown(mgr);
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            // 文件断言在 sink 释放后：Windows 下 sink 句柄锁着文件
            try
            {
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(data));
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：单任务限速取 min（T5）——全局 1MB/s + 单任务 512KB/s →
        /// 实测 ≈512KB/s：4MB ≈7s（扣 512KB 突发后 (4-0.5)MB / 0.5MBps）。
        /// 带宽 [4.2s, 9.8s] 下限同时证明单任务桶压过全局桶（仅全局时 ≈3s）。
        /// </summary>
        [Test]
        public void RateLimit_TaskLimitTakesMinWithGlobal()
        {
            byte[] data = TestPaths.Content(4 * 1024 * 1024, 12);
            string url = _server.Add("rate-t.bin", data);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 1024 * 1024, 0);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));
            ulong sink = DlmgrDLL.dlmgr_sink_create_file(mgr, outPath, (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                var sw = Stopwatch.StartNew();
                int rc = DlmgrDLL.dlmgr_enqueue(mgr, url, null, "rate-t.bin", (ulong)data.Length,
                                                TestPaths.Crc32(data), 0, 512UL * 1024, sink,
                                                out ulong id);
                Assert.That(rc, Is.EqualTo(0), "enqueue 失败");
                Record r = WaitTerminal(id, TerminalMs);
                sw.Stop();
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "限速下载应 Done, err=" + r.Err);
                double sec = sw.Elapsed.TotalSeconds;
                Assert.That(sec, Is.InRange(4.2, 9.8),
                            "4MB @512KB/s 应 ≈7s（含 512KB 突发），实测 " + sec.ToString("F2") + "s");
            }
            finally
            {
                DlmgrDLL.dlmgr_shutdown(mgr);
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            // 文件断言在 sink 释放后：Windows 下 sink 句柄锁着文件
            try
            {
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(data));
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：动态解除限速（T5）——全局 512KB/s 下 6MB 任务跑到中途，
        /// set_global_speed(0) 立即生效恢复全速：总耗时远小于全程限速的
        /// ≈11s（带宽 <5s），且文件完整正确。
        /// </summary>
        [Test]
        public void RateLimit_SetGlobalZero_ResumesFullSpeed()
        {
            byte[] data = TestPaths.Content(6 * 1024 * 1024, 13);
            string url = _server.Add("rate-0.bin", data);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 512UL * 1024, 0);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));
            ulong sink = DlmgrDLL.dlmgr_sink_create_file(mgr, outPath, (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                var sw = Stopwatch.StartNew();
                ulong id = EnqueueLocal(mgr, url, "rate-0.bin", data, sink);
                WaitState(id, (int)DlmgrTaskState.Running, 15_000);
                Thread.Sleep(1500); // 限速下载确实进行中（≈512KB 突发 + 0.5s 配额）
                Assert.That(DlmgrDLL.dlmgr_set_global_speed(mgr, 0), Is.EqualTo(0));
                Record r = WaitTerminal(id, TerminalMs);
                sw.Stop();
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "解除限速后应 Done, err=" + r.Err);
                double sec = sw.Elapsed.TotalSeconds;
                Assert.That(sec, Is.InRange(1.5, 5.0),
                            "中途解除限速应 <5s（全程限速 ≈11s），实测 " + sec.ToString("F2") + "s");
            }
            finally
            {
                DlmgrDLL.dlmgr_shutdown(mgr);
                DlmgrDLL.dlmgr_sink_release(sink);
            }
            // 文件断言在 sink 释放后：Windows 下 sink 句柄锁着文件
            try
            {
                Assert.That(File.ReadAllBytes(outPath), Is.EqualTo(data));
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：上报节流与顺序（T5）——10Hz 节流下任务回调频率 ≤ ~11 次/秒
        /// （慢速 2MB ≈3.2s，应 ≥5 次进度上报证明上报在工作）；同一轮先任务后
        /// 全局：全局回调首次报 done=1 之前，该任务的 Done 终态回调必已发生。
        /// cancel 终态语义已由 Cancel_RunningTask 覆盖，不在此重复。
        /// </summary>
        [Test]
        public void CallbackThrottle_10Hz_TaskBeforeGlobal()
        {
            byte[] data = TestPaths.Content(2 * 1024 * 1024, 14);
            string url = _server.Add("throttle.bin", data, slowMs: 25, chunk: 16 * 1024);
            string outDir = TestPaths.NewDir();
            string outPath = Path.Combine(outDir, "out.bin");

            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 0, 0);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            var evLock = new object();
            int seq = 0, taskDoneSeq = -1, globalFirstDoneSeq = -1;
            long firstTick = 0, lastTick = 0;
            int runningCount = 0;
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr,
                (user, id, state, done, total, bps, err) =>
                {
                    lock (evLock)
                    {
                        seq++;
                        if (state == (int)DlmgrTaskState.Running)
                        {
                            runningCount++;
                            long t = Stopwatch.GetTimestamp();
                            if (firstTick == 0) firstTick = t;
                            lastTick = t;
                        }
                        if (state == (int)DlmgrTaskState.Done) taskDoneSeq = seq;
                    }
                    OnTask(user, id, state, done, total, bps, err);
                }, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_set_global_callback(mgr,
                (user, active, doneCount, failedCount, bytesDone, bytesTotal, bps) =>
                {
                    lock (evLock)
                    {
                        seq++;
                        if (doneCount >= 1 && globalFirstDoneSeq < 0) globalFirstDoneSeq = seq;
                    }
                }, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));

            ulong sink = DlmgrDLL.dlmgr_sink_create_file(mgr, outPath, (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                ulong id = EnqueueLocal(mgr, url, "throttle.bin", data, sink);
                Record r = WaitTerminal(id, TerminalMs);
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "应为 Done, err=" + r.Err);

                // 等全局回调上报 done=1（终态 tick 与全局 tick 之间有 ≤100ms 滞后）
                var deadline = DateTime.UtcNow.AddSeconds(5);
                while (Volatile.Read(ref globalFirstDoneSeq) < 0 && DateTime.UtcNow < deadline)
                {
                    Thread.Sleep(50);
                }

                Assert.That(runningCount, Is.GreaterThanOrEqualTo(5), "进度上报应在工作");
                double window = (lastTick - firstTick) / (double)Stopwatch.Frequency;
                Assert.That(window, Is.GreaterThan(0.5), "慢速下载窗口应足够长");
                Assert.That(runningCount, Is.AtMost((int)(11 * window) + 3),
                            "10Hz 节流下进度回调 " + runningCount + " 次 / " + window.ToString("F1") +
                            "s，超 ~11 次/秒");

                Assert.That(Volatile.Read(ref globalFirstDoneSeq), Is.GreaterThan(0),
                            "全局回调应上报 done=1");
                Assert.That(taskDoneSeq, Is.GreaterThan(0), "任务 Done 终态回调应已发生");
                Assert.That(taskDoneSeq, Is.LessThan(globalFirstDoneSeq),
                            "同一轮必须先任务后全局");
            }
            finally
            {
                DlmgrDLL.dlmgr_shutdown(mgr);
                DlmgrDLL.dlmgr_sink_release(sink);
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        // --- 杀进程重启（T6 手动验证，由 csharp/killrecovery.ps1 驱动）--------------------
        // 固定目录让两个独立进程共享 VFS 状态；NB_KILL_RECOVERY 未设时跳过（CI 永不触发）。
        // 语义依据：commit 仅内存可见，flush 才是耐久边界（§2.2）；重开时 Downloading
        // 残留条目丢弃、数据成垃圾（§2.2 open）。

        private static string KillRecoveryDir()
        {
            return Path.Combine(Path.GetTempPath(), "nb-kill-recovery");
        }

        /// <summary>
        /// stage1：两个快任务下载完成并 flush（耐久边界），慢任务仍在下载时宿主
        /// 挂住等死——被脚本整树杀掉后，磁盘上留下：已 flush 的 Active 条目 +
        /// 慢任务的半截数据（Downloading 残留）。
        /// </summary>
        [Test]
        public void KillRecovery_Stage1_DownloadFlushThenHang()
        {
            if (Environment.GetEnvironmentVariable("NB_KILL_RECOVERY") == null)
            {
                Assert.Ignore("杀进程重启手动验证：由 csharp/killrecovery.ps1 驱动（设 NB_KILL_RECOVERY=1 可直跑）");
            }
            string dir = KillRecoveryDir();
            Directory.CreateDirectory(dir);
            byte[] fast = TestPaths.Content(256 * 1024, 41);
            byte[] slow = TestPaths.Content(4 * 1024 * 1024, 42);
            string fastUrl = _server.Add("kr-fast.bin", fast);
            string slowUrl = _server.Add("kr-slow.bin", slow, slowMs: 40, chunk: 16 * 1024);

            IntPtr h = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h, Is.Not.EqualTo(IntPtr.Zero));
            var sinks = new ulong[3];
            try
            {
                sinks[0] = DlVfs.dlvfs_sink_create_for_vfs(h, "kr-a", (ulong)fast.Length);
                sinks[1] = DlVfs.dlvfs_sink_create_for_vfs(h, "kr-b", (ulong)fast.Length);
                sinks[2] = DlVfs.dlvfs_sink_create_for_vfs(h, "kr-slow", (ulong)slow.Length);
                ulong a = Enqueue(fastUrl, null, "kr-a", (ulong)fast.Length,
                                  TestPaths.Crc32(fast), sinks[0]);
                ulong b = Enqueue(fastUrl, null, "kr-b", (ulong)fast.Length,
                                  TestPaths.Crc32(fast), sinks[1]);
                ulong s = Enqueue(slowUrl, null, "kr-slow", (ulong)slow.Length,
                                  TestPaths.Crc32(slow), sinks[2]);

                WaitAllTerminal(new[] { a, b }, 60_000);
                VfsDLL.vfs_flush(h); // 耐久边界：两个 commit 落进 header
                File.WriteAllText(Path.Combine(dir, "stage1.ready"), "ok");
                WaitState(s, (int)DlmgrTaskState.Running, 15_000);
                // 慢任务下载中挂住等死（模拟宿主运行中被杀）；被杀则 finally 不执行。
                Thread.Sleep(Timeout.Infinite);
            }
            finally
            {
                for (int i = 0; i < 3; i++)
                {
                    if (sinks[i] != 0) DlmgrDLL.dlmgr_sink_release(sinks[i]);
                }
                VfsDLL.vfs_flush(h);
                VfsDLL.vfs_close(h);
            }
        }

        /// <summary>
        /// stage2（杀进程后重启）：重开同一 VFS——已 flush 的两个文件 Active 且
        /// 逐字节完好；慢任务条目不在（Downloading 残留被清理）；可重新整理。
        /// </summary>
        [Test]
        public void KillRecovery_Stage2_VerifyRecovery()
        {
            if (Environment.GetEnvironmentVariable("NB_KILL_RECOVERY") == null)
            {
                Assert.Ignore("杀进程重启手动验证：由 csharp/killrecovery.ps1 驱动（设 NB_KILL_RECOVERY=1 可直跑）");
            }
            string dir = KillRecoveryDir();
            Assert.That(File.Exists(Path.Combine(dir, "stage1.ready")), Is.True, "先跑 stage1");
            byte[] fast = TestPaths.Content(256 * 1024, 41);

            IntPtr h = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h, Is.Not.EqualTo(IntPtr.Zero));
            try
            {
                // 已 flush 的 commit 文件完好 Active
                foreach (string name in new[] { "kr-a", "kr-b" })
                {
                    int err = VfsDLL.vfs_lookup(h, DlmgrDLL.Utf8Bytes(name),
                                                out _, out ulong size, out int state, out uint crc);
                    Assert.That(err, Is.EqualTo((int)VfsResult.OK), name + " 应存在");
                    Assert.That((VfsFileState)state, Is.EqualTo(VfsFileState.Active), name + " 应 Active");
                    Assert.That(size, Is.EqualTo((ulong)fast.Length));
                    Assert.That(crc, Is.EqualTo(TestPaths.Crc32(fast)));
                }
                // 慢任务（Downloading 残留）被清理：条目不可见
                int errSlow = VfsDLL.vfs_lookup(h, DlmgrDLL.Utf8Bytes("kr-slow"),
                                                out _, out _, out _, out _);
                Assert.That(errSlow, Is.EqualTo((int)VfsResult.NotFound), "Downloading 残留应被丢弃");

                // 可重新整理（崩溃矩阵 §2.3 的 Finalize 语义）
                using (var done = new ManualResetEventSlim(false))
                {
                    int compactErr = int.MinValue;
                    VfsDoneDelegate onDone = (user, err2) => { compactErr = err2; done.Set(); };
                    int rc = VfsDLL.vfs_compact(h, 0, null, onDone, IntPtr.Zero);
                    Assert.That(rc, Is.EqualTo((int)VfsResult.OK), "compact 应可启动");
                    Assert.That(done.Wait(60_000), Is.True, "整理回调超时");
                    Assert.That(compactErr, Is.EqualTo((int)VfsResult.OK), "重启后必须可重新整理");
                }
            }
            finally
            {
                VfsDLL.vfs_flush(h);
                VfsDLL.vfs_close(h);
            }

            using (var reader = VfsReader.Open(dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                foreach (string name in new[] { "kr-a", "kr-b" })
                {
                    Assert.That(reader.TryReadBytes(name, out byte[] got), Is.True, name);
                    Assert.That(got, Is.EqualTo(fast), name + " 逐字节完好");
                }
            }
            try { Directory.Delete(dir, true); } catch { }
        }

        /// <summary>
        /// 测试重点：预扩容语义（T6）——下载期间 files.vfs 物理大小恒定
        /// （alloc 时 set_len 一次性预留），边写边采样 vfs_stat 断言不变，
        /// Done 后仍不变（整理前的物理大小只增不因写入变化）。
        /// </summary>
        [Test]
        public void DownloadIntoVfs_PhysicalSizeStableDuringWrite()
        {
            byte[] data = TestPaths.Content(2 * 1024 * 1024, 43);
            string url = _server.Add("phys.bin", data, slowMs: 20, chunk: 16 * 1024);
            string dir = TestPaths.NewDir();

            IntPtr h = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h, Is.Not.EqualTo(IntPtr.Zero));
            ulong sink = DlVfs.dlvfs_sink_create_for_vfs(h, "phys.bin", (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL));
            try
            {
                ulong id = Enqueue(url, null, "phys.bin", (ulong)data.Length,
                                   TestPaths.Crc32(data), sink);
                WaitState(id, (int)DlmgrTaskState.Running, 15_000);

                ulong prevPhysical = 0;
                for (int sample = 0; sample < 3; sample++)
                {
                    Assert.That(VfsDLL.vfs_stat(h, out _, out ulong physical, out _,
                                                out _, out _, out _),
                                Is.EqualTo((int)VfsResult.OK));
                    if (sample > 0)
                    {
                        Assert.That(physical, Is.EqualTo(prevPhysical),
                                    "下载期间物理大小必须恒定（alloc 已一次性预留）");
                    }
                    prevPhysical = physical;
                    Thread.Sleep(300);
                }
                Record r = WaitTerminal(id, TerminalMs);
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "应为 Done, err=" + r.Err);
                Assert.That(VfsDLL.vfs_stat(h, out _, out ulong finalPhysical, out _,
                                            out _, out _, out _),
                            Is.EqualTo((int)VfsResult.OK));
                Assert.That(finalPhysical, Is.EqualTo(prevPhysical), "Done 后物理大小不变");
            }
            finally
            {
                DlmgrDLL.dlmgr_sink_release(sink);
                VfsDLL.vfs_flush(h);
                VfsDLL.vfs_close(h);
            }
            try { Directory.Delete(dir, true); } catch { }
        }

        /// <summary>
        /// 测试重点：dlmgr + VFS sink 端到端（T6 胶水场景）——下载经
        /// dlvfs_sink_create_for_vfs 直写 VFS 容器：终态 Done 后条目立即
        /// 以 Active 状态可见（commit 已随 sink 完成触发），内容与 crc
        /// 与下载源逐字节一致。
        /// </summary>
        [Test]
        public void DownloadIntoVfsSink_E2E()
        {
            byte[] data = TestPaths.Content(512 * 1024, 55);
            string url = _server.Add("vfs.bin", data);
            string dir = TestPaths.NewDir();

            IntPtr h = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h, Is.Not.EqualTo(IntPtr.Zero));

            ulong sink = DlVfs.dlvfs_sink_create_for_vfs(h, "dl.bin", (ulong)data.Length);
            Assert.That(sink, Is.Not.EqualTo(0UL), "dlvfs_sink_create_for_vfs 失败");
            try
            {
                ulong id = Enqueue(url, null, "dl.bin", (ulong)data.Length, TestPaths.Crc32(data), sink);
                Record r = WaitTerminal(id, TerminalMs);
                Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "应为 Done, err=" + r.Err);
            }
            finally
            {
                // 纪律：先等终态（已 WaitTerminal）→ 释放 sink → 才允许关 VFS 句柄
                DlmgrDLL.dlmgr_sink_release(sink);
                VfsDLL.vfs_flush(h);
                VfsDLL.vfs_close(h);
            }

            // 重新打开验证：下载进 VFS 的文件对读者立即可见
            using (var reader = VfsReader.Open(dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                Assert.That(reader.TryGet("dl.bin", out VfsEntry e), Is.True);
                Assert.That(e.State, Is.EqualTo(VfsFileState.Active));
                Assert.That(e.Size, Is.EqualTo((ulong)data.Length));
                Assert.That(e.Crc, Is.EqualTo(TestPaths.Crc32(data)));
                Assert.That(reader.TryReadBytes("dl.bin", out byte[] got), Is.True);
                Assert.That(got, Is.EqualTo(data));
            }
            try { Directory.Delete(dir, true); } catch { }
        }

        /// <summary>
        /// One reader owns the VFS for downloads AND reads. Commit callbacks run on
        /// workers and must reject self/cross-manager shutdown without throwing.
        /// Entries are read through that same reader after terminal, without reopening.
        /// </summary>
        [Test]
        public void DownloadIntoOneReader_CommitShutdownRejected_ThenReadSameInstance()
        {
            string dir = TestPaths.NewDir();
            IntPtr mgr = DlmgrDLL.dlmgr_create(2, 0, 0);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            try
            {
                using (var reader = VfsReader.Open(dir))
                {
                    var contents = new[] { TestPaths.Content(128 * 1024, 31), TestPaths.Content(256 * 1024, 32) };
                    var names = new[] { "reader-first.bin", "reader-second.bin" };
                    var sinks = new ulong[contents.Length];
                    var ids = new ulong[contents.Length];
                    int commits = 0, unexpectedResult = 0;
                    reader.SetCommitCallback((user, name, offset, size, crc) =>
                    {
                        int self = DlmgrDLL.dlmgr_shutdown_try(mgr);
                        int other = DlmgrDLL.dlmgr_shutdown_try(_mgr);
                        if (self != -1 || other != -1) Interlocked.Exchange(ref unexpectedResult, 1);
                        Interlocked.Increment(ref commits);
                    });
                    try
                    {
                        Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
                        reader.RefreshIndex();
                        Assert.That(reader.Count, Is.EqualTo(0));
                        // Queue before start: worker callbacks may run before all
                        // join handles are published, so thread identity must use TLS.
                        for (int i = 0; i < contents.Length; i++)
                        {
                            sinks[i] = DlVfs.dlvfs_sink_create_for_vfs(reader, names[i], (ulong)contents[i].Length);
                            Assert.That(sinks[i], Is.Not.EqualTo(0UL));
                            ids[i] = EnqueueLocal(mgr, _server.Add(names[i], contents[i]), names[i], contents[i], sinks[i]);
                        }
                        Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));
                        WaitAllTerminal(ids, TerminalMs);
                        Assert.That(Volatile.Read(ref commits), Is.EqualTo(contents.Length));
                        Assert.That(Volatile.Read(ref unexpectedResult), Is.EqualTo(0));
                        reader.RefreshIndex();
                        reader.EnsureMapping();
                        Assert.That(reader.Count, Is.EqualTo(contents.Length));
                        for (int i = 0; i < contents.Length; i++)
                        {
                            Assert.That(_records[ids[i]].State, Is.EqualTo((int)DlmgrTaskState.Done));
                            Assert.That(reader.TryGet(names[i], out VfsEntry entry), Is.True);
                            Assert.That(entry.State, Is.EqualTo(VfsFileState.Active));
                            Assert.That(entry.Size, Is.EqualTo((ulong)contents[i].Length));
                            Assert.That(entry.Crc, Is.EqualTo(TestPaths.Crc32(contents[i])));
                            Assert.That(reader.TryReadBytes(names[i], out byte[] got), Is.True);
                            Assert.That(got, Is.EqualTo(contents[i]));
                        }
                        int result = DlmgrDLL.dlmgr_shutdown_try(mgr);
                        if (result == 0) mgr = IntPtr.Zero;
                        Assert.That(result, Is.EqualTo(0));
                    }
                    finally
                    {
                        // Even on failure, join workers before releasing their sinks;
                        // the reader owns its handle until ALL sinks are gone.
                        if (mgr != IntPtr.Zero)
                        {
                            DlmgrDLL.dlmgr_shutdown(mgr);
                            mgr = IntPtr.Zero;
                        }
                        foreach (ulong sink in sinks)
                        {
                            if (sink != 0) DlmgrDLL.dlmgr_sink_release(sink);
                        }
                    }
                }
            }
            finally
            {
                if (mgr != IntPtr.Zero) DlmgrDLL.dlmgr_shutdown(mgr);
                try { Directory.Delete(dir, true); } catch { }
            }
        }

        // --- 并发 / 稳定性 -----------------------------------------------------------
        // native 侧契约依据：dlmgr_enqueue 全程持 queue/tasks 锁 + 原子 id；
        // sink 注册表为 Mutex + 原子句柄（rust dlmgr/mod.rs、dlmgr/sink.rs）——
        // 多任务、多线程 enqueue 均为合法用法。

        /// <summary>
        /// 测试重点：多任务并发与 worker 池调度——8 个任务（> 2 worker）一次性
        /// 入队，排队 + 2 worker 并行消化：全部 Done、每个 sink 文件逐字节正确、
        /// 全局计数器收敛（done=8 / failed=0 / active=0）。
        /// </summary>
        [Test]
        public void ConcurrentTasks_AllComplete_AndGlobalCountersMatch()
        {
            const int TaskCount = 8;
            var contents = new byte[TaskCount][];
            var urls = new string[TaskCount];
            var outDir = TestPaths.NewDir();
            var outPaths = new string[TaskCount];
            var sinks = new ulong[TaskCount];
            var ids = new ulong[TaskCount];
            try
            {
                for (int i = 0; i < TaskCount; i++)
                {
                    contents[i] = TestPaths.Content(1024 * 1024, (byte)(i + 1));
                    urls[i] = _server.Add("conc-" + i + ".bin", contents[i]);
                    outPaths[i] = Path.Combine(outDir, "out-" + i + ".bin");
                    sinks[i] = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPaths[i], (ulong)contents[i].Length);
                    Assert.That(sinks[i], Is.Not.EqualTo(0UL), "sink " + i + " 创建失败");
                }

                // 先全部入队，再统一等待——排队与并行调度都真实发生
                for (int i = 0; i < TaskCount; i++)
                {
                    ids[i] = Enqueue(urls[i], null, "task-" + i, (ulong)contents[i].Length,
                                     TestPaths.Crc32(contents[i]), sinks[i]);
                }

                WaitAllTerminal(ids, 120_000);
                for (int i = 0; i < TaskCount; i++)
                {
                    Record r = _records[ids[i]];
                    Assert.That(r.State, Is.EqualTo((int)DlmgrTaskState.Done), "任务 " + i + " 应 Done");
                    Assert.That(r.Done, Is.EqualTo((ulong)contents[i].Length));
                }
                WaitGlobalCounters(TaskCount, 0);
            }
            finally
            {
                for (int i = 0; i < TaskCount; i++)
                {
                    if (sinks[i] != 0) DlmgrDLL.dlmgr_sink_release(sinks[i]);
                }
            }
            try
            {
                for (int i = 0; i < TaskCount; i++)
                {
                    Assert.That(File.ReadAllBytes(outPaths[i]), Is.EqualTo(contents[i]),
                                "任务 " + i + " 文件内容不符");
                }
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：混跑隔离性——慢速任务被取消的同时，快速任务不受干扰
        /// 正常完成：fast 全部 Done 且文件正确、慢速任务 Canceled、
        /// 全局计数器（done=2 / failed=0 / active=0）证明取消未被记成失败。
        /// </summary>
        [Test]
        public void CancelSlow_WhileFastTasksComplete()
        {
            byte[] slowData = TestPaths.Content(4 * 1024 * 1024, 91);
            string slowUrl = _server.Add("mix-slow.bin", slowData, slowMs: 25, chunk: 16 * 1024);
            byte[] fastData = TestPaths.Content(256 * 1024, 92);
            string fastUrl = _server.Add("mix-fast.bin", fastData);
            var outDir = TestPaths.NewDir();
            var fastPaths = new[] { Path.Combine(outDir, "f0.bin"), Path.Combine(outDir, "f1.bin") };
            string slowPath = Path.Combine(outDir, "slow.bin");

            ulong slowSink = DlmgrDLL.dlmgr_sink_create_file(_mgr, slowPath, (ulong)slowData.Length);
            var fastSinks = new ulong[2];
            for (int i = 0; i < 2; i++)
            {
                fastSinks[i] = DlmgrDLL.dlmgr_sink_create_file(_mgr, fastPaths[i], (ulong)fastData.Length);
                Assert.That(fastSinks[i], Is.Not.EqualTo(0UL));
            }
            Assert.That(slowSink, Is.Not.EqualTo(0UL));
            try
            {
                ulong slowId = Enqueue(slowUrl, null, "mix-slow", (ulong)slowData.Length,
                                       TestPaths.Crc32(slowData), slowSink);
                var fastIds = new ulong[2];
                for (int i = 0; i < 2; i++)
                {
                    fastIds[i] = Enqueue(fastUrl, null, "mix-fast-" + i, (ulong)fastData.Length,
                                         TestPaths.Crc32(fastData), fastSinks[i]);
                }

                WaitAllTerminal(fastIds, 60_000);
                Assert.That(_records[fastIds[0]].State, Is.EqualTo((int)DlmgrTaskState.Done));
                Assert.That(_records[fastIds[1]].State, Is.EqualTo((int)DlmgrTaskState.Done));

                WaitState(slowId, (int)DlmgrTaskState.Running, 30_000);
                Assert.That(DlmgrDLL.dlmgr_cancel(_mgr, slowId), Is.EqualTo(0));
                Record slowRec = WaitTerminal(slowId, 30_000);
                Assert.That(slowRec.State, Is.EqualTo((int)DlmgrTaskState.Canceled));
                Assert.That(slowRec.Err, Is.EqualTo((int)DlmgrTaskError.Canceled));

                WaitGlobalCounters(2, 0);
            }
            finally
            {
                if (slowSink != 0) DlmgrDLL.dlmgr_sink_release(slowSink);
                for (int i = 0; i < 2; i++)
                {
                    if (fastSinks[i] != 0) DlmgrDLL.dlmgr_sink_release(fastSinks[i]);
                }
            }
            try
            {
                Assert.That(File.ReadAllBytes(fastPaths[0]), Is.EqualTo(fastData));
                Assert.That(File.ReadAllBytes(fastPaths[1]), Is.EqualTo(fastData));
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：绑定层多线程 enqueue 线程安全——4 个 C# 线程同时调
        /// dlmgr_enqueue（native 全程持锁，见 mod.rs）：20 个 taskId 互不重复、
        /// 全部 Done、全局计数器精确等于 20/0，无丢失无错乱。
        /// </summary>
        [Test]
        public void ConcurrentEnqueue_FromMultipleThreads()
        {
            const int Threads = 4;
            const int PerThread = 5;
            byte[] data = TestPaths.Content(256 * 1024, 66);
            string url = _server.Add("mt.bin", data);
            var outDir = TestPaths.NewDir();
            var outPaths = new string[Threads * PerThread];
            var sinks = new ulong[Threads * PerThread];
            var ids = new ConcurrentQueue<ulong>();
            try
            {
                for (int i = 0; i < outPaths.Length; i++)
                {
                    outPaths[i] = Path.Combine(outDir, "out-" + i + ".bin");
                    sinks[i] = DlmgrDLL.dlmgr_sink_create_file(_mgr, outPaths[i], (ulong)data.Length);
                    Assert.That(sinks[i], Is.Not.EqualTo(0UL));
                }

                var threads = new List<Thread>();
                for (int t = 0; t < Threads; t++)
                {
                    int t0 = t * PerThread; // 本线程负责的 sink 区间
                    threads.Add(new Thread(() =>
                    {
                        for (int i = t0; i < t0 + PerThread; i++)
                        {
                            int rc = DlmgrDLL.dlmgr_enqueue(_mgr, url, null, "mt-" + i,
                                (ulong)data.Length, TestPaths.Crc32(data), 0, 0, sinks[i], out ulong id);
                            Assert.That(rc, Is.EqualTo(0), "多线程 enqueue 失败");
                            ids.Enqueue(id);
                        }
                    }));
                }
                threads.ForEach(th => th.Start());
                threads.ForEach(th => th.Join(30_000));

                Assert.That(ids.Count, Is.EqualTo(Threads * PerThread), "入队数不符");
                Assert.That(new HashSet<ulong>(ids).Count, Is.EqualTo(Threads * PerThread), "taskId 有重复");

                WaitAllTerminal(ids, 120_000);
                WaitGlobalCounters(Threads * PerThread, 0);
            }
            finally
            {
                for (int i = 0; i < sinks.Length; i++)
                {
                    if (sinks[i] != 0) DlmgrDLL.dlmgr_sink_release(sinks[i]);
                }
            }
            try
            {
                for (int i = 0; i < outPaths.Length; i++)
                {
                    Assert.That(File.ReadAllBytes(outPaths[i]), Is.EqualTo(data), "文件 " + i + " 内容不符");
                }
            }
            finally
            {
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：混合 sink 稳定性压测——单个 manager、单个 VFS 容器上
        /// 12 个 VFS sink + 12 个文件 sink 共 24 个任务并发跑完（真实 Unity
        /// 用法形态）：全部 Done、VFS 条目 Active 且逐字节正确、文件 sink
        /// 内容正确、全局计数器 24/0、manager shutdown 干净收尾。
        /// </summary>
        [Test]
        public void Stability_SoakMixedSinks()
        {
            const int PerKind = 12;
            byte[] data = TestPaths.Content(256 * 1024, 77);
            string url = _server.Add("soak.bin", data);
            var dir = TestPaths.NewDir();
            var outDir = TestPaths.NewDir();
            var fileSinks = new ulong[PerKind];
            var vfsSinks = new ulong[PerKind];
            var ids = new List<ulong>();

            IntPtr h = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h, Is.Not.EqualTo(IntPtr.Zero));
            try
            {
                for (int i = 0; i < PerKind; i++)
                {
                    string fp = Path.Combine(outDir, "f-" + i + ".bin");
                    fileSinks[i] = DlmgrDLL.dlmgr_sink_create_file(_mgr, fp, (ulong)data.Length);
                    Assert.That(fileSinks[i], Is.Not.EqualTo(0UL));
                    vfsSinks[i] = DlVfs.dlvfs_sink_create_for_vfs(h, "soak-v-" + i, (ulong)data.Length);
                    Assert.That(vfsSinks[i], Is.Not.EqualTo(0UL));
                }
                for (int i = 0; i < PerKind; i++)
                {
                    ids.Add(Enqueue(url, null, "soak-f-" + i, (ulong)data.Length,
                                    TestPaths.Crc32(data), fileSinks[i]));
                    ids.Add(Enqueue(url, null, "soak-v-" + i, (ulong)data.Length,
                                    TestPaths.Crc32(data), vfsSinks[i]));
                }

                WaitAllTerminal(ids, 180_000);
                for (int i = 0; i < PerKind; i++)
                {
                    Assert.That(_records[ids[i * 2]].State, Is.EqualTo((int)DlmgrTaskState.Done));
                    Assert.That(_records[ids[i * 2 + 1]].State, Is.EqualTo((int)DlmgrTaskState.Done));
                }
                WaitGlobalCounters(PerKind * 2, 0);
            }
            finally
            {
                // 纪律：全部到终态后才能释放 sink；VFS 句柄在 sink 全释放后才关
                for (int i = 0; i < PerKind; i++)
                {
                    if (fileSinks[i] != 0) DlmgrDLL.dlmgr_sink_release(fileSinks[i]);
                    if (vfsSinks[i] != 0) DlmgrDLL.dlmgr_sink_release(vfsSinks[i]);
                }
                VfsDLL.vfs_flush(h);
                VfsDLL.vfs_close(h);
            }

            // 验证 VFS 侧：12 个条目全部 Active 且逐字节正确
            using (var reader = VfsReader.Open(dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                Assert.That(reader.Count, Is.EqualTo(PerKind));
                for (int i = 0; i < PerKind; i++)
                {
                    Assert.That(reader.TryGet("soak-v-" + i, out VfsEntry e), Is.True);
                    Assert.That(e.State, Is.EqualTo(VfsFileState.Active));
                    Assert.That(e.Crc, Is.EqualTo(TestPaths.Crc32(data)));
                    Assert.That(reader.TryReadBytes("soak-v-" + i, out byte[] got), Is.True);
                    Assert.That(got, Is.EqualTo(data));
                }
            }
            try
            {
                for (int i = 0; i < PerKind; i++)
                {
                    Assert.That(File.ReadAllBytes(Path.Combine(outDir, "f-" + i + ".bin")),
                                Is.EqualTo(data));
                }
            }
            finally
            {
                try { Directory.Delete(dir, true); } catch { }
                try { Directory.Delete(outDir, true); } catch { }
            }
        }

        /// <summary>
        /// 测试重点：失败恢复语义（用户需求 1）——3 个文件下载，中间的因
        /// 瞬时网络故障（前 3 次请求传输中途掐断，重试预算耗尽）失败；
        /// VFS 句柄关闭重开（"中断"）后：已完成文件保持 Active、失败文件
        /// 条目已被 abort 丢弃；宿主按索引跳过已完成的、只对失败的重新入队。
        /// 反证：对 Active 名字重复下载在 prepare 阶段失败（Failed/Network），
        /// 已有数据不受影响（sink 创建是懒封装，alloc 推迟到 prepare）。
        /// 边界：失败文件重下从 0 开始（断点续传仅限同一任务内）。
        /// </summary>
        [Test]
        public void RerunAfterFailure_SkipsCommitted_RetriesFailed()
        {
            var names = new[] { "file-1", "file-2", "file-3" };
            var contents = new[]
            {
                TestPaths.Content(256 * 1024, 1),
                TestPaths.Content(1024 * 1024, 2),
                TestPaths.Content(256 * 1024, 3),
            };
            var urls = new[]
            {
                _server.Add("rf-1.bin", contents[0]),
                _server.Add("rf-2.bin", contents[1], dropRequests: 3), // 初始 + 2 次重试全掐断
                _server.Add("rf-3.bin", contents[2]),
            };
            var dir = TestPaths.NewDir();

            IntPtr h = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h, Is.Not.EqualTo(IntPtr.Zero));
            var sinks = new ulong[3];
            var ids = new ulong[3];
            try
            {
                // ── 会话 1：三个文件一起下载，中间的失败
                for (int i = 0; i < 3; i++)
                {
                    sinks[i] = DlVfs.dlvfs_sink_create_for_vfs(h, names[i], (ulong)contents[i].Length);
                    Assert.That(sinks[i], Is.Not.EqualTo(0UL), names[i] + " sink 创建失败");
                    ids[i] = Enqueue(urls[i], null, names[i], (ulong)contents[i].Length,
                                     TestPaths.Crc32(contents[i]), sinks[i]);
                }
                WaitAllTerminal(ids, 120_000);
                Assert.That(_records[ids[0]].State, Is.EqualTo((int)DlmgrTaskState.Done));
                Assert.That(_records[ids[1]].State, Is.EqualTo((int)DlmgrTaskState.Failed), "file-2 应失败");
                Assert.That(_records[ids[1]].Err, Is.EqualTo((int)DlmgrTaskError.Network));
                Assert.That(_records[ids[2]].State, Is.EqualTo((int)DlmgrTaskState.Done));
            }
            finally
            {
                for (int i = 0; i < 3; i++)
                {
                    if (sinks[i] != 0) DlmgrDLL.dlmgr_sink_release(sinks[i]);
                }
                VfsDLL.vfs_flush(h);
                VfsDLL.vfs_close(h); // VFS 中断：关闭句柄（close 自带 best-effort 落盘）
            }

            // ── 会话 2：重开 VFS + 新 manager，只补失败的
            _records.Clear();
            IntPtr mgr2 = DlmgrDLL.dlmgr_create(2, 0, 2);
            Assert.That(mgr2, Is.Not.EqualTo(IntPtr.Zero));
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr2, OnTask, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr2), Is.EqualTo(0));
            IntPtr h2 = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h2, Is.Not.EqualTo(IntPtr.Zero));
            ulong sink2 = 0;
            try
            {
                // 重开后索引：已完成的 Active，失败的已消失（abort 丢弃条目）
                int err = VfsDLL.vfs_lookup(h2, DlmgrDLL.Utf8Bytes(names[0]), out _, out _, out int s1, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That((VfsFileState)s1, Is.EqualTo(VfsFileState.Active));
                err = VfsDLL.vfs_lookup(h2, DlmgrDLL.Utf8Bytes(names[2]), out _, out _, out int s3, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That((VfsFileState)s3, Is.EqualTo(VfsFileState.Active));
                err = VfsDLL.vfs_lookup(h2, DlmgrDLL.Utf8Bytes(names[1]), out _, out _, out _, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.NotFound), "失败文件的条目应已被 abort 丢弃");

                // 反证：对 Active 名字重复下载——sink 创建是懒封装（成功返回句柄），
                // 真正的 alloc 在 worker prepare 阶段：任务 Failed(Network)，
                // 且已有数据毫发无损（这就是宿主必须按索引跳过的原因）
                ulong dupSink = DlVfs.dlvfs_sink_create_for_vfs(h2, names[0], (ulong)contents[0].Length);
                Assert.That(dupSink, Is.Not.EqualTo(0UL));
                int rc = DlmgrDLL.dlmgr_enqueue(mgr2, urls[0], null, names[0],
                                                (ulong)contents[0].Length, TestPaths.Crc32(contents[0]),
                                                0, 0, dupSink, out ulong dupId);
                Assert.That(rc, Is.EqualTo(0));
                Record dup = WaitTerminal(dupId, 60_000);
                Assert.That(dup.State, Is.EqualTo((int)DlmgrTaskState.Failed), "重复下载应失败");
                Assert.That(dup.Err, Is.EqualTo((int)DlmgrTaskError.Network));
                DlmgrDLL.dlmgr_sink_release(dupSink);
                err = VfsDLL.vfs_lookup(h2, DlmgrDLL.Utf8Bytes(names[0]), out _, out _, out s1, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That((VfsFileState)s1, Is.EqualTo(VfsFileState.Active), "重复下载不得破坏已有数据");

                // 宿主跳过逻辑：只对缺失的 file-2 重新入队（从 0 开始）
                sink2 = DlVfs.dlvfs_sink_create_for_vfs(h2, names[1], (ulong)contents[1].Length);
                Assert.That(sink2, Is.Not.EqualTo(0UL));
                rc = DlmgrDLL.dlmgr_enqueue(mgr2, urls[1], null, names[1],
                                            (ulong)contents[1].Length, TestPaths.Crc32(contents[1]),
                                            0, 0, sink2, out ulong id2);
                Assert.That(rc, Is.EqualTo(0));
                Record r2 = WaitTerminal(id2, 120_000);
                Assert.That(r2.State, Is.EqualTo((int)DlmgrTaskState.Done), "恢复后重下应 Done");
            }
            finally
            {
                if (sink2 != 0) DlmgrDLL.dlmgr_sink_release(sink2);
                VfsDLL.vfs_flush(h2);
                VfsDLL.vfs_close(h2);
                DlmgrDLL.dlmgr_shutdown(mgr2);
            }

            // 终验：重开后三个文件全部 Active 且逐字节正确（1/3 未被重下、2 已补齐）
            using (var reader = VfsReader.Open(dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                Assert.That(reader.Count, Is.EqualTo(3));
                for (int i = 0; i < 3; i++)
                {
                    Assert.That(reader.TryGet(names[i], out VfsEntry e), Is.True);
                    Assert.That(e.State, Is.EqualTo(VfsFileState.Active));
                    Assert.That(e.Crc, Is.EqualTo(TestPaths.Crc32(contents[i])));
                    Assert.That(reader.TryReadBytes(names[i], out byte[] got), Is.True);
                    Assert.That(got, Is.EqualTo(contents[i]));
                }
            }
            try { Directory.Delete(dir, true); } catch { }
        }

        /// <summary>
        /// 测试重点：动态提权（dlmgr_set_priority）——单 worker 下派发/完成
        /// 顺序确定：慢任务占住 worker，两个低优先级与一个紧急任务排队，
        /// 入队后把紧急任务从 P0 提到 P10 → 它越过两个低优先级任务被先派发
        /// （完成序 slow → urgent → low1 → low2）。反证：运行中任务提权返回
        /// -1（不可抢占）、未知 id 返回 -1。
        /// </summary>
        [Test]
        public void SetPriority_QueuedTaskDispatchedFirst()
        {
            byte[] slowData = TestPaths.Content(2 * 1024 * 1024, 81);
            string slowUrl = _server.Add("prio-slow.bin", slowData, slowMs: 25, chunk: 16 * 1024);
            byte[] fastData = TestPaths.Content(128 * 1024, 82); // 8 块 × 30ms = 240ms，跨多个上报 tick
            string fastUrl = _server.Add("prio-fast.bin", fastData, slowMs: 30, chunk: 16 * 1024);
            var outDir = TestPaths.NewDir();

            // 单 worker：派发严格串行，完成顺序 == 派发顺序，断言确定
            IntPtr mgr = DlmgrDLL.dlmgr_create(1, 0, 0);
            Assert.That(mgr, Is.Not.EqualTo(IntPtr.Zero));
            Assert.That(DlmgrDLL.dlmgr_set_task_callback(mgr, OnTask, IntPtr.Zero), Is.EqualTo(0));
            Assert.That(DlmgrDLL.dlmgr_start(mgr), Is.EqualTo(0));

            var outPaths = new[] { "slow.bin", "low1.bin", "low2.bin", "urgent.bin" }
                .Select(n => Path.Combine(outDir, n)).ToArray();
            var sinks = new ulong[4];
            try
            {
                for (int i = 0; i < 4; i++)
                {
                    byte[] body = i == 0 ? slowData : fastData;
                    sinks[i] = DlmgrDLL.dlmgr_sink_create_file(mgr, outPaths[i], (ulong)body.Length);
                    Assert.That(sinks[i], Is.Not.EqualTo(0UL));
                }
                ulong slowId = EnqueueLocal(mgr, slowUrl, "prio-slow", slowData, sinks[0]);
                ulong low1 = EnqueueLocal(mgr, fastUrl, "prio-low1", fastData, sinks[1]);
                ulong low2 = EnqueueLocal(mgr, fastUrl, "prio-low2", fastData, sinks[2]);
                ulong urgent = EnqueueLocal(mgr, fastUrl, "prio-urgent", fastData, sinks[3]);

                // 反证：运行中任务不可抢占（提权返回 -1）；未知 id 返回 -1。
                // 先等 slow 真正 Running——慢 runner 上入队到出队有窗口，Pending
                // 状态提权合法返回 0（曾致本测试在 mac runner 上 flaky）。
                WaitState(slowId, (int)DlmgrTaskState.Running, 10_000);
                Assert.That(DlmgrDLL.dlmgr_set_priority(mgr, slowId, 5), Is.EqualTo(-1),
                            "运行中任务提权应被拒绝");
                Assert.That(DlmgrDLL.dlmgr_set_priority(mgr, 9999, 5), Is.EqualTo(-1));

                // 功能本体：排队中的 urgent 从 P0 提到 P10，越过 low1/low2 先派发
                Assert.That(DlmgrDLL.dlmgr_set_priority(mgr, urgent, 10), Is.EqualTo(0));

                var all = new[] { slowId, low1, low2, urgent };
                WaitAllTerminal(all, 120_000);
                Assert.That(DoneOrderSnapshot(),
                            Is.EqualTo(new List<ulong> { slowId, urgent, low1, low2 }),
                            "提权后的任务应越过低优先级任务先派发");
            }
            finally
            {
                for (int i = 0; i < 4; i++)
                {
                    if (sinks[i] != 0) DlmgrDLL.dlmgr_sink_release(sinks[i]);
                }
                DlmgrDLL.dlmgr_shutdown(mgr); // 消费句柄，与 TearDown 的 _mgr 互不影响
            }
            try { Directory.Delete(outDir, true); } catch { }
        }

        /// <summary>指定 manager 的 enqueue（并发/生命周期测试用，不绑 fixture 的 _mgr）。</summary>
        private ulong EnqueueLocal(IntPtr mgr, string url, string name, byte[] data, ulong sink)
        {
            int rc = DlmgrDLL.dlmgr_enqueue(mgr, url, null, name, (ulong)data.Length,
                                            TestPaths.Crc32(data), 0, 0, sink, out ulong id);
            Assert.That(rc, Is.EqualTo(0), "enqueue 失败: " + name);
            return id;
        }
    }
}
