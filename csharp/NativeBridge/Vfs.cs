//
// Vfs.cs — C# binding for the NativeBridge "vfs" module.
//
// This is the managed mirror of the native contract exported by
//   src/nativebridge/rust/src/modules/vfs/mod.rs
// Keep the two in lock-step: every P/Invoke below matches a #[no_mangle]
// export 1:1 (cdecl calling convention, same argument widths).
//
// Design notes:
//   * namespace NativeBridgeF, LIBNAME "NativeBridge" (native library file name),
//     same as Curlw.cs.
//   * The index lives in native memory (header.vfs is never exposed). Index
//     queries go through vfs_lookup / vfs_enumerate_*; this class only maps
//     files.vfs (MemoryMappedFile) and slices Active entries in-place.
//   * strings cross the ABI as UTF-8: string params are encoded via
//     DlmgrDLL.Utf8Bytes (NUL-terminated byte[]). Never use CharSet.Ansi — on
//     Windows it converts through the system ANSI code page (e.g. GBK) while
//     the native side expects UTF-8.
//   * Callback keep-alive: the native side stores raw function pointers. This
//     file keeps strong managed references to every delegate it registers, but
//     user-supplied delegates MUST be static + [MonoPInvokeCallback] on
//     IL2CPP/AOT and must stay referenced for the handle lifetime.
//   * Tear-free reading: only VfsFileState.Active entries are ever exposed by
//     TryReadRaw/ReadBytes; committed data never moves while readers hold it
//     (compaction is exclusive with writers), and a generation change signals
//     that offsets may have shifted → RefreshIndex().
//
#if !UNITY_WEBGL
using System;
using System.Collections.Generic;
using System.IO;
using System.IO.MemoryMappedFiles;
using System.Runtime.InteropServices;
using System.Text;

namespace NativeBridgeF
{
    // --- result / state enums (values mirror the native ABI) ---------------------------
    public enum VfsResult
    {
        OK = 0,
        IO = 1,
        NotFound = 2,
        AlreadyExists = 3,
        BusyCompacting = 4,
        BusyWriters = 5,
        CrcMismatch = 6,
        WriteOverflow = 7,
        GenChanged = 8,
        BufferTooSmall = 9,
        InvalidArg = 10,
        StateInvalid = 11,
    }

    public enum VfsFileState
    {
        Active = 0,       // committed; safe to read
        Deleted = 1,      // soft-deleted; space reclaimed by compaction
        Downloading = 2,  // writer in flight; never read
        Bad = 3,          // crc mismatch found by compaction; dropped at Finalize
    }

    public enum VfsCompactState
    {
        Idle = 0,
        Scan = 1,
        Move = 2,
        Finalize = 3,
    }

    public struct VfsEntry
    {
        public ulong Offset;      // absolute offset into files.vfs
        public ulong Size;        // logical size (unpadded)
        public VfsFileState State;
        public uint Crc;          // crc32 of the committed content
    }

    public struct VfsStatInfo
    {
        public ulong Logical;     // append cursor (end of used space)
        public ulong Physical;    // files.vfs on-disk size
        public ulong Total;       // 4K-aligned spans of all registered entries
        public ulong Active;
        public ulong Deleted;
        public ulong Garbage;     // logical minus total (aborts, replaced deletes, ...)
    }

    /// <summary>
    /// An unsafe slice into the mapped view of files.vfs. Valid until the
    /// mapping is rebuilt by a later EnsureMapping() (physical size change),
    /// released via ReleaseMapping(), or Dispose.
    /// </summary>
    public struct VfsRawSpan
    {
        public IntPtr Ptr;
        public int Length;
    }

    // --- callbacks -------------------------------------------------------------
    // MUST be static + [MonoPInvokeCallback(typeof(VfsCommitDelegate))] on
    // IL2CPP/AOT, and the delegate instance must stay referenced (VfsReader pins
    // the one it registers). Fires on the committing thread.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate void VfsCommitDelegate(IntPtr user, IntPtr name, ulong off, ulong size, uint crc);

    // Fires on the compaction thread; percent 0..100, name = file being moved.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate void VfsProgressDelegate(IntPtr user, int percent, IntPtr name);

