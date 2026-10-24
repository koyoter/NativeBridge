//
// DownloadManager.cs — C# binding for the NativeBridge "dlmgr" module.
//
// This is the managed mirror of the native contract exported by
//   src/nativebridge/rust/src/modules/dlmgr/mod.rs
// Keep the two in lock-step: every P/Invoke below matches a #[no_mangle]
// export 1:1 (cdecl calling convention, same argument widths).
//
// Design notes:
//   * namespace NativeBridgeF, LIBNAME "NativeBridge" (native library file name),
//     same as Curlw.cs / Vfs.cs.
//   * The manager runs N native worker threads (one blocking curl easy each,
//     single connection per task) plus a 10 Hz reporter thread that fires the
//     callbacks: per-task first (only on state/progress change, terminal
//     states reported at the transition tick), then the global callback
//     (every tick). Callbacks fire on the native reporter thread (sink/commit
//     callbacks may also fire on a native worker thread).
//   * strings cross the ABI as UTF-8: string params are encoded via Utf8Bytes
//     (NUL-terminated byte[]) and returned char* values are read via
//     PtrToStringUtf8. Never use CharSet.Ansi — on Windows it converts through
//     the system ANSI code page (e.g. GBK) while the native side expects UTF-8.
//   * Callback keep-alive: the native side stores RAW function pointers. The
//     DlmgrDLL wrappers below keep static strong references to every delegate
//     they register, but user-supplied delegates MUST be static +
//     [MonoPInvokeCallback(typeof(DlmgrTaskCallback))] on IL2CPP/AOT and stay
//     referenced for the manager lifetime. Dispatch to the main thread yourself
//     if needed; never let managed exceptions escape a native callback.
//   * Sinks: a sink is a u64 handle from a native sink registry (glue bridges
//     VFS-backed sinks via the Rust sink vtable; DlmgrSinkCreateFile gives
//     plain-file ones). Release a sink only after its task reported a
//     terminal state (Done / Failed / Canceled).
//   * Lifecycle: Create -> (SetCaBlob | SetHttpVersion | AddDohUrl)* -> Start ->
//     Enqueue* -> Shutdown. Successful shutdown consumes the handle (cancel all
//     + join). Shutdown from ANY native dlmgr worker/reporter thread is rejected
//     without consuming it; callbacks should use dlmgr_shutdown_try and request
//     external shutdown. curlw_global_init is the host's responsibility and must
//     be called before Create (dlmgr_create internally calls curl_share_init).
//   * Dual-URL semantics (native): range_url is probed with
//     "Range: bytes=0-"; a non-206 answer, Content-Range total mismatch or
//     crc mismatch switches the task to full_url from zero ONCE (not
//     consuming retries); disconnects resume with "Range: bytes=off-"
//     in-session; full_url failures consume the retry budget with 1s/2s/4s
//     exponential backoff, then the task fails.
//   * Priority (native): enqueue-time ordering only (larger first, FIFO tie,
//     no preemption). Queued tasks can be re-prioritized via
//     dlmgr_set_priority; running/retired tasks return -1.
//
#if !UNITY_WEBGL
using System;
using System.Runtime.InteropServices;
using System.Text;

namespace NativeBridgeF
{
    // --- enums (values mirror the native ABI) -----------------------------------------
    public enum DlmgrTaskState
    {
        Pending = 0,
        Running = 1,
        Verifying = 2,
        Done = 3,
        Failed = 4,
        Canceled = 5,
    }

    public enum DlmgrTaskError
    {
        OK = 0,
        UnsupportedRange = 1,
        SizeMismatch = 2,
        CrcMismatch = 3,
        Network = 4,
        Timeout = 5,
        Canceled = 6,
        Busy = 7,
    }

    // CURLOPT_HTTP_VERSION passthrough values (mirror curl.h). None = curl
    // negotiates; V3 prefers HTTP/3 with fallback, V3Only never falls back
    // (needs server Alt-Svc/QUIC support).
    public enum CurlHttpVersion
    {
        None = 0,
        V10 = 1,
        V11 = 2,
        V2 = 3,
        V2Tls = 4,
        V2PriorKnowledge = 5,
        V3 = 30,
        V3Only = 31,
    }

