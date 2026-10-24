use super::*;

#[test]
fn internal_thread_guard_restores_identity_and_rejects_any_manager() {
    let first = Arc::new(Shared::new(1, 0, 0, ptr::null_mut()));
    let second = Arc::new(Shared::new(1, 0, 0, ptr::null_mut()));
    let mut first_handle = Box::new(first.clone());
    let mut second_handle = Box::new(second.clone());
    let first_ptr = &mut *first_handle as *mut Arc<Shared> as *mut c_void;
    let second_ptr = &mut *second_handle as *mut Arc<Shared> as *mut c_void;
    assert!(INTERNAL_MANAGER.with(|current| current.get().is_null()));
    {
        let _guard = InternalThreadGuard::enter(&first);
        assert_eq!(INTERNAL_MANAGER.with(Cell::get), Arc::as_ptr(&first));
        // Neither manager has any stored JoinHandles. Rejection must not
        // depend on handle publication or consume/mutate either manager.
        // SAFETY: first_ptr 指向上面 Box 内存活的 Arc<Shared>；当前线程在
        // InternalThreadGuard 内，shutdown_try 必走拒绝路径不消费句柄。
        assert_eq!(unsafe { dlmgr_shutdown_try(first_ptr) }, -1);
        // SAFETY: 同上——second_ptr 指向存活 Arc，拒绝路径。
        assert_eq!(unsafe { dlmgr_shutdown_try(second_ptr) }, -1);
        // SAFETY: 同上：first_ptr 存活；legacy void 版在 guard 内同样拒绝（无返回码）。
        unsafe { dlmgr_shutdown(first_ptr) }; // legacy void ABI must also no-op
        assert!(!first.queue.lock().unwrap().shutdown);
        assert!(!second.queue.lock().unwrap().shutdown);
        assert!(!first.reporter_stop.load(Ordering::Relaxed));
        assert!(!second.reporter_stop.load(Ordering::Relaxed));
        {
            let _nested = InternalThreadGuard::enter(&second);
            assert_eq!(INTERNAL_MANAGER.with(Cell::get), Arc::as_ptr(&second));
        }
        assert_eq!(INTERNAL_MANAGER.with(Cell::get), Arc::as_ptr(&first));
    }
    assert!(INTERNAL_MANAGER.with(|current| current.get().is_null()));
    assert_eq!(Arc::strong_count(&first), 2);
    assert_eq!(Arc::strong_count(&second), 2);
    // SAFETY: null 句柄是文档化的"成功 no-op"路径。
    assert_eq!(unsafe { dlmgr_shutdown_try(ptr::null_mut()) }, 0);
}

/// dlmgr_set_priority：排队中任务提权后先被派发；同级保持原 seq（FIFO）；
/// 重复改/未知 id 返回 false。
#[test]
fn set_priority_reorders_queue() {
    let sh = Arc::new(Shared::new(1, 0, 0, std::ptr::null_mut()));
    let mk = |id: u64| {
        Arc::new(TaskShared::new(
            id,
            "r".into(),
            String::new(),
            1,
            0,
            0,
            1,
        ))
    };
    {
        let mut q = sh.queue.lock().unwrap();
        // 入队顺序：低(1)、低(2)、高(3) —— 派发序应为 3、1、2
        q.heap.push(HeapTask {
            priority: 0,
            seq: 0,
            task: mk(1),
        });
        q.heap.push(HeapTask {
            priority: 0,
            seq: 1,
            task: mk(2),
        });
        q.heap.push(HeapTask {
            priority: 5,
            seq: 2,
            task: mk(3),
        });
    }

    // 任务 1 提到最高：派发序变为 1、3、2
    assert!(sh.set_priority(1, 10));
    {
        let mut q = sh.queue.lock().unwrap();
        assert_eq!(q.heap.pop().unwrap().task.id, 1);
        assert_eq!(q.heap.pop().unwrap().task.id, 3);
        assert_eq!(q.heap.pop().unwrap().task.id, 2);
    }

    // 队列已空 → false；未知 id → false
    assert!(!sh.set_priority(1, 99));
    assert!(!sh.set_priority(42, 99));
}