    // Fires exactly once per vfs_compact on the compaction thread; err is a VfsResult.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate void VfsDoneDelegate(IntPtr user, int err);

    /// <summary>
    /// Thin 1:1 binding of the native vfs C ABI (see modules/vfs/mod.rs).
    /// </summary>
    public static class VfsDLL
    {
#if (UNITY_IOS || UNITY_TVOS) && !UNITY_EDITOR
        public const string LIBNAME = "__Internal";
#else
        public const string LIBNAME = "NativeBridge";
#endif

        // --- version / ABI ------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_abi_version();

        // --- open / close ---------------------------------------------------------
        // Returns an opaque handle or IntPtr.Zero. Pair with exactly one vfs_close.
        // C# MUST NOT close the handle while a native writer (download sink) is
        // alive: C# 保证 sink 存活期不 close.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr vfs_open(byte[] dir);

        // Path-based open: index and data files named individually (additive
        // superset of vfs_open; VFS_ABI_VERSION unchanged). The native side
        // rejects an empty/whitespace path or index == data (trimmed,
        // case-folded string compare) by returning IntPtr.Zero.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr vfs_open_paths(byte[] index, byte[] data);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void vfs_close(IntPtr h);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_flush(IntPtr h);

        // --- write path (three-phase) -----------------------------------------------
        // Typically driven by the Rust glue layer (dlmgr VfsSink), not by C#; the
        // writer trio is exported for hosts/tests that write from managed code.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr vfs_alloc(IntPtr h, byte[] name, ulong size);

        // Native export is "vfs_writer_write"; the _imp suffix avoids a C# name clash
        // with the public safe overload below.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "vfs_writer_write")]
        private static extern int vfs_writer_write_imp(IntPtr w, ulong rel_off, IntPtr buf, UIntPtr len);

        /// <summary>Sequential write into the writer's window (cdecl, byte[] pinned for the call).</summary>
        public static unsafe int vfs_writer_write(IntPtr w, ulong rel_off, byte[] data, int offset, int count)
        {
            fixed (byte* p = &data[offset])
            {
                return vfs_writer_write_imp(w, rel_off, (IntPtr)p, (UIntPtr)count);
            }
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_writer_commit(IntPtr w);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_writer_abort(IntPtr w);

        // --- delete ------------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_delete(IntPtr h, byte[] name);

        // --- lookup --------------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_lookup(IntPtr h, byte[] name, out ulong out_off,
                                            out ulong out_size, out int out_state, out uint out_crc);

        // --- enumerate (two-phase) --------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_enumerate_query(IntPtr h, out ulong out_gen,
                                                     out uint out_count, out uint out_blob_len);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_enumerate_read(IntPtr h, ulong gen, byte[] names_buf,
                                                    ulong[] off, ulong[] size, uint[] crc,
                                                    int[] state, UIntPtr cap);

