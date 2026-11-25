use super::super::*;

#[test]
fn a_stop_that_timed_out_is_retryable() {
    let e = error_envelope(&anyhow::anyhow!(
        "STOP_TIMEOUT: Claude process 7 did not exit within 10s of SIGTERM; it was not killed"
    ));
    assert_eq!(e["error"]["code"], "STOP_TIMEOUT");
    assert_eq!(e["error"]["retryable"], true);
}
