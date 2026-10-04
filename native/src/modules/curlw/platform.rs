use super::*;

pub(super) unsafe fn fd_zero(set: *mut fd_set) {
    #[cfg(windows)]
    {
        (*set).fd_count = 0;
    }
    #[cfg(not(windows))]
    {
        ptr::write_bytes(set, 0, 1);
    }
}

pub(super) unsafe fn fd_copy(dst: *mut fd_set, src: *const fd_set) {
    ptr::copy_nonoverlapping(src, dst, 1);
}

#[cfg(windows)]
#[repr(C)]
pub(super) struct CRITICAL_SECTION {
    _debug_info: *mut c_void,
    _lock_count: i32,
    _recursion_count: i32,
    _owning_thread: *mut c_void,
    _lock_semaphore: *mut c_void,
    _spin_count: usize,
}

#[cfg(windows)]
extern "system" {
    fn InitializeCriticalSection(lpCriticalSection: *mut CRITICAL_SECTION);
    fn EnterCriticalSection(lpCriticalSection: *mut CRITICAL_SECTION);
    fn LeaveCriticalSection(lpCriticalSection: *mut CRITICAL_SECTION);
    fn DeleteCriticalSection(lpCriticalSection: *mut CRITICAL_SECTION);
}

pub(super) struct RawLock {
    #[cfg(windows)]
    cs: Box<CRITICAL_SECTION>,
    #[cfg(not(windows))]
    mtx: Box<pthread_mutex_t>,
}

impl RawLock {
    pub(super) fn new() -> Option<Self> {
        #[cfg(windows)]
        unsafe {
            let mut cs = Box::new(mem::zeroed::<CRITICAL_SECTION>());
            InitializeCriticalSection(&mut *cs);
            Some(Self { cs })
        }
        #[cfg(not(windows))]
        unsafe {
            let mut mtx = Box::new(mem::zeroed::<pthread_mutex_t>());
            if libc::pthread_mutex_init(&mut *mtx, ptr::null()) != 0 {
                return None;
            }
            Some(Self { mtx })
        }
    }

    fn lock(&self) {
        #[cfg(windows)]
        unsafe {
            EnterCriticalSection(&*self.cs as *const _ as *mut _);
        }
        #[cfg(not(windows))]
        unsafe {
            libc::pthread_mutex_lock(&*self.mtx as *const _ as *mut _);
        }
    }

    fn unlock(&self) {
        #[cfg(windows)]
        unsafe {
            LeaveCriticalSection(&*self.cs as *const _ as *mut _);
        }
        #[cfg(not(windows))]
        unsafe {
            libc::pthread_mutex_unlock(&*self.mtx as *const _ as *mut _);
        }
    }
}

impl Drop for RawLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            DeleteCriticalSection(&mut *self.cs);
        }
        #[cfg(not(windows))]
        unsafe {
            libc::pthread_mutex_destroy(&mut *self.mtx);
        }
    }
}

unsafe impl Send for RawLock {}
unsafe impl Sync for RawLock {}

pub(super) struct ShareLockSet {
    locks: [RawLock; CURL_LOCK_DATA_LAST as usize],
}

impl ShareLockSet {
    pub(super) fn new() -> Option<Box<Self>> {
        let locks: [RawLock; CURL_LOCK_DATA_LAST as usize] = [
            RawLock::new()?,
            RawLock::new()?,
            RawLock::new()?,
            RawLock::new()?,
            RawLock::new()?,
            RawLock::new()?,
            RawLock::new()?,
            RawLock::new()?,
        ];
        Some(Box::new(Self { locks }))
    }
}

pub(super) struct FdSetPoolInner {
    chunk_size: usize,
    blocks: Vec<Box<[fd_set]>>,
    free_list: Vec<*mut fd_set>,
}

pub(super) struct FdSetPool {
    inner: Mutex<FdSetPoolInner>,
}

impl FdSetPool {
    pub(super) fn new(chunk: usize) -> Self {
        let chunk_size = if chunk == 0 { 32 } else { chunk };
        Self {
            inner: Mutex::new(FdSetPoolInner {
                chunk_size,
                blocks: Vec::new(),
                free_list: Vec::new(),
            }),
        }
    }

    pub(super) fn allocate(&self) -> *mut fd_set {
        let mut inner = self.inner.lock().unwrap();
        if inner.free_list.is_empty() {
            let count = inner.chunk_size;
            let mut sets = Vec::with_capacity(count);
            for _ in 0..count {
                sets.push(unsafe { mem::zeroed::<fd_set>() });
            }
            let sets = sets.into_boxed_slice();
            let base = sets.as_ptr() as *mut fd_set;
            for i in 0..count {
                inner.free_list.push(unsafe { base.add(i) });
            }
            inner.blocks.push(sets);
        }
        inner.free_list.pop().unwrap()
    }