    // --- callbacks ---------------------------------------------------------------
    // Task callback: cb(user, task_id, state, done, total, bps, err).
    // MUST be static + [MonoPInvokeCallback(typeof(DlmgrTaskCallback))] on
    // IL2CPP/AOT; the delegate instance must stay referenced (DlmgrDLL pins the
    // one it registers). Fires on the native reporter thread.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate void DlmgrTaskCallback(IntPtr user, ulong taskId, int state, ulong done,
                                           ulong total, ulong bps, int err);

    // Global callback: cb(user, active, done_cnt, failed_cnt, bytes_done,
    // bytes_total, bps). Fired every ~100 ms after the per-task callbacks.
    // MUST be static + [MonoPInvokeCallback(typeof(DlmgrGlobalCallback))].
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate void DlmgrGlobalCallback(IntPtr user, uint active, uint doneCount,
                                             uint failedCount, ulong bytesDone, ulong bytesTotal,
                                             ulong bps);

    /// <summary>
    /// Per-native-handle strong references to callback delegates: the native side
    /// stores raw function pointers, so a registered managed delegate must outlive
    /// its registration (GC of the thunk crashes the next native callback). One
    /// instance per binding class (CurlwDLL / DlmgrDLL) — state must not mix
    /// across libraries. Null delegates are a no-op (clear-registration call).
    /// </summary>
    internal sealed class DelegateRoots
    {
        private readonly System.Collections.Generic.Dictionary<IntPtr, System.Collections.Generic.List<Delegate>>
            _byHandle = new System.Collections.Generic.Dictionary<IntPtr, System.Collections.Generic.List<Delegate>>();
        private readonly object _lock = new object();

        public void Add(IntPtr handle, Delegate cb)
        {
            if (cb == null) return;
            lock (_lock)
            {
                System.Collections.Generic.List<Delegate> list;
                if (!_byHandle.TryGetValue(handle, out list))
                {
                    _byHandle[handle] = list = new System.Collections.Generic.List<Delegate>();
                }
                list.Add(cb);
            }
        }

        /// <summary>Drops a handle's roots (easy cleanup/reset, manager shutdown).</summary>
        public void Remove(IntPtr handle)
        {
            lock (_lock) { _byHandle.Remove(handle); }
        }

        /// <summary>Handle's live roots (null if none) — snapshot for a later Install.</summary>
        public System.Collections.Generic.List<Delegate> Snapshot(IntPtr handle)
        {
            lock (_lock)
            {
                return _byHandle.TryGetValue(handle, out System.Collections.Generic.List<Delegate> list) ? list : null;
            }
        }

        /// <summary>Installs a copy of a snapshot under a new handle (easy_duphandle).</summary>
        public void Install(IntPtr handle, System.Collections.Generic.List<Delegate> roots)
        {
            if (roots == null || handle == IntPtr.Zero) return;
            lock (_lock) { _byHandle[handle] = new System.Collections.Generic.List<Delegate>(roots); }
        }
    }

    /// <summary>
    /// Thin 1:1 binding of the native dlmgr C ABI (see modules/dlmgr/mod.rs).
    /// </summary>
    public static class DlmgrDLL
    {
#if (UNITY_IOS || UNITY_TVOS) && !UNITY_EDITOR
        public const string LIBNAME = "__Internal";
#else
        public const string LIBNAME = "NativeBridge";
#endif

        // --- UTF-8 string marshalling ----------------------------------------
        // byte[] parameters are copied verbatim by the P/Invoke marshaler (no
        // code page conversion), so a NUL-terminated UTF-8 buffer is what the
        // native side sees. Keep every string-taking DllImport below on byte[]
        // and expose only string overloads that go through this helper. It is
        // shared assembly-wide (Vfs.cs / DlVfs use it too).
        internal static byte[] Utf8Bytes(string s)
        {
            byte[] buf = new byte[Encoding.UTF8.GetByteCount(s) + 1];
            Encoding.UTF8.GetBytes(s, 0, s.Length, buf, 0); // trailing byte stays 0
            return buf;
        }

        // --- version / ABI ---------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_abi_version();

        // --- lifecycle ---------------------------------------------------------
        // workerCount is clamped to 1..=64; globalBps == 0 = unlimited;
        // retryCount is the retry budget (default 3). Returns IntPtr.Zero on failure.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr dlmgr_create(uint workerCount, ulong globalBps, uint retryCount);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_start(IntPtr mgr);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_add_doh_url")]
        private static extern int dlmgr_add_doh_url_imp(IntPtr mgr, byte[] url);

        // UTF-8 safe (see Utf8Bytes). Apply before Start for guaranteed effect.
        public static int dlmgr_add_doh_url(IntPtr mgr, string url)
        {
            return dlmgr_add_doh_url_imp(mgr, url == null ? null : Utf8Bytes(url));
        }

        // CA certificate bundle (PEM) for CURLOPT_CAINFO_BLOB. The BoringSSL
        // build has no compiled-in default CA path, so on iOS/Android/tvOS HTTPS
        // fails the handshake unless the host supplies one (e.g. read a PEM from
        // StreamingAssets and pass its bytes). Snapshotted when workers are
        // created — apply before Start; passing null clears it.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_set_ca_blob(IntPtr mgr, byte[] data, UIntPtr len);

        public static int dlmgr_set_ca_blob(IntPtr mgr, byte[] ca)
        {
            return dlmgr_set_ca_blob(mgr, ca, (UIntPtr)(ca?.LongLength ?? 0));
        }

        // CURLOPT_HTTP_VERSION passthrough (see CurlHttpVersion). Snapshotted
        // when workers are created — apply before Start. 0 = curl default.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_set_http_version(IntPtr mgr, long version);

        public static int dlmgr_set_http_version(IntPtr mgr, CurlHttpVersion version)
        {
            return dlmgr_set_http_version(mgr, (long)version);
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_shutdown_try")]
        private static extern int dlmgr_shutdown_try_imp(IntPtr mgr);

        /// <summary>
        /// Returns 0 after canceling tasks, joining all threads and consuming the
        /// handle (IntPtr.Zero is a successful no-op). Returns -1 on ANY dlmgr
        /// worker/reporter thread, including sink/commit callbacks, without changing
        /// the manager or releasing callback roots. This also forbids cross-manager
        /// shutdown from callbacks to prevent join cycles.
        /// Use this API in callbacks: rejection does not throw. Request shutdown on
        /// an external thread and return; do not wait for shutdown inside a callback.
        /// The caller must serialize external shutdown with other handle use,
        /// including Start and another shutdown. Do not reuse a consumed handle.
        /// </summary>
        public static int dlmgr_shutdown_try(IntPtr mgr)
        {
            int result = dlmgr_shutdown_try_imp(mgr);
            if (result == 0)
            {
                // Native joins have completed: no callback can still be in flight.
                // Roots drop only AFTER the native call: never hold the roots lock
                // across it (callbacks may re-enter).
                s_roots.Remove(mgr);
            }
            return result;
        }

        /// <summary>
        /// Consumes the handle on success; throws InvalidOperationException when
        /// called from any dlmgr internal thread, leaving the handle and callback
        /// roots intact. Callbacks MUST use dlmgr_shutdown_try instead so rejection
        /// cannot throw across the unmanaged boundary.
        /// </summary>
        public static void dlmgr_shutdown(IntPtr mgr)
        {
            if (dlmgr_shutdown_try(mgr) != 0)
            {
                throw new InvalidOperationException(
                    "Shutdown is forbidden on dlmgr worker/reporter threads. " +
                    "Use dlmgr_shutdown_try in callbacks and shut down from an external thread.");
            }
        }

        // --- tasks ---------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_enqueue")]
        private static extern int dlmgr_enqueue_imp(IntPtr mgr, byte[] rangeUrl, byte[] fullUrl,
                                                    byte[] name, ulong size, uint crc32, int priority,
                                                    ulong taskBps, ulong sink, out ulong taskId);

        // fullUrl may be null/empty = no fallback. Returns 0 and writes taskId on
        // success, -1 on failure.
        public static int dlmgr_enqueue(IntPtr mgr, string rangeUrl, string fullUrl, string name,
                                        ulong size, uint crc32, int priority, ulong taskBps,
                                        ulong sink, out ulong taskId)
        {
            return dlmgr_enqueue_imp(mgr,
                                     rangeUrl == null ? null : Utf8Bytes(rangeUrl),
                                     fullUrl == null ? null : Utf8Bytes(fullUrl),
                                     name == null ? null : Utf8Bytes(name),
                                     size, crc32, priority, taskBps, sink, out taskId);
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_cancel(IntPtr mgr, ulong taskId);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_cancel_all(IntPtr mgr);

        // 动态改排队中任务的下载权重（priority 大者先派发、同级 FIFO、不抢占）。
        // 仅排队中的任务可改：运行中不可抢占、终态已退役、id 不存在，返回 -1。
        // paused 期间同样生效。
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_set_priority(IntPtr mgr, ulong taskId, int priority);

        // Stops dispatching new tasks; running tasks run to completion. Use
        // dlmgr_active_count == 0 as the compaction precondition.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_pause(IntPtr mgr);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_resume(IntPtr mgr);

        // Global speed limit, effective immediately; bps == 0 = unlimited.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int dlmgr_set_global_speed(IntPtr mgr, ulong bps);

        // --- callbacks (keep-alive wrappers) -------------------------------------
        // The native side stores raw function pointers, so the managed delegate
        // MUST survive as long as it is registered. References are kept per
        // manager handle via DelegateRoots (same discipline as Curlw.cs s_roots):
        // a process-wide single slot would drop a live delegate when a second
        // manager registers or a callback is re-registered, and the GC'd thunk
        // would crash the next native callback.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_set_task_callback")]
        private static extern int dlmgr_set_task_callback_imp(IntPtr mgr, DlmgrTaskCallback cb, IntPtr user);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_set_global_callback")]
        private static extern int dlmgr_set_global_callback_imp(IntPtr mgr, DlmgrGlobalCallback cb, IntPtr user);

        private static readonly DelegateRoots s_roots = new DelegateRoots();

        // cb == null clears the registration natively. The handle's PREVIOUS
        // delegates are deliberately kept alive: a callback may still be in
        // flight on the reporter thread when the slot is cleared (bounded by
        // manager count, freed on shutdown).
        public static int dlmgr_set_task_callback(IntPtr mgr, DlmgrTaskCallback cb, IntPtr user)
        {
            s_roots.Add(mgr, cb);
            return dlmgr_set_task_callback_imp(mgr, cb, user);
        }

        public static int dlmgr_set_global_callback(IntPtr mgr, DlmgrGlobalCallback cb, IntPtr user)
        {
            s_roots.Add(mgr, cb);
            return dlmgr_set_global_callback_imp(mgr, cb, user);
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern uint dlmgr_active_count(IntPtr mgr);

        // --- sinks -----------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_sink_create_file")]
        private static extern ulong dlmgr_sink_create_file_imp(IntPtr mgr, byte[] path, ulong size);

        // Plain-file sink (one OS handle per sink, pwrite at absolute offsets).
        // Returns a u64 sink handle for dlmgr_enqueue, 0 on failure. Release it
        // only after the task reported a terminal state.
        public static ulong dlmgr_sink_create_file(IntPtr mgr, string path, ulong size)
        {
            return dlmgr_sink_create_file_imp(mgr, path == null ? null : Utf8Bytes(path), size);
        }

        // Releases a sink handle (plain-file or VFS-backed via DlVfs below). Call
        // exactly once, only after the task reported Done / Failed / Canceled.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void dlmgr_sink_release(ulong sink);
    }

    /// <summary>
    /// glue-layer binding: download-into-VFS sink (bridges dlmgr's sink vtable
    /// to a VFS writer; see modules/glue.rs). The native sink holds its own
    /// reference to the VFS, so an early vfs_close/Dispose no longer invalidates
    /// it; still, keep the reader/handle alive as good practice until every sink
    /// created from it has been released after task termination.
    /// </summary>
    public static class DlVfs
    {
        public const string LIBNAME = DlmgrDLL.LIBNAME;

        // Creates a sink that downloads `name` into the VFS `vfs` (handle from
        // VfsDLL.vfs_open). `size` is ignored (ABI placeholder); the task's size
        // is enforced by the sink's prepare when the download starts.
        // Returns a u64 sink handle for dlmgr_enqueue, 0 on failure. Release it
        // with DlmgrDLL.dlmgr_sink_release after the task's terminal state;
        // on finish the VFS entry is committed (visible immediately to readers).
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern ulong dlvfs_sink_create_for_vfs(IntPtr vfs, byte[] name, ulong size);

        // UTF-8 safe wrapper (see DlmgrDLL.Utf8Bytes).
        public static ulong dlvfs_sink_create_for_vfs(IntPtr vfs, string name, ulong size)
        {
            return dlvfs_sink_create_for_vfs(vfs, DlmgrDLL.Utf8Bytes(name ?? ""), size);
        }

        /// <summary>
        /// Downloads into the same native VFS owned by reader; does not open a
        /// second handle or transfer ownership. Keep reader alive and undisposed
        /// until every sink created from it has been released after task termination.
        /// After Done, call RefreshIndex and EnsureMapping on this same reader to
        /// read the committed entry; no close/reopen or flush is needed for visibility.
        /// </summary>
        public static ulong dlvfs_sink_create_for_vfs(VfsReader reader, string name, ulong size)
        {
            if (reader == null) throw new ArgumentNullException("reader");
            return dlvfs_sink_create_for_vfs(reader.NativeHandle, name, size);
        }
    }
}
#endif // !UNITY_WEBGL
