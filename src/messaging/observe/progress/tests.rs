use super::*;
use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_resume_without_partial_claim_and_revalidate_live_head() {
    let (_tmp, paths, input, now) = fixture();
    let a = attachment_fixture(&paths, &input);
    let receipt = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
    let history = a.home.join("sessions/test.jsonl");
    fs::create_dir_all(history.parent().unwrap()).unwrap();
    fs::write(&history, "synthetic").unwrap();
    let phase = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let (mode, count, path, id, client) = (
        phase.clone(),
        reads.clone(),
        history.clone(),
        a.id.clone(),
        receipt.message_id.clone(),
    );
    let server = scripted_server(&a.endpoint(),move |r| {
        if r["method"] == "thread/read" {
            return Some(json!({"thread":{"id":id,"path":path}}));
        }
        assert_eq!(r["method"],"thread/turns/list");
        if count.fetch_add(1,Ordering::SeqCst)==0 { std::thread::sleep(Duration::from_millis(3250)); }
        let turn = json!({"id":"turn-1","status":"completed","itemsView":"summary","items":[
            {"type":"userMessage","id":"item-1","clientId":client,"content":"SECRET_NOT_STORED"},
            {"type":"agentMessage","id":"reply-1","text":"Visible answer","phase":"final_answer"}]});
        Some(if r["params"]["cursor"].is_null() {
            json!({"data":if mode.load(Ordering::SeqCst)==1 {json!([])}else{json!([turn])},"nextCursor":"older"})
        } else { json!({"data":[],"nextCursor":null}) })
    }).await;
    let mut c = a.connect().await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
            "reply_scan_pending"
        );
    }
    // The completed candidate is still private until older pages and the
    // live head agree. A changed in-memory projection invalidates it.
    phase.store(1, Ordering::SeqCst);
    assert_eq!(
        read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
        "reply_scan_changed"
    );
    phase.store(0, Ordering::SeqCst);
    for _ in 0..2 {
        assert_eq!(
            read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
            "reply_scan_pending"
        );
    }
    let file = paths
        .root
        .join("messaging/observations")
        .join(format!("{}.json", receipt.message_id));
    assert!(
        !fs::read_to_string(&file)
            .unwrap()
            .contains("SECRET_NOT_STORED")
    );
    let found = read(&paths, &a, &mut c, &receipt).await.unwrap().unwrap();
    assert_eq!(found.state, "completed_with_reply");
    assert_eq!(reads.load(Ordering::SeqCst), 6);
    assert!(!file.exists());
    server.abort();
}
#[tokio::test]
async fn duplicate_across_polls_is_rejected_and_disk_change_resets_scan() {
    let (_tmp, paths, input, now) = fixture();
    let a = attachment_fixture(&paths, &input);
    let receipt = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
    let history = a.home.join("sessions/test.jsonl");
    fs::create_dir_all(history.parent().unwrap()).unwrap();
    fs::write(&history, "synthetic").unwrap();
    let cursors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (seen, path, id, client) = (
        cursors.clone(),
        history.clone(),
        a.id.clone(),
        receipt.message_id.clone(),
    );
    let server = scripted_server(&a.endpoint(),move |r| {
        if r["method"]=="thread/read" { return Some(json!({"thread":{"id":id,"path":path}})); }
        assert_eq!(r["method"],"thread/turns/list");
        seen.lock().unwrap().push(r["params"]["cursor"].clone());
        Some(json!({"data":[{"id":"turn-1","status":"inProgress","itemsView":"summary","items":[{"type":"userMessage","id":"item-1","clientId":client}]}],"nextCursor":if r["params"]["cursor"].is_null(){json!("older")}else{Value::Null}}))
    }).await;
    let mut c = a.connect().await.unwrap();
    assert_eq!(
        read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
        "reply_scan_pending"
    );
    fs::write(&history, "changed history").unwrap();
    assert_eq!(
        read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
        "reply_scan_pending"
    );
    assert_eq!(
        read(&paths, &a, &mut c, &receipt).await.unwrap_err().0,
        "ambiguous_native_correlation"
    );
    assert_eq!(
        *cursors.lock().unwrap(),
        vec![Value::Null, Value::Null, json!("older")]
    );
    assert!(receipt.correlation.is_none());
    server.abort();
}
