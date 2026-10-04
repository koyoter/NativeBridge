//
// CurlwTests.cs — CurlwDLL 冒烟测试：防 ABI 漂移，不深测 curl 语义。
//
// 覆盖：abi/version_info、easy 阻塞式 GET 全链路（含 WRITEFUNCTION 委托
// 与 getinfo）、multi 轮询循环 GET（Unity 主循环驱动模式的等价物）。
// 深度语义由 curl 本身与 Rust 侧测试保证，这里只验证绑定层不失真。
//
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
using NUnit.Framework;

namespace NativeBridgeF.Tests
{
    [TestFixture]
    public class CurlwTests
    {
        private HttpTestServer _server;

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

        // --- helpers -------------------------------------------------------------

        private static IntPtr Utf8Ptr(string s)
        {
            return Marshal.StringToCoTaskMemUTF8(s);
        }

        // 写回调：收集响应体（签名必须与 CurlwWriteDataDelegate 一致）
        private static UIntPtr WriteBody(IntPtr content, UIntPtr size, UIntPtr nmemb, IntPtr userdata)
        {
            var sink = (List<byte>)((GCHandle)userdata).Target;
            int n = (int)(size * nmemb);
            byte[] buf = new byte[n];
            Marshal.Copy(content, buf, 0, n);
            sink.AddRange(buf);
            return (UIntPtr)n;
        }

        /// <summary>给 easy 句柄设 URL + 写回调（响应体经 GCHandle 收集）。</summary>
        private static void SetBodySink(IntPtr easy, IntPtr urlPtr, GCHandle h)
        {
            Assert.That(CurlwDLL.curlw_easy_setopt_pointer(easy, CURLoption.CURLOPT_URL, urlPtr),
                        Is.EqualTo(CURLcode.CURLE_OK));
            Assert.That(CurlwDLL.curlw_easy_setopt_pointer(easy, CURLoption.CURLOPT_WRITEDATA,
                        GCHandle.ToIntPtr(h)), Is.EqualTo(CURLcode.CURLE_OK));
            Assert.That(CurlwDLL.curlw_easy_setopt_pointer(
                        easy, CURLoption.CURLOPT_WRITEFUNCTION,
                        (CurlwWriteDataDelegate)WriteBody), Is.EqualTo(CURLcode.CURLE_OK));
        }

        // --- tests -----------------------------------------------------------------

        /// <summary>
        /// 测试重点：curlw ABI 版本守卫为 1，version_info 指针有效且
        /// 版本号非零——最低成本的"库活着"冒烟。
        /// </summary>
        [Test]
        public void AbiVersion_AndVersionInfo()
        {
            Assert.That(CurlwDLL.curlw_abi_version(), Is.EqualTo(1));
            IntPtr verinfo = CurlwDLL.curlw_version_info();
            Assert.That(verinfo, Is.Not.EqualTo(IntPtr.Zero));
            Assert.That(CurlwDLL.curlw_verinfo_version_num(verinfo), Is.Not.EqualTo(0U));
        }

        /// <summary>
        /// 测试重点：easy 阻塞式 GET 全链路——setopt(URL/WRITEDATA/
        /// WRITEFUNCTION 委托) → perform → 响应体逐字节一致、
        /// RESPONSE_CODE == 200。委托跨 ABI 调用不失真。
        /// </summary>
        [Test]
        public void Easy_PerformGetsBody()
        {
            byte[] data = TestPaths.Content(256 * 1024, 33);
            string url = _server.Add("easy.bin", data);
            IntPtr urlPtr = Utf8Ptr(url);
            var body = new List<byte>();
            GCHandle h = GCHandle.Alloc(body);
            try
            {
                IntPtr easy = CurlwDLL.curlw_easy_init();
                Assert.That(easy, Is.Not.EqualTo(IntPtr.Zero));
                try
                {
                    SetBodySink(easy, urlPtr, h);
                    Assert.That(CurlwDLL.curlw_easy_perform(easy), Is.EqualTo(CURLcode.CURLE_OK));
                    Assert.That(CurlwDLL.curlw_easy_getinfo_long(easy, CURLINFO.CURLINFO_RESPONSE_CODE,
                                out long code), Is.EqualTo(CURLcode.CURLE_OK));
                    Assert.That(code, Is.EqualTo(200L));
                }
                finally
                {
                    CurlwDLL.curlw_easy_cleanup(easy);
                }
                Assert.That(body, Is.EqualTo(new List<byte>(data)));
            }
            finally
            {
                h.Free();
                Marshal.FreeCoTaskMem(urlPtr);
            }
        }

        /// <summary>
        /// 测试重点：multi 轮询循环（Unity 主循环模式的等价物）——
        /// multi_add_handle → 循环 multi_perform 至 running==0 →
        /// multi_info_read 取到 CURLE_OK 与响应体。验证 multi 系列
        /// 绑定与消息提取链路。
        /// </summary>
        [Test]
        public void Multi_PerformLoopGetsBody()
        {
            byte[] data = TestPaths.Content(128 * 1024, 44);
            string url = _server.Add("multi.bin", data);
            IntPtr urlPtr = Utf8Ptr(url);
            var body = new List<byte>();
            GCHandle h = GCHandle.Alloc(body);
            try
            {
                IntPtr easy = CurlwDLL.curlw_easy_init();
                Assert.That(easy, Is.Not.EqualTo(IntPtr.Zero));
                IntPtr multi = CurlwDLL.curlw_multi_init();
                Assert.That(multi, Is.Not.EqualTo(IntPtr.Zero));
                try
                {
                    SetBodySink(easy, urlPtr, h);
                    Assert.That(CurlwDLL.curlw_multi_add_handle(multi, easy),
                                Is.EqualTo(CURLMcode.CURLM_OK));

                    var deadline = DateTime.UtcNow.AddSeconds(30);
                    int running = 1;
                    while (running > 0 && DateTime.UtcNow < deadline)
                    {
                        Assert.That(CurlwDLL.curlw_multi_perform(multi, out running),
                                    Is.EqualTo(CURLMcode.CURLM_OK));
                        if (running > 0) Thread.Sleep(10);
                    }
                    Assert.That(running, Is.EqualTo(0), "multi_perform 未在限时内完成");

                    bool gotMsg = false;
                    IntPtr msg;
                    while ((msg = CurlwDLL.curlw_multi_info_read(multi, out int remaining)) != IntPtr.Zero)
                    {
                        // 每条消息：取 easy 句柄与最终结果，完成消息必须 CURLE_OK
                        Assert.That(CurlwDLL.curlw_msg_get_easy_handle(msg), Is.EqualTo(easy));
                        Assert.That(CurlwDLL.curlw_msg_get_result(msg), Is.EqualTo(CURLcode.CURLE_OK));
                        gotMsg = true;
                    }
                    Assert.That(gotMsg, Is.True, "multi_info_read 未返回完成消息");
                    Assert.That(body, Is.EqualTo(new List<byte>(data)));
                }
                finally
                {
                    CurlwDLL.curlw_multi_remove_handle(multi, easy);
                    CurlwDLL.curlw_easy_cleanup(easy);
                    CurlwDLL.curlw_multi_cleanup(multi);
                }
            }
            finally
            {
                h.Free();
                Marshal.FreeCoTaskMem(urlPtr);
            }
        }
    }
}
