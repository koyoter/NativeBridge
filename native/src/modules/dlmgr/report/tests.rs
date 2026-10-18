use super::*;
use libc::c_void;
use std::sync::atomic::AtomicBool;

use super::super::task::ERR_CANCELED;
use super::super::SendPtr;

static REPORTED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn on_task(
    _user: *mut c_void,
    _id: u64,
    state: i32,
    _done: u64,
    _total: u64,
    _bps: u64,
    _err: i32,
) {
    if state == TASK_CANCELED {
        REPORTED.store(true, Ordering::SeqCst);
    }
}

/// R18：stop 置位后 reporter 必须补拍一拍——shutdown 在 join 完 worker 后才
/// 置 stop，最后一批终态（cancel_all 的 Canceled 等）不补拍就永远不上报。
#[test]
fn final_pass_reports_terminal_after_stop() {
    let sh = Arc::new(Shared::new(1, 0, 0, std::ptr::null_mut()));
    let t = Arc::new(TaskShared::new(
        7,
        "r".into(),
        String::new(),
        1,
        0,
        0,
        0,
    ));
    t.err.store(ERR_CANCELED, Ordering::Relaxed); // 先 err 后 state
    t.state.store(TASK_CANCELED, Ordering::Release);
    sh.tasks.lock().unwrap().insert(7, t);
    *sh.task_cb.lock().unwrap() = Some((
        on_task as super::super::DlmgrTaskCb,
        SendPtr(std::ptr::null_mut()),
    ));
    sh.reporter_stop.store(true, Ordering::Release);

    // stop 已置位：while 不进循环，只剩最终补拍一拍
    reporter_loop(sh);
    assert!(REPORTED.load(Ordering::SeqCst));
}
