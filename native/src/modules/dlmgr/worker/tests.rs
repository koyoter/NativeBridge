use super::*;

const SIZE: u64 = 100;

fn rs(range: bool, fb: bool, retries: u32) -> RunState {
    RunState {
        range_phase: range,
        fallback_used: fb,
        retries,
        last_err: ERR_OK,
    }
}

fn http_ok(
    resp: i64,
    cr: Option<(u64, u64)>,
    clen: i64,
    complete: bool,
    crc_ok: bool,
) -> AttemptOutcome {
    AttemptOutcome::HttpOk {
        resp,
        cr,
        clen,
        complete,
        crc_ok,
    }
}

#[test]
fn range_200_falls_back_without_retry() {
    let mut st = rs(true, false, 0);
    assert!(matches!(
        transition(&mut st, &http_ok(200, None, -1, false, false), SIZE, 3),
        Decision::Fallback(e) if e == ERR_UNSUPPORTED_RANGE
    ));
    assert_eq!(st.retries, 0); // 兜底不消耗重试
}

#[test]
fn range_416_falls_back() {
    let mut st = rs(true, false, 1);
    assert!(matches!(
        transition(&mut st, &http_ok(416, None, -1, false, false), SIZE, 3),
        Decision::Fallback(e) if e == ERR_UNSUPPORTED_RANGE
    ));
}

#[test]
fn range_content_range_mismatch_falls_back() {
    let mut st = rs(true, false, 0);
    // 总长不符
    assert!(matches!(
        transition(&mut st, &http_ok(206, Some((0, 99)), -1, false, true), SIZE, 3),
        Decision::Fallback(e) if e == ERR_SIZE_MISMATCH
    ));
    // 起始偏移不设闸（Q1 决策）：总长相符即放行，错源由 crc 兜底；
    // 字节不足（complete=false）视同断线，走断点重试
    assert!(matches!(
        transition(
            &mut st,
            &http_ok(206, Some((5, SIZE)), -1, false, true),
            SIZE,
            3
        ),
        Decision::Resume
    ));
    // Content-Range 缺失
    assert!(matches!(
        transition(&mut st, &http_ok(206, None, -1, false, true), SIZE, 3),
        Decision::Fallback(e) if e == ERR_SIZE_MISMATCH
    ));
}

#[test]
fn range_crc_mismatch_falls_back() {
    let mut st = rs(true, false, 0);
    assert!(matches!(
        transition(&mut st, &http_ok(206, Some((0, SIZE)), -1, true, false), SIZE, 3),
        Decision::Fallback(e) if e == ERR_CRC_MISMATCH
    ));
    assert_eq!(st.retries, 0);
}

#[test]
fn range_done_when_complete_and_crc_ok() {
    let mut st = rs(true, false, 0);
    assert!(matches!(
        transition(
            &mut st,
            &http_ok(206, Some((0, SIZE)), SIZE as i64, true, true),
            SIZE,
            3
        ),
        Decision::Done
    ));
}

#[test]
fn range_neterr_resumes_then_falls_back_when_exhausted() {
    let mut st = rs(true, false, 0);
    for expect in 1..=3u32 {
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
            Decision::Resume
        ));
        assert_eq!(st.retries, expect);
    }
    // 预算用尽且未兜底 → 兜底
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
        Decision::Fallback(e) if e == ERR_NETWORK
    ));
}

#[test]
fn timeout_maps_to_timeout_err() {
    let mut st = rs(true, false, 0);
    assert!(matches!(
        transition(
            &mut st,
            &AttemptOutcome::NetErr(CURLE_OPERATION_TIMEDOUT),
            SIZE,
            3
        ),
        Decision::Resume
    ));
    assert_eq!(st.last_err, ERR_TIMEOUT);
}

#[test]
fn gatefail_on_full_phase_consumes_retry_then_fails() {
    let mut st = rs(false, true, 0);
    assert!(matches!(
        transition(
            &mut st,
            &AttemptOutcome::GateFail(ERR_SIZE_MISMATCH),
            SIZE,
            2
        ),
        Decision::RestartFromZero
    ));
    assert_eq!(st.retries, 1);
    assert!(matches!(
        transition(
            &mut st,
            &AttemptOutcome::GateFail(ERR_SIZE_MISMATCH),
            SIZE,
            2
        ),
        Decision::RestartFromZero
    ));
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::GateFail(ERR_SIZE_MISMATCH), SIZE, 2),
        Decision::Fail(e) if e == ERR_SIZE_MISMATCH
    ));
}

#[test]
fn full_crc_mismatch_consumes_retry_then_fails() {
    let mut st = rs(false, true, 0);
    assert!(matches!(
        transition(
            &mut st,
            &http_ok(200, None, SIZE as i64, true, false),
            SIZE,
            1
        ),
        Decision::RestartFromZero
    ));
    assert_eq!(st.last_err, ERR_CRC_MISMATCH);
    assert!(matches!(
        transition(&mut st, &http_ok(200, None, SIZE as i64, true, false), SIZE, 1),
        Decision::Fail(e) if e == ERR_CRC_MISMATCH
    ));
}

#[test]
fn full_clen_mismatch_gates() {
    let mut st = rs(false, true, 0);
    // 响应 2xx 但 Content-Length ≠ size
    assert!(matches!(
        transition(&mut st, &http_ok(200, None, 50, true, true), SIZE, 0),
        Decision::Fail(e) if e == ERR_SIZE_MISMATCH
    ));
}

#[test]
fn full_neterr_retries_from_zero_then_fails() {
    let mut st = rs(false, true, 0);
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
        Decision::RestartFromZero
    ));
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
        Decision::RestartFromZero
    ));
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
        Decision::RestartFromZero
    ));
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
        Decision::Fail(e) if e == ERR_NETWORK
    ));
}

#[test]
fn canceled_and_sink_error() {
    let mut st = rs(true, false, 0);
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::Canceled, SIZE, 3),
        Decision::Fail(e) if e == ERR_CANCELED
    ));
    assert!(matches!(
        transition(&mut st, &AttemptOutcome::SinkError, SIZE, 3),
        Decision::Fail(e) if e == ERR_NETWORK
    ));
}

#[test]
fn backoff_responds_to_cancel() {
    let t = TaskShared::new(1, "r".into(), String::new(), 1, 0, 0, 0);
    t.cancel.store(true, Ordering::Relaxed);
    assert!(!backoff_wait(&t, 1)); // 取消即时返回，不真等 1s
}