        // --- generation / stat ----------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern ulong vfs_get_generation(IntPtr h);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_stat(IntPtr h, out ulong logical, out ulong physical,
                                          out ulong total, out ulong active, out ulong deleted,
                                          out ulong garbage);

        // --- commit callback ---------------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_set_commit_callback(IntPtr h, VfsCommitDelegate cb, IntPtr user);

        // --- compaction (async) -----------------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_compact(IntPtr h, ulong reserve_extra,
                                             VfsProgressDelegate progress_cb,
                                             VfsDoneDelegate done_cb, IntPtr user);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int vfs_compact_status(IntPtr h, out int out_state, out int out_percent);
    }

    /// <summary>
    /// Read-side convenience over a VFS container (one index/data file pair):
    /// keeps the native handle and a string→entry index in sync. The
    /// MemoryMappedFile view of the data file is NOT
    /// managed implicitly — its establish/rebuild/release timing is fully under
    /// the caller's control via EnsureMapping()/ReleaseMapping().
    ///
    /// Usage: Open → RefreshIndex → EnsureMapping → TryGet/TryReadRaw/ReadBytes.
        /// Poll Generation (and/or register a commit callback via
        /// SetCommitCallback) and call RefreshIndex() when
    /// it changed; after a compaction done callback call RefreshIndex() first and
    /// then EnsureMapping() — offsets may all have shifted. Never Dispose while a
    /// native writer/sink on this handle is alive.
    /// </summary>
    public sealed class VfsReader : IDisposable
    {
        private IntPtr _h;
        private readonly string _indexPath;
        private readonly string _dataPath;
        private readonly Dictionary<string, VfsEntry> _index = new Dictionary<string, VfsEntry>();
        private ulong _refreshedGen;

        // files.vfs mapping (rebuilt whenever the physical size changes)
        private MemoryMappedFile _mmf;
        private MemoryMappedViewAccessor _view;
        private unsafe byte* _basePtr;
        private long _mappedPhysical = -1;

        // keep-alive: native stores raw function pointers. Registered delegates
        // are accumulated here and never dropped while the handle lives — a
        // replace/clear must not unpin a delegate the native side may already
        // have copied and be about to invoke (same discipline as DlmgrDLL;
        // bounded by the number of registrations, cleared on Dispose).
        private readonly List<VfsCommitDelegate> _commitKeepAlive = new List<VfsCommitDelegate>();

        public string IndexFilePath { get { return _indexPath; } }
        public string DataFilePath { get { return _dataPath; } }

        private VfsReader(IntPtr handle, string indexPath, string dataPath)
        {
            _h = handle;
            _indexPath = indexPath;
            _dataPath = dataPath;
        }

        /// <summary>
        /// Path-based open: names the index and data files individually (parent
        /// directories and the files themselves are created on demand). Throws
        /// IOException on failure — including the native side's guards (an
        /// empty/whitespace path, or the same path for index and data under a
        /// trimmed, case-folded string compare).
        /// </summary>
        public static VfsReader Open(string indexPath, string dataPath)
        {
            if (indexPath == null) throw new ArgumentNullException("indexPath");
            if (dataPath == null) throw new ArgumentNullException("dataPath");
            IntPtr h = VfsDLL.vfs_open_paths(DlmgrDLL.Utf8Bytes(indexPath), DlmgrDLL.Utf8Bytes(dataPath));
            if (h == IntPtr.Zero)
            {
                throw new IOException("vfs_open_paths failed for " + indexPath + " + " + dataPath);
            }
            return new VfsReader(h, indexPath, dataPath);
        }

        /// <summary>
        /// Directory-based open: opens (or initializes) the VFS container
        /// dir/header.vfs + dir/files.vfs. Throws IOException on failure.
        /// </summary>
        public static VfsReader Open(string dir)
        {
            if (dir == null) throw new ArgumentNullException("dir");
            return Open(Path.Combine(dir, "header.vfs"), Path.Combine(dir, "files.vfs"));
        }

        /// <summary>ABI guard for diagnostics: must equal VfsDLL-side VFS_ABI_VERSION (1).</summary>
        public static int AbiVersion()
        {
            return VfsDLL.vfs_abi_version();
        }

        /// <summary>
        /// Re-enumerates the native index into the local dictionary (retrying a
        /// bounded number of times on VfsResult.GenChanged). Pure index operation:
        /// it never touches the files.vfs mapping — follow with EnsureMapping()
        /// before reading data. Call after any generation change visible via
        /// Generation / the commit callback / compaction done. Throws IOException
        /// while a compaction is running (VfsResult.BusyCompacting) — poll
        /// CompactStatus() and refresh after the done callback fires.
        /// </summary>
        public void RefreshIndex()
        {
            const int MaxAttempts = 8;
            for (int attempt = 0; ; attempt++)
            {
                ulong gen, dummy1;
                uint count, blobLen;
                int err = VfsDLL.vfs_enumerate_query(_h, out gen, out count, out blobLen);
                ThrowOnError(err, "vfs_enumerate_query");

                byte[] names = blobLen > 0 ? new byte[blobLen] : new byte[0];
                ulong[] offs = new ulong[count];
                ulong[] sizes = new ulong[count];
                uint[] crcs = new uint[count];
                int[] states = new int[count];

                err = VfsDLL.vfs_enumerate_read(_h, gen, names, offs, sizes, crcs, states, (UIntPtr)count);
                if (err == (int)VfsResult.GenChanged && attempt < MaxAttempts - 1)
                {
                    continue; // index moved under us — re-query
                }
                ThrowOnError(err, "vfs_enumerate_read");

                var idx = new Dictionary<string, VfsEntry>((int)count);
                int start = 0;
                for (uint i = 0; i < count; i++)
                {
                    // names blob: NUL-terminated UTF-8 names back to back
                    int end = start;
                    while (end < names.Length && names[end] != 0) end++;
                    string name = Encoding.UTF8.GetString(names, start, end - start);
                    start = end + 1;
                    idx[name] = new VfsEntry
                    {
                        Offset = offs[i],
                        Size = sizes[i],
                        State = (VfsFileState)states[i],
                        Crc = crcs[i],
                    };
                }
                _index.Clear();
                foreach (var kv in idx) _index.Add(kv.Key, kv.Value);
                _refreshedGen = gen;
                break;
            }
        }

        /// <summary>
        /// Idempotently establishes the read-only files.vfs mapping: creates it on
        /// the first call, rebuilds it when the physical size changed (download
        /// appends, compaction truncate/pre-allocate), and returns in place when
        /// nothing changed. Caller protocol: call after every RefreshIndex and
        /// after any physical size change, before reading data. Throws IOException
        /// on failure.
        /// </summary>
        public void EnsureMapping()
        {
            ulong logical, physical, total, active, deleted, garbage;
            ThrowOnError(VfsDLL.vfs_stat(_h, out logical, out physical, out total,
                                         out active, out deleted, out garbage), "vfs_stat");
            if (_mmf != null && (long)physical == _mappedPhysical)
            {
                return;
            }
            ReleaseMapping();
            if (physical > 0)
            {
                // Open via an explicit FileStream with FileShare.ReadWrite: the Rust
                // side keeps read/write handles on files.vfs for its whole lifetime,
                // and the plain CreateFromFile(string,...) overload shares only
                // FileShare.Read — the existing writer handle conflicts with that
                // share mode and Windows fails the open with a sharing violation.
                var fs = new FileStream(_dataPath, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
                _mmf = MemoryMappedFile.CreateFromFile(fs, null, 0, MemoryMappedFileAccess.Read,
                                                       HandleInheritability.None, false);
                _view = _mmf.CreateViewAccessor(0, 0, MemoryMappedFileAccess.Read);
                unsafe
                {
                    byte* p = null;
                    _view.SafeMemoryMappedViewHandle.AcquirePointer(ref p);
                    _basePtr = p;
                }
            }
            _mappedPhysical = (long)physical;
        }

        /// <summary>
        /// Releases the files.vfs mapping (no-op when none is live). The caller
        /// owns the mapping lifetime: it MUST be called before starting a
        /// compaction while a mapping exists (Windows cannot truncate a mapped
        /// file), and may also be called whenever reads are no longer needed.
        /// </summary>
        public void ReleaseMapping()
        {
            unsafe
            {
                if (_view != null)
                {
                    _view.SafeMemoryMappedViewHandle.ReleasePointer();
                    _basePtr = null;
                    _view.Dispose();
                    _view = null;
                }
            }
            if (_mmf != null)
            {
                _mmf.Dispose();
                _mmf = null;
            }
            _mappedPhysical = -1;
        }

        /// <summary>Live native generation (bumped by every index change: alloc,
        /// abort, commit, delete, compaction finalize). Compare with the value
        /// seen at last RefreshIndex to detect changes.</summary>
        public ulong Generation
        {
            get { return VfsDLL.vfs_get_generation(_h); }
        }

        /// <summary>Generation captured by the last RefreshIndex.</summary>
        public ulong RefreshedGeneration
        {
            get { return _refreshedGen; }
        }

        /// <summary>Index lookup without I/O (dictionary hit).</summary>
        public bool TryGet(string name, out VfsEntry entry)
        {
            return _index.TryGetValue(name, out entry);
        }

        /// <summary>Entry count from the last RefreshIndex.</summary>
        public int Count
        {
            get { return _index.Count; }
        }

        /// <summary>
        /// Zero-copy read of a committed (Active) file: returns a pointer+length
        /// slice into the mapped view. Valid until the next mapping rebuild or
        /// Dispose. Refuses non-Active entries — the anti-tear gate. Returns
        /// false (fails safely, never throws) when the mapping is absent — not
        /// yet established or already released — or the entry lies outside the
        /// mapped view.
        /// </summary>
        public unsafe bool TryReadRaw(string name, out VfsRawSpan span)
        {
            span = default(VfsRawSpan);
            VfsEntry e;
            if (!_index.TryGetValue(name, out e) || e.State != VfsFileState.Active)
            {
                return false;
            }
            if (_view == null || e.Offset + e.Size > (ulong)_view.Capacity || e.Size > int.MaxValue)
            {
                return false;
            }
            span.Ptr = (IntPtr)(_basePtr + e.Offset);
            span.Length = (int)e.Size;
            return true;
        }

        /// <summary>Safe convenience: copies the file content into a byte[].</summary>
        public bool TryReadBytes(string name, out byte[] data)
        {
            data = null;
            VfsRawSpan span;
            if (!TryReadRaw(name, out span)) return false;
            data = new byte[span.Length];
            unsafe
            {
                Marshal.Copy(span.Ptr, data, 0, span.Length);
            }
            return true;
        }

        /// <summary>Byte accounting snapshot (available during compaction too).</summary>
        public VfsStatInfo Stat()
        {
            VfsStatInfo s;
            ulong logical, physical, total, active, deleted, garbage;
            ThrowOnError(VfsDLL.vfs_stat(_h, out logical, out physical, out total,
                                         out active, out deleted, out garbage), "vfs_stat");
            s.Logical = logical; s.Physical = physical; s.Total = total;
            s.Active = active; s.Deleted = deleted; s.Garbage = garbage;
            return s;
        }

        /// <summary>Soft delete (generation++). Re-query with RefreshIndex to observe.</summary>
        public void Delete(string name)
        {
            ThrowOnError(VfsDLL.vfs_delete(_h, DlmgrDLL.Utf8Bytes(name)), "vfs_delete");
        }

        /// <summary>Explicit persistence (no periodic flush; unflushed commits are lost on crash).</summary>
        public void Flush()
        {
            ThrowOnError(VfsDLL.vfs_flush(_h), "vfs_flush");
        }

        /// <summary>
        /// Async compaction. progress/done may be null; if given they MUST be
        /// static + [MonoPInvokeCallback] on IL2CPP/AOT and stay referenced
        /// (this reader does NOT pin user-supplied progress/done delegates —
        /// keep them in a field yourself). After done: RefreshIndex().
        /// </summary>
        public void Compact(ulong reserveExtra, VfsProgressDelegate progress, VfsDoneDelegate done, IntPtr user)
        {
            ThrowOnError(VfsDLL.vfs_compact(_h, reserveExtra, progress, done, user), "vfs_compact");
        }

        public VfsCompactState CompactStatus(out int percent)
        {
            int state;
            ThrowOnError(VfsDLL.vfs_compact_status(_h, out state, out percent), "vfs_compact_status");
            return (VfsCompactState)state;
        }

        /// <summary>
        /// Registers the optional commit callback. The delegate is pinned by this
        /// reader for the handle lifetime; on IL2CPP/AOT it MUST be a static
        /// method decorated with [MonoPInvokeCallback(typeof(VfsCommitDelegate))].
        /// Pass null to clear.
        /// </summary>
        public void SetCommitCallback(VfsCommitDelegate cb)
        {
            if (cb != null) _commitKeepAlive.Add(cb);
            ThrowOnError(VfsDLL.vfs_set_commit_callback(_h, cb, IntPtr.Zero), "vfs_set_commit_callback");
        }

        private static void ThrowOnError(int err, string api)
        {
            if (err != (int)VfsResult.OK)
            {
                throw new IOException(api + " failed: " + (VfsResult)err);
            }
        }

        /// <summary>
        /// Releases the mapping and closes the native handle. C# 保证 sink 存活期
        /// 不 close: never call while a native writer/sink is outstanding.
        /// </summary>
        public void Dispose()
        {
            ReleaseMapping();
            if (_h != IntPtr.Zero)
            {
                VfsDLL.vfs_close(_h);
                _h = IntPtr.Zero;
            }
            _commitKeepAlive.Clear();
        }
    }
}
#endif // !UNITY_WEBGL
