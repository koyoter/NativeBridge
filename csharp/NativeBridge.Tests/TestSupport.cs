//
// TestSupport.cs — 测试基础设施：全局 curl 初始化 + 临时目录/内容工具。
//
using System;
using System.IO;
using System.IO.Hashing;
using System.Text;
using NUnit.Framework;

namespace NativeBridgeF.Tests
{
    /// <summary>
    /// 程序集级一次性初始化：dlmgr / curlw 的宿主约定是 Start 前必须
    /// curlw_global_init（见 DownloadManager.cs 头注释），全程只做一次。
    /// </summary>
    [SetUpFixture]
    public class AssemblyLifecycle
    {
        [OneTimeSetUp]
        public void GlobalInit()
        {
            CurlwDLL.curlw_global_init(CURLDefines.CURL_GLOBAL_DEFAULT, 32);
        }

        [OneTimeTearDown]
        public void GlobalCleanup()
        {
            CurlwDLL.curlw_global_cleanup();
        }
    }

    /// <summary>每个测试独立的临时目录与通用断言工具。</summary>
    internal static class TestPaths
    {
        public static string NewDir()
        {
            string dir = Path.Combine(Path.GetTempPath(), "nbtest-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            return dir;
        }

        public static byte[] Content(int size, byte seed)
        {
            byte[] data = new byte[size];
            for (int i = 0; i < data.Length; i++) data[i] = (byte)(seed + i);
            return data;
        }

        public static uint Crc32(byte[] data)
        {
            var crc = new Crc32();
            crc.Append(data);
            return crc.GetCurrentHashAsUInt32();
        }
    }
}
