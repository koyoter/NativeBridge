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
//     (every tick).
//   * strings cross the ABI as UTF-8: string params are encoded via Utf8Bytes
//     (NUL-terminated byte[]) and returned char* values are read via
//     PtrToStringUtf8. Never use CharSet.Ansi — on Windows it converts through
//     the system ANSI code page (e.g. GBK) while the native side expects UTF-8.
//   * Callback keep-alive: the native side stores RAW function pointers. The
//     DlmgrDLL wrappers below keep static strong references to every delegate
//     they register, but user-supplied delegates MUST be static +
//     [MonoPInvokeCallback(typeof(DlmgrTaskCallback))] on IL2CPP/AOT and stay
//     referenced for the manager lifetime. Callbacks fire on a native worker
//     thread — dispatch to the main thread yourself if needed.
//   * Sinks: a sink is a u64 handle from a native sink registry (glue bridges
//     VFS-backed sinks via the Rust sink vtable; DlmgrSinkCreateFile gives
//     plain-file ones). Release a sink only after its task reported a
//     terminal state (Done / Failed / Canceled).
//   * Lifecycle: Create -> (AddDohUrl)* -> Start -> Enqueue* -> Shutdown.
//     Shutdown consumes the handle (cancel all + join, cannot deadlock).
//     curlw_global_init is the host's responsibility and must be called
//     before Start.
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

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_shutdown")]
        private static extern void dlmgr_shutdown_imp(IntPtr mgr);

        // Consumes the handle: cancel all + join threads; do not use afterwards.
        // The reporter thread is joined before shutdown returns, so no callback
        // can be in flight when this manager's keep-alive entries are dropped.
        public static void dlmgr_shutdown(IntPtr mgr)
        {
            dlmgr_shutdown_imp(mgr);
            lock (s_cbLock)
            {
                s_handleCallbacks.Remove(mgr);
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
        // manager handle (same discipline as Curlw.cs s_handleCallbacks): a
        // process-wide single slot would drop a live delegate when a second
        // manager registers or a callback is re-registered, and the GC'd thunk
        // would crash the next native callback.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_set_task_callback")]
        private static extern int dlmgr_set_task_callback_imp(IntPtr mgr, DlmgrTaskCallback cb, IntPtr user);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "dlmgr_set_global_callback")]
        private static extern int dlmgr_set_global_callback_imp(IntPtr mgr, DlmgrGlobalCallback cb, IntPtr user);

        private static readonly System.Collections.Generic.Dictionary<IntPtr, System.Collections.Generic.List<Delegate>>
            s_handleCallbacks = new System.Collections.Generic.Dictionary<IntPtr, System.Collections.Generic.List<Delegate>>();
        private static readonly object s_cbLock = new object();

        // cb == null clears the registration natively. The handle's PREVIOUS
        // delegates are deliberately kept alive: a callback may still be in
        // flight on the reporter thread when the slot is cleared (bounded by
        // manager count, freed on shutdown).
        private static void KeepAlive(IntPtr handle, Delegate cb)
        {
            if (cb == null) return;
            lock (s_cbLock)
            {
                System.Collections.Generic.List<Delegate> list;
                if (!s_handleCallbacks.TryGetValue(handle, out list))
                {
                    list = new System.Collections.Generic.List<Delegate>();
                    s_handleCallbacks[handle] = list;
                }
                list.Add(cb);
            }
        }

        // cb == null clears the registration (keep-alive references stay, see above).
        public static int dlmgr_set_task_callback(IntPtr mgr, DlmgrTaskCallback cb, IntPtr user)
        {
            KeepAlive(mgr, cb);
            return dlmgr_set_task_callback_imp(mgr, cb, user);
        }

        public static int dlmgr_set_global_callback(IntPtr mgr, DlmgrGlobalCallback cb, IntPtr user)
        {
            KeepAlive(mgr, cb);
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
    /// to a VFS writer; see modules/glue.rs). The vfs handle must stay open
    /// (no vfs_close) for the whole lifetime of every sink created from it.
    /// </summary>
    public static class DlVfs
    {
#if (UNITY_IOS || UNITY_TVOS) && !UNITY_EDITOR
        public const string LIBNAME = "__Internal";
#else
        public const string LIBNAME = "NativeBridge";
#endif

        // Creates a sink that downloads `name` (size in bytes, enforced at
        // prepare time) into the VFS `vfs` (handle from VfsDLL.vfs_open).
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
    }
}
#endif // !UNITY_WEBGL
