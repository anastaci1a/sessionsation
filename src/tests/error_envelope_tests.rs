use super::super::*;

#[test]
fn enospc_anywhere_in_the_chain_is_disk_full() {
    let raw = std::io::Error::from_raw_os_error(libc::ENOSPC);
    let error = anyhow::Error::new(raw).context("write the catalog");
    let envelope = error_envelope(&error);
    assert_eq!(envelope["error"]["code"], "DISK_FULL");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("DISK_FULL:"), "{message}");
    assert!(message.contains("No space left on device"), "{message}");
    assert_eq!(envelope["error"]["retryable"], false);
    // Another I/O failure keeps the generic code.
    let other = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EACCES));
    assert_eq!(error_envelope(&other)["error"]["code"], "OPERATION_FAILED");
}

#[test]
fn insufficient_space_carries_both_figures() {
    let error = anyhow::Error::new(sessionsation::disk_space::InsufficientSpace {
        operation: "archive".into(),
        path: "/root".into(),
        free_bytes: 5,
        needed_bytes: 9,
    });
    let envelope = error_envelope(&error);
    assert_eq!(envelope["error"]["code"], "INSUFFICIENT_DISK_SPACE");
    assert_eq!(
        (
            envelope["error"]["free_bytes"].as_u64(),
            envelope["error"]["needed_bytes"].as_u64()
        ),
        (Some(5), Some(9))
    );
}

#[test]
fn failed_wait_is_operation_failed_and_retryable_only_when_the_account_was_busy() {
    let failed = |detail: &str| {
        anyhow::Error::new(sessionsation::session_model::NotConfirmed {
            code: "OPERATION_FAILED",
            message: detail.into(),
            operation: json!({"phase":"failed","detail":detail}),
        })
    };
    let busy = error_envelope(&failed("ACCOUNT_BUSY: held (os error 11)"));
    assert_eq!(busy["error"]["code"], "OPERATION_FAILED");
    assert_eq!(busy["error"]["retryable"], true);
    assert_eq!(busy["error"]["operation"]["phase"], "failed");
    let other = error_envelope(&failed("HISTORY_CHANGED"));
    assert_eq!(other["error"]["code"], "OPERATION_FAILED");
    assert_eq!(other["error"]["retryable"], false);
    let timeout = error_envelope(&anyhow::Error::new(
        sessionsation::session_model::NotConfirmed {
            code: "OPERATION_TIMEOUT",
            message: "still running".into(),
            operation: json!({}),
        },
    ));
    assert_eq!(timeout["error"]["code"], "OPERATION_TIMEOUT");
    assert_eq!(timeout["error"]["retryable"], true);
}

/// Contract 1.69.3: a stop that failed after `--discard-queued` keeps its code and message and
/// reports what it discarded.
#[test]
fn a_failed_discarding_stop_reports_what_it_discarded() {
    let inner = anyhow::anyhow!("QUEUED_INPUT: queued input [\"m\"] would be stranded");
    let (top, full) = (inner.to_string(), format!("{inner:#}"));
    let value = json!({"discarded_messages":["a"],"not_discarded":[],"discarded_native_items":[]});
    let error = inner.context(sessionsation::api::Discarded { value, top, full });
    let envelope = error_envelope(&error);
    assert_eq!(envelope["error"]["code"], "QUEUED_INPUT");
    assert_eq!(
        envelope["error"]["message"],
        "QUEUED_INPUT: queued input [\"m\"] would be stranded"
    );
    assert_eq!(envelope["error"]["retryable"], false);
    assert_eq!(envelope["error"]["discarded_messages"], json!(["a"]));
    assert_eq!(envelope["error"]["not_discarded"], json!([]));
}
