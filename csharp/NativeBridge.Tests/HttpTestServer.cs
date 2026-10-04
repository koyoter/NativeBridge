//
// HttpTestServer.cs — 进程内最小 HTTP 测试服务器，供 Dlmgr/Curlw 真下载测试使用。
//
// 能力：按路径伺服固定字节；支持 Range: bytes=N-（206 + Content-Range）；
//       可配置"永远 200"（模拟不支持 Range 的源，触发 dual-URL 回退）；
//       可配置慢速分块发送（供取消测试抢在完成前 cancel）。
// ponytail: HttpListener 单线程顺序处理；并发多任务测试出现时再上 ThreadPool。
//
using System;
using System.Collections.Generic;
using System.Net;
using System.Text;
using System.Threading;

namespace NativeBridgeF.Tests
{
    internal sealed class HttpTestServer : IDisposable
    {
        private sealed class Route
        {
            public byte[] Body;
            public bool Force200;       // 忽略 Range，永远回 200 全量
            public int SlowMs;          // 每块之间的停顿（0 = 尽快）
            public int Chunk;           // 慢速模式的块大小
            public int DropRequests;    // 前 N 次请求传输中途掐断（瞬时故障），之后正常
            public int Seen;            // 已收到的请求计数（与 DropRequests 配合）
        }

        private readonly HttpListener _listener = new HttpListener();
        private readonly Dictionary<string, Route> _routes = new Dictionary<string, Route>(StringComparer.Ordinal);
        private readonly Thread _thread;
        private volatile bool _running = true;

        public string BaseUrl { get; }

        public HttpTestServer()
        {
            int port = FreePort();
            BaseUrl = "http://127.0.0.1:" + port + "/";
            _listener.Prefixes.Add(BaseUrl);
            _listener.Start();
            _thread = new Thread(Loop) { IsBackground = true };
            _thread.Start();
        }

        private static int FreePort()
        {
            var l = System.Net.Sockets.TcpListener.Create(0);
            l.Start();
            int port = ((IPEndPoint)l.LocalEndpoint).Port;
            l.Stop();
            return port;
        }

        /// <summary>注册路径。返回完整 URL。dropRequests &gt; 0 时前 N 次请求写一半后断连。</summary>
        public string Add(string path, byte[] body, bool force200 = false, int slowMs = 0, int chunk = 16 * 1024, int dropRequests = 0)
        {
            _routes[path.TrimStart('/')] = new Route
            {
                Body = body, Force200 = force200, SlowMs = slowMs, Chunk = chunk, DropRequests = dropRequests
            };
            return BaseUrl + path.TrimStart('/');
        }

        private void Loop()
        {
            while (_running)
            {
                HttpListenerContext ctx;
                try { ctx = _listener.GetContext(); }
                catch { break; } // Stop() 时抛异常退出
                // 并发处理：慢速响应不得堵塞其他连接（并发测试的前提）
                var captured = ctx;
                ThreadPool.QueueUserWorkItem(_ =>
                {
                    try { Handle(captured); }
                    catch { try { captured.Response.Abort(); } catch { } }
                });
            }
        }

        private void Handle(HttpListenerContext ctx)
        {
            string path = ctx.Request.Url.AbsolutePath.TrimStart('/');
            if (!_routes.TryGetValue(path, out Route route))
            {
                ctx.Response.StatusCode = 404;
                ctx.Response.Close();
                return;
            }

            byte[] body = route.Body;
            string range = ctx.Request.Headers["Range"];
            bool rangeRequested = range != null &&
                                  range.StartsWith("bytes=", StringComparison.OrdinalIgnoreCase);
            long start = 0;
            if (rangeRequested)
            {
                string spec = range.Substring("bytes=".Length);
                int dash = spec.IndexOf('-');
                string startStr = dash > 0 ? spec.Substring(0, dash) : "";
                if (startStr.Length > 0) long.TryParse(startStr, out start);
                if (start < 0 || start >= body.Length) start = 0;
            }

            // Range 请求且未配置 Force200 → 206 切片（start=0 也回 206：
            // worker 用 "Range: bytes=0-" 探测 range_url，200 会触发回退）。
            // Force200 → 永远 200 全量，模拟不支持 Range 的源。
            bool partial = rangeRequested && !route.Force200;
            ctx.Response.StatusCode = partial ? 206 : 200;
            ctx.Response.ContentType = "application/octet-stream";
            if (partial)
            {
                ctx.Response.Headers["Content-Range"] =
                    "bytes " + start + "-" + (body.Length - 1) + "/" + body.Length;
            }
            int from = partial ? (int)start : 0;
            ctx.Response.ContentLength64 = body.Length - from;

            // 瞬时故障模拟：头部承诺全量、只发一半后强断连接
            // （curl 报网络错误 → worker 退避重试 / 耗尽预算 → Failed）
            if (route.DropRequests > 0)
            {
                int seen = Interlocked.Increment(ref route.Seen);
                if (seen <= route.DropRequests)
                {
                    int half = (int)((body.Length - from) / 2);
                    if (half > 0) ctx.Response.OutputStream.Write(body, from, half);
                    ctx.Response.OutputStream.Flush();
                    ctx.Response.Abort();
                    return;
                }
            }

            if (route.SlowMs > 0)
            {
                // 慢速分块（206/200 都生效）：给取消测试留出窗口；客户端断开时抛异常结束
                int chunk = route.Chunk;
                for (int off = from; off < body.Length; off += chunk)
                {
                    if (!_running) throw new OperationCanceledException();
                    int n = Math.Min(chunk, body.Length - off);
                    ctx.Response.OutputStream.Write(body, off, n);
                    Thread.Sleep(route.SlowMs);
                }
            }
            else
            {
                ctx.Response.OutputStream.Write(body, from, (int)(body.Length - from));
            }
            ctx.Response.OutputStream.Flush();
            ctx.Response.Close();
        }

        public void Dispose()
        {
            _running = false;
            try { _listener.Stop(); } catch { }
            _thread.Join(2000);
            ((IDisposable)_listener).Dispose();
        }
    }
}
