//
// DlmgrTests.cs — DlmgrDLL / DlVfs 对真 native 库的下载管理测试。
//
// 真实走 curl → 127.0.0.1 HttpTestServer，覆盖：文件 sink 下载回环、
// dual-URL 回退语义（range 源不支持 Range → 切 full 源）、crc 闸失败、
// 运行中取消、以及 dlmgr + VFS sink 的端到端胶水（T6 场景）。
//
using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
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

                // 反证：运行中任务不可抢占（提权返回 -1）；未知 id 返回 -1
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
