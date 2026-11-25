use super::super::*;
use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
#[tokio::test]
async fn explicit_start_is_exact_once_and_lost_ack_is_never_replayed() {
    for lose_ack in [false, true] {
        let (_dir, paths, input, now) = fixture();
        let a = attachment_fixture(&paths, &input);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let task = scripted_server(&a.endpoint(), move |request| {
            assert_eq!(request["method"], "thread/queue/start");
            assert_eq!(request["params"]["queuedSubmissionId"], "only-this-item");
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (!lose_ack).then(|| json!({"turn":{"id":"started-turn","status":"inProgress"}}))
        })
        .await;
        let mut control = a.connect().await.unwrap();
        let mut ledger = Ledger::open(&paths).unwrap();
        let mut receipt = ledger.accept(&input, now).unwrap();
        receipt.state = "native_accepted".into();
        receipt.native_queue_id = Some("only-this-item".into());
        ledger.save(&receipt).unwrap();
        start_accepted(&mut control, &mut ledger, &mut receipt)
            .await
            .unwrap();
        assert_eq!(
            receipt.recovery_start.as_ref().unwrap().state,
            if lose_ack { "unknown" } else { "confirmed" }
        );
        assert!(receipt.correlation.is_none());
        drop(ledger);
        let mut ledger = Ledger::open(&paths).unwrap();
        let mut receipt = ledger.get(&input.message_id).unwrap().unwrap();
        start_accepted(&mut control, &mut ledger, &mut receipt)
            .await
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        task.abort();
    }
}
