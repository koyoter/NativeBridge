//
// VfsTests.cs — VfsDLL / VfsReader 对真 native 库的绑定测试。
//
// 覆盖：容器打开、三段式写、UTF-8 文件名、软删除、防撕裂门禁、映射重建、
//       持久化、异步压实、并发写下的 GenChanged 收敛、commit 回调、
//       enumerate 两段式契约、参数守卫。
//
using System;
using System.Collections.Concurrent;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using NUnit.Framework;

namespace NativeBridgeF.Tests
{
    [TestFixture]
    public class VfsTests
    {
        private string _dir;

        [SetUp]
        public void SetUp()
        {
            _dir = TestPaths.NewDir();
        }

        [TearDown]
        public void TearDown()
        {
            // 各测试负责先释放句柄/映射；此处尽力清理，失败不影响判定。
            try { Directory.Delete(_dir, recursive: true); } catch { }
        }

        // --- helpers -------------------------------------------------------------

        /// <summary>目录式打开：dir/header.vfs + dir/files.vfs。</summary>
        private static IntPtr OpenRaw(string dir)
        {
            IntPtr h = VfsDLL.vfs_open_paths(
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "header.vfs")),
                DlmgrDLL.Utf8Bytes(Path.Combine(dir, "files.vfs")));
            Assert.That(h, Is.Not.EqualTo(IntPtr.Zero), "vfs_open_paths 失败");
            return h;
        }

        /// <summary>裸 API 三段式写：alloc → write → commit。</summary>
        private static void NativeWrite(IntPtr h, string name, byte[] data)
        {
            IntPtr w = VfsDLL.vfs_alloc(h, DlmgrDLL.Utf8Bytes(name), (ulong)data.Length);
            Assert.That(w, Is.Not.EqualTo(IntPtr.Zero), "vfs_alloc 失败: " + name);
            if (data.Length > 0)
            {
                int err = VfsDLL.vfs_writer_write(w, 0, data, 0, data.Length);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK), "vfs_writer_write 失败: " + name);
            }
            Assert.That(VfsDLL.vfs_writer_commit(w), Is.EqualTo((int)VfsResult.OK), "commit 失败: " + name);
        }

        // --- tests -----------------------------------------------------------------

        /// <summary>
        /// 测试重点：目录式打开会创建 header.vfs/files.vfs 文件对，ABI 版本守卫恒为 1。
        /// 验证容器初始化与 C#/native 版本契约。
        /// </summary>
        [Test]
        public void Open_CreatesContainer_AndAbiVersionIs1()
        {
            Assert.That(VfsReader.AbiVersion(), Is.EqualTo(1));
            using (var reader = VfsReader.Open(_dir))
            {
                Assert.That(File.Exists(Path.Combine(_dir, "header.vfs")), Is.True);
                Assert.That(File.Exists(Path.Combine(_dir, "files.vfs")), Is.True);
                Assert.That(reader.Count, Is.EqualTo(0));
            }
        }

        /// <summary>
        /// 测试重点：写路径全链路（alloc → write → commit）落盘后，
        /// VfsReader 读回内容逐字节一致，Size/State/Crc 与写入内容吻合。
        /// </summary>
        [Test]
        public void WriteCommitRead_Roundtrip()
        {
            byte[] data = TestPaths.Content(100 * 1024, 7);
            IntPtr h = OpenRaw(_dir);
            try
            {
                NativeWrite(h, "blob.bin", data);
                NativeWrite(h, "small.txt", Encoding.UTF8.GetBytes("hello vfs"));
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }

            using (var reader = VfsReader.Open(_dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();

                Assert.That(reader.TryGet("blob.bin", out VfsEntry e), Is.True);
                Assert.That(e.Size, Is.EqualTo((ulong)data.Length));
                Assert.That(e.State, Is.EqualTo(VfsFileState.Active));
                Assert.That(e.Crc, Is.EqualTo(TestPaths.Crc32(data)));

                Assert.That(reader.TryReadBytes("blob.bin", out byte[] got), Is.True);
                Assert.That(got, Is.EqualTo(data));

                Assert.That(reader.TryReadBytes("small.txt", out byte[] txt), Is.True);
                Assert.That(Encoding.UTF8.GetString(txt), Is.EqualTo("hello vfs"));
            }
        }

        /// <summary>
        /// 测试重点：UTF-8 文件名跨 ABI 不经 ANSI 代码页转换（禁 CharSet.Ansi 的意义），
        /// 中文名/空格/特殊字符按原字节写入并按原字符串读出。
        /// </summary>
        [Test]
        public void Utf8FileName_Roundtrip()
        {
            string[] names = { "中文资源.bin", "with space.png", "emoji-名-✓.dat" };
            byte[] data = TestPaths.Content(4096, 3);

            IntPtr h = OpenRaw(_dir);
            try
            {
                for (int i = 0; i < names.Length; i++) NativeWrite(h, names[i], data);
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }

            using (var reader = VfsReader.Open(_dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                Assert.That(reader.Count, Is.EqualTo(names.Length));
                foreach (string name in names)
                {
                    Assert.That(reader.TryGet(name, out _), Is.True, "缺少条目: " + name);
                    Assert.That(reader.TryReadBytes(name, out byte[] got), Is.True, "读失败: " + name);
                    Assert.That(got, Is.EqualTo(data));
                }
            }
        }

        /// <summary>
        /// 测试重点：vfs_open_paths 的 native 侧守卫——index/data 同路径
        /// （大小写不敏感、去空白）必须拒绝，C# 侧翻译为 IOException。
        /// </summary>
        [Test]
        public void OpenPaths_RejectsIndexEqualsData()
        {
            string index = Path.Combine(_dir, "index.vfs");
            Assert.Throws<IOException>(
                () => VfsReader.Open(index, index));
            Assert.Throws<IOException>(
                () => VfsReader.Open(index, "  " + index.ToUpperInvariant() + " "));
            Assert.Throws<ArgumentNullException>(() => VfsReader.Open(null, index));
        }

        /// <summary>
        /// 测试重点：软删除语义——删除后条目仍列出但 State=Deleted，
        /// 防撕裂门禁拒绝非 Active 条目的读取；Generation 已前移。
        /// </summary>
        [Test]
        public void Delete_IsSoftDeleted_AndUnreadable()
        {
            byte[] data = TestPaths.Content(8192, 1);
            IntPtr h = OpenRaw(_dir);
            try
            {
                NativeWrite(h, "doomed.bin", data);
                NativeWrite(h, "keeper.bin", data);
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }

            using (var reader = VfsReader.Open(_dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                ulong genBefore = reader.Generation;

                reader.Delete("doomed.bin");
                Assert.That(reader.Generation, Is.GreaterThan(genBefore), "删除应 bump generation");

                reader.RefreshIndex();
                reader.EnsureMapping();

                Assert.That(reader.TryGet("doomed.bin", out VfsEntry e), Is.True,
                    "压实前列表仍应包含软删除条目");
                Assert.That(e.State, Is.EqualTo(VfsFileState.Deleted));
                Assert.That(reader.TryReadBytes("doomed.bin", out _), Is.False,
                    "软删除条目必须不可读");
                Assert.That(reader.TryReadBytes("keeper.bin", out byte[] got), Is.True);
                Assert.That(got, Is.EqualTo(data));
            }
        }

        /// <summary>
        /// 测试重点：删除不存在的名字返回 NotFound(2)——错误码原样透传，
        /// 不吞不译。
        /// </summary>
        [Test]
        public void Delete_MissingName_ReturnsNotFound()
        {
            IntPtr h = OpenRaw(_dir);
            try
            {
                int err = VfsDLL.vfs_delete(h, DlmgrDLL.Utf8Bytes("no-such-file"));
                Assert.That(err, Is.EqualTo((int)VfsResult.NotFound));
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }
        }

        /// <summary>
        /// 测试重点：writer abort 的空间核算——放弃的写入进入 Garbage
        /// （逻辑大小不动而 span 作废），Active/Deleted 为字节口径
        /// （committed.bin 恰好 4096 字节），且不产生可读条目。
        /// </summary>
        [Test]
        public void WriterAbort_LeavesGarbage()
        {
            IntPtr h = OpenRaw(_dir);
            try
            {
                NativeWrite(h, "committed.bin", TestPaths.Content(4096, 9));

                IntPtr w = VfsDLL.vfs_alloc(h, DlmgrDLL.Utf8Bytes("aborted.bin"), 64 * 1024);
                Assert.That(w, Is.Not.EqualTo(IntPtr.Zero));
                VfsDLL.vfs_writer_write(w, 0, TestPaths.Content(1024, 0), 0, 1024);
                Assert.That(VfsDLL.vfs_writer_abort(w), Is.EqualTo((int)VfsResult.OK));

                ulong logical, physical, total, active, deleted, garbage;
                Assert.That(VfsDLL.vfs_stat(h, out logical, out physical, out total,
                    out active, out deleted, out garbage), Is.EqualTo((int)VfsResult.OK));
                Assert.That(garbage, Is.GreaterThan(0UL), "abort 的 span 应计入 Garbage");
                Assert.That(active, Is.EqualTo(4096UL), "Active 为已提交字节数");
                Assert.That(deleted, Is.EqualTo(0UL));

                // abort 后名字不占索引（同句柄 lookup 不可见）
                int err = VfsDLL.vfs_lookup(h, DlmgrDLL.Utf8Bytes("aborted.bin"),
                    out _, out _, out _, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.NotFound));
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }
        }

        /// <summary>
        /// 测试重点：在途写入的状态机与防撕裂语义——alloc 后条目以
        /// Downloading 态存在于索引（同句柄 lookup 可见），commit 翻转为
        /// Active 且 crc/size 落定；在途期间已 commit 数据不受影响。
        /// 注：索引为句柄私有（一次 open 一个 VFS 实例），跨句柄读取不在
        /// 契约内，故这里用同句柄 lookup 验证。
        /// </summary>
        [Test]
        public void DownloadingState_GatesRead()
        {
            byte[] data = TestPaths.Content(32 * 1024, 5);
            IntPtr h = OpenRaw(_dir);
            try
            {
                NativeWrite(h, "committed.bin", data);

                IntPtr w = VfsDLL.vfs_alloc(h, DlmgrDLL.Utf8Bytes("in-flight.bin"), (ulong)data.Length);
                Assert.That(w, Is.Not.EqualTo(IntPtr.Zero));
                VfsDLL.vfs_writer_write(w, 0, data, 0, data.Length);
                // 不 commit —— writer 在途

                int err = VfsDLL.vfs_lookup(h, DlmgrDLL.Utf8Bytes("in-flight.bin"),
                    out _, out _, out int inFlightState, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That((VfsFileState)inFlightState, Is.EqualTo(VfsFileState.Downloading),
                    "在途条目必须标记 Downloading");

                err = VfsDLL.vfs_lookup(h, DlmgrDLL.Utf8Bytes("committed.bin"),
                    out _, out _, out int committedState, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That((VfsFileState)committedState, Is.EqualTo(VfsFileState.Active),
                    "在途写入不得影响已 commit 数据（无撕裂）");

                Assert.That(VfsDLL.vfs_writer_commit(w), Is.EqualTo((int)VfsResult.OK));
                err = VfsDLL.vfs_lookup(h, DlmgrDLL.Utf8Bytes("in-flight.bin"),
                    out ulong off, out ulong size, out int state2, out uint crc);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That((VfsFileState)state2, Is.EqualTo(VfsFileState.Active));
                Assert.That(size, Is.EqualTo((ulong)data.Length));
                Assert.That(crc, Is.EqualTo(TestPaths.Crc32(data)));
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }
        }

        /// <summary>
        /// 测试重点：恢复协议（设计 §2.2）——句柄带着未 commit 的
        /// Downloading 残留被关闭后，重新打开时残留条目必须被丢弃
        /// （不可读、名字不存在），其占用的区间转为 Garbage。
        /// </summary>
        [Test]
        public void UncommittedWriter_RecoveryDropsResidue()
        {
            IntPtr h = OpenRaw(_dir);
            NativeWrite(h, "keeper.bin", TestPaths.Content(4096, 3));
            IntPtr w = VfsDLL.vfs_alloc(h, DlmgrDLL.Utf8Bytes("residue.bin"), 32 * 1024);
            Assert.That(w, Is.Not.EqualTo(IntPtr.Zero));
            VfsDLL.vfs_writer_write(w, 0, TestPaths.Content(1024, 1), 0, 1024);
            // 不 commit、不 abort，直接 close（模拟崩溃后的重开路径）
            VfsDLL.vfs_close(h);

            IntPtr h2 = OpenRaw(_dir);
            try
            {
                int err = VfsDLL.vfs_lookup(h2, DlmgrDLL.Utf8Bytes("residue.bin"),
                    out _, out _, out _, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.NotFound), "Downloading 残留应在重开时丢弃");

                err = VfsDLL.vfs_lookup(h2, DlmgrDLL.Utf8Bytes("keeper.bin"),
                    out _, out _, out int state, out _);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That((VfsFileState)state, Is.EqualTo(VfsFileState.Active), "完好数据不受残留影响");

                ulong logical, physical, total, active, deleted, garbage;
                Assert.That(VfsDLL.vfs_stat(h2, out logical, out physical, out total,
                    out active, out deleted, out garbage), Is.EqualTo((int)VfsResult.OK));
                Assert.That(garbage, Is.GreaterThan(0UL), "残留区间应计入 Garbage");
            }
            finally
            {
                VfsDLL.vfs_close(h2);
            }
        }

        /// <summary>
        /// 测试重点：索引为句柄私有（每次 open 独立 VFS 实例，flush/close 才
        /// 落盘发布）——两轮写入会话后新开句柄必须看到完整索引与全部数据，
        /// 且 EnsureMapping 幂等（物理尺寸未变时原地返回）。
        /// </summary>
        [Test]
        public void OpenReads_GrownContainerAcrossWriterSessions()
        {
            byte[] a = TestPaths.Content(64 * 1024, 11);
            byte[] b = TestPaths.Content(128 * 1024, 22);

            IntPtr h = OpenRaw(_dir);
            try { NativeWrite(h, "a.bin", a); }
            finally { VfsDLL.vfs_close(h); } // close 即 best-effort 落盘

            IntPtr h2 = OpenRaw(_dir);
            try { NativeWrite(h2, "b.bin", b); }
            finally { VfsDLL.vfs_close(h2); }

            using (var reader = VfsReader.Open(_dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                reader.EnsureMapping(); // 幂等：物理未变不重建

                Assert.That(reader.Count, Is.EqualTo(2));
                Assert.That(reader.TryReadBytes("a.bin", out byte[] gotA), Is.True);
                Assert.That(gotA, Is.EqualTo(a));
                Assert.That(reader.TryReadBytes("b.bin", out byte[] gotB), Is.True);
                Assert.That(gotB, Is.EqualTo(b));
            }
        }

        /// <summary>
        /// 测试重点：显式 Flush 的持久化承诺——flush 后完全关闭再重开，
        /// 数据与索引完整无损（崩溃安全模型的基础）。
        /// </summary>
        [Test]
        public void Flush_PersistsAcrossHandles()
        {
            byte[] data = TestPaths.Content(16 * 1024, 42);
            IntPtr h = OpenRaw(_dir);
            try
            {
                NativeWrite(h, "persist.bin", data);
                Assert.That(VfsDLL.vfs_flush(h), Is.EqualTo((int)VfsResult.OK));
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }

            using (var reader = VfsReader.Open(_dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                Assert.That(reader.TryReadBytes("persist.bin", out byte[] got), Is.True);
                Assert.That(got, Is.EqualTo(data));
            }
        }

        // 压实回调必须是 static 委托（IL2CPP/AOT 纪律；本测试类生命周期内保活）
        private static readonly ManualResetEventSlim CompactDone = new ManualResetEventSlim(false);
        private static int s_compactErr = int.MinValue;

        private static void OnCompactDone(IntPtr user, int err)
        {
            s_compactErr = err;
            CompactDone.Set();
        }

        /// <summary>
        /// 测试重点：异步压实端到端——删除 2/3 文件后压实：done 回调到达、
        /// 已删条目从索引消失、幸存数据逐字节完好、物理尺寸收缩、状态回到 Idle。
        /// </summary>
        [Test]
        public void Compact_RemovesDeleted_AndKeepsActive()
        {
            byte[] a = TestPaths.Content(64 * 1024, 1);
            byte[] b = TestPaths.Content(64 * 1024, 2);
            byte[] c = TestPaths.Content(64 * 1024, 3);

            IntPtr h = OpenRaw(_dir);
            try
            {
                NativeWrite(h, "a.bin", a);
                NativeWrite(h, "b.bin", b);
                NativeWrite(h, "c.bin", c);
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }

            using (var reader = VfsReader.Open(_dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                reader.Delete("a.bin");
                reader.Delete("b.bin");
                reader.Flush();
                ulong physicalBefore = reader.Stat().Physical;

                // Windows 无法截断被映射的文件：按文档协议，压实效实前必须先释放映射
                reader.ReleaseMapping();

                CompactDone.Reset();
                s_compactErr = int.MinValue;
                reader.Compact(0, null, OnCompactDone, IntPtr.Zero);
                Assert.That(CompactDone.Wait(60_000), Is.True, "压实 done 回调未在 60s 内到达");
                Assert.That(s_compactErr, Is.EqualTo((int)VfsResult.OK), "压实报告错误");

                // done 后按约定：先 RefreshIndex 再 EnsureMapping（重建映射）
                reader.RefreshIndex();
                reader.EnsureMapping();

                Assert.That(reader.TryGet("a.bin", out _), Is.False, "已删 a.bin 应从索引消失");
                Assert.That(reader.TryGet("b.bin", out _), Is.False, "已删 b.bin 应从索引消失");
                Assert.That(reader.TryReadBytes("c.bin", out byte[] gotC), Is.True);
                Assert.That(gotC, Is.EqualTo(c), "幸存数据损坏");

                VfsStatInfo st = reader.Stat();
                Assert.That(st.Physical, Is.LessThan(physicalBefore), "物理尺寸应收缩");
                Assert.That(reader.CompactStatus(out int percent), Is.EqualTo(VfsCompactState.Idle));
            }
        }

        /// <summary>
        /// 测试重点：GenChanged 收敛与同句柄跨线程安全——后台线程经同一
        /// VfsReader 持续 Delete（native 侧互斥串行化，generation 前进），
        /// 主线程持续 RefreshIndex：query 与 read 之间 gen 前进时必须返回
        /// GEN_CHANGED 并由有界重试收敛，最终索引恰好等于剩余条目数。
        /// </summary>
        [Test]
        public void RefreshIndex_ConvergesUnderConcurrentDeletes()
        {
            const int FileCount = 200;
            IntPtr h = OpenRaw(_dir);
            try
            {
                for (int i = 0; i < FileCount; i++)
                {
                    NativeWrite(h, "gen-" + i, TestPaths.Content(256, (byte)i));
                }
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }

            var deletError = new ConcurrentQueue<string>();
            var deleterDone = new ManualResetEventSlim(false);

            using (var reader = VfsReader.Open(_dir))
            {
                reader.RefreshIndex();
                reader.EnsureMapping();
                Assert.That(reader.Count, Is.EqualTo(FileCount));

                var thread = new Thread(() =>
                {
                    try
                    {
                        for (int i = 0; i < FileCount; i++)
                        {
                            reader.Delete("gen-" + i);
                        }
                    }
                    catch (Exception ex)
                    {
                        deletError.Enqueue(ex.Message);
                    }
                    finally
                    {
                        deleterDone.Set();
                    }
                });
                thread.Start();

                int rounds = 0;
                try
                {
                    while (!deleterDone.Wait(0))
                    {
                        reader.RefreshIndex(); // 内部对 GenChanged 有界重试
                        reader.EnsureMapping();
                        rounds++;
                        Assert.That(rounds, Is.LessThan(100_000), "刷新循环失控");
                        Thread.Sleep(1);
                    }
                    deleterDone.Wait(30_000);
                    reader.RefreshIndex();
                    reader.EnsureMapping();

                    Assert.That(deletError.IsEmpty, Is.True, "删除线程异常: " +
                        (deletError.IsEmpty ? "" : string.Join("; ", deletError)));
                    // 软删除不移除条目（压实才移除）：终态 = 全部条目仍在但均为 Deleted
                    Assert.That(reader.Count, Is.EqualTo(FileCount), "索引条目数不变");
                    for (int i = 0; i < FileCount; i++)
                    {
                        Assert.That(reader.TryGet("gen-" + i, out VfsEntry e), Is.True);
                        Assert.That(e.State, Is.EqualTo(VfsFileState.Deleted), "gen-" + i + " 应为 Deleted");
                    }
                    Assert.That(rounds, Is.GreaterThan(0), "并发期应发生过刷新");
                }
                finally
                {
                    deleterDone.Wait(30_000);
                    thread.Join(30_000);
                }
            }
        }

        // commit 回调保活字段（native 存裸函数指针，委托必须活到回调之后）；
        // 委托类型必须就是 ABI 声明的 VfsCommitDelegate（P/Invoke 类型一致性）
        private static readonly ConcurrentQueue<(string name, ulong off, ulong size, uint crc)> CommitEvents
            = new ConcurrentQueue<(string, ulong, ulong, uint)>();

        private static void OnCommit(IntPtr user, IntPtr name, ulong off, ulong size, uint crc)
        {
            CommitEvents.Enqueue((System.Runtime.InteropServices.Marshal.PtrToStringUTF8(name), off, size, crc));
        }

        /// <summary>
        /// 测试重点：commit 回调契约——同句柄 commit 时回调恰好触发一次，
        /// 携带正确的文件名（UTF-8 反解）/offset/size/crc。
        /// </summary>
        [Test]
        public void CommitCallback_FiresWithNameSizeCrc()
        {
            byte[] data = TestPaths.Content(8 * 1024, 77);
            IntPtr h = OpenRaw(_dir);
            try
            {
                var cb = new VfsCommitDelegate(OnCommit);
                Assert.That(VfsDLL.vfs_set_commit_callback(h, cb, IntPtr.Zero),
                    Is.EqualTo((int)VfsResult.OK));

                NativeWrite(h, "notify.bin", data);

                Assert.That(CommitEvents.TryDequeue(out var ev), Is.True, "commit 回调未触发");
                Assert.That(ev.name, Is.EqualTo("notify.bin"));
                Assert.That(ev.size, Is.EqualTo((ulong)data.Length));
                Assert.That(ev.crc, Is.EqualTo(TestPaths.Crc32(data)));
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }
        }

        /// <summary>
        /// 测试重点：enumerate 两段式的容量契约——read 阶段传入小于 query
        /// 返回条目数的 cap 时，必须拒绝（BufferTooSmall）而不是静默截断。
        /// </summary>
        [Test]
        public void EnumerateRead_TooSmallCap_ReturnsBufferTooSmall()
        {
            IntPtr h = OpenRaw(_dir);
            try
            {
                for (int i = 0; i < 3; i++)
                {
                    NativeWrite(h, "e" + i, TestPaths.Content(128, (byte)i));
                }

                int err = VfsDLL.vfs_enumerate_query(h, out ulong gen, out uint count, out uint blobLen);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK));
                Assert.That(count, Is.EqualTo(3U));

                byte[] names = new byte[blobLen];
                ulong[] offs = new ulong[count];
                ulong[] sizes = new ulong[count];
                uint[] crcs = new uint[count];
                int[] states = new int[count];

                err = VfsDLL.vfs_enumerate_read(h, gen, names, offs, sizes, crcs, states, (UIntPtr)(count - 1));
                Assert.That(err, Is.EqualTo((int)VfsResult.BufferTooSmall),
                    "cap < count 必须返回 BufferTooSmall");

                err = VfsDLL.vfs_enumerate_read(h, gen, names, offs, sizes, crcs, states, (UIntPtr)count);
                Assert.That(err, Is.EqualTo((int)VfsResult.OK), "cap == count 应成功");
            }
            finally
            {
                VfsDLL.vfs_close(h);
            }
        }
    }
}