    pub(super) fn deallocate(&self, p: *mut fd_set) {
        if p.is_null() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.free_list.push(p);
    }
}

unsafe impl Send for FdSetPool {}
unsafe impl Sync for FdSetPool {}

pub(super) static GLOBAL_MTX: OnceLock<Mutex<()>> = OnceLock::new();
pub(super) static G_INIT_COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
pub(super) static G_FD_POOL: AtomicPtr<FdSetPool> = AtomicPtr::new(ptr::null_mut());
pub(super) static G_SHARE_LOCKS: OnceLock<Mutex<HashMap<usize, Box<ShareLockSet>>>> = OnceLock::new();

pub(super) fn global_mtx() -> &'static Mutex<()> {
    GLOBAL_MTX.get_or_init(|| Mutex::new(()))
}

pub(super) fn init_count() -> &'static Mutex<usize> {
    G_INIT_COUNT.get_or_init(|| Mutex::new(0))
}

pub(super) fn share_locks() -> &'static Mutex<HashMap<usize, Box<ShareLockSet>>> {
    G_SHARE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) type SocketManagedCb = unsafe extern "C" fn(sockfd: intptr_t, userptr: *mut c_void) -> c_int;

pub(super) static G_OPEN_CB: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
pub(super) static G_CLOSE_CB: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

pub(super) unsafe extern "C" fn open_socket_trampoline(
    clientp: *mut c_void,
    _purpose: curlsocktype,
    address: *mut curl_sockaddr,
) -> curl_socket_t {
    let cb_ptr = G_OPEN_CB.load(Ordering::Acquire);
    if cb_ptr.is_null() {
        return CURL_SOCKET_BAD;
    }
    let cb: SocketManagedCb = mem::transmute(cb_ptr);
    if cb(-1 as intptr_t, clientp) == 0 {
        return CURL_SOCKET_BAD;
    }
    let family = (*address).family;
    let socktype = (*address).socktype;
    let protocol = (*address).protocol;
    #[cfg(windows)]
    let fd = socket(family, socktype, protocol);
    #[cfg(not(windows))]
    let fd = socket(family, socktype, protocol) as curl_socket_t;
    if fd != CURL_SOCKET_BAD {
        cb(fd as intptr_t, clientp);
    }
    fd
}

pub(super) unsafe extern "C" fn close_socket_trampoline(
    clientp: *mut c_void,
    item: curl_socket_t,
) -> c_int {
    let cb_ptr = G_CLOSE_CB.load(Ordering::Acquire);
    if !cb_ptr.is_null() {
        let cb: SocketManagedCb = mem::transmute(cb_ptr);
        if cb(item as intptr_t, clientp) != 0 {
            return 0;
        }
    }
    if item == CURL_SOCKET_BAD {
        return 0;
    }
    #[cfg(windows)]
    {
        if closesocket(item) == 0 { 0 } else { 1 }
    }
    #[cfg(not(windows))]
    {
        if close(item as c_int) == 0 { 0 } else { 1 }
    }
}

pub(super) unsafe extern "C" fn share_lock(
    _handle: *mut CURL,
    data: curl_lock_data,
    _access: curl_lock_access,
    userptr: *mut c_void,
) {
    if userptr.is_null() {
        return;
    }
    if data < 0 || data >= CURL_LOCK_DATA_LAST {
        return;
    }
    let lock_set = &*(userptr as *const ShareLockSet);
    lock_set.locks[data as usize].lock();
}

pub(super) unsafe extern "C" fn share_unlock(
    _handle: *mut CURL,
    data: curl_lock_data,
    userptr: *mut c_void,
) {
    if userptr.is_null() {
        return;
    }
    if data < 0 || data >= CURL_LOCK_DATA_LAST {
        return;
    }
    let lock_set = &*(userptr as *const ShareLockSet);
    lock_set.locks[data as usize].unlock();
}


#[cfg(windows)]
const SOCKET_TIMED_OUT: c_int = WSAETIMEDOUT;
#[cfg(not(windows))]
const SOCKET_TIMED_OUT: c_int = ETIMEDOUT;

#[inline]
pub(super) unsafe fn set_socket_timeout_errno() {
    #[cfg(windows)]
    {
        WSASetLastError(SOCKET_TIMED_OUT);
    }
    #[cfg(not(windows))]
    {
        *errno_ptr() = SOCKET_TIMED_OUT;
    }
}

#[inline]
pub(super) unsafe fn get_socket_errno() -> c_int {
    #[cfg(windows)]
    {
        WSAGetLastError()
    }
    #[cfg(not(windows))]
    {
        *errno_ptr()
    }
}

#[inline]
pub(super) fn get_eintr_errno() -> c_int {
    #[cfg(windows)]
    {
        WSAEINTR
    }
    #[cfg(not(windows))]
    {
        EINTR
    }
}

