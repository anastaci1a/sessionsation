use super::super::*;
use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_server};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_native_reads_report_the_exact_rpc_without_claiming_evidence() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    for method in ["thread/read", "thread/turns/list"] {
        let (_dir, paths, input, _) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let i: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"test","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let s = catalog::reconcile(&paths, &[i]).unwrap().remove(0);
        let listener = tokio::net::UnixListener::bind(a.endpoint()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(raw))) = ws.next().await {
                let req: Value = serde_json::from_str(&raw).unwrap();
                let name = req["method"].as_str().unwrap();
                let result = match name {
                    "initialized" => continue,
                    "initialize" => json!({"userAgent":"fixture/0.163.0 (Linux)"}),
                    "thread/read" => {
                        json!({"thread":{"id":req["params"]["threadId"],"status":{"type":"idle"}}})
                    }
                    "thread/turns/list" => json!({"data":[{"id":"last","status":"completed"}]}),
                    _ => panic!("Unexpected mutating request {name}"),
                };
                if name == method {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
                if ws
                    .send(Message::Text(
                        json!({"id":req["id"],"result":result}).to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let out = observe_public(&paths, &s.id, &s.generation())
            .await
            .unwrap();
        server.abort();
        assert_eq!(out["observation"]["failure"]["method"], method);
        assert_eq!(out["observation"]["failure"]["budget_ms"], 3000);
        assert_eq!(out["runtime_state"], "unobserved");
        assert_eq!(out["observation"]["status"], "unobserved");
        assert!(out["observed_at"].is_null() && out["valid_until"].is_null());
        for action in ["submit", "queue", "resume"] {
            assert_eq!(out["actions"][action]["available"], false);
        }
    }
}

#[tokio::test]
async fn passive_observation_uses_only_read_only_rpc_and_rejects_catalog_races() {
    for (busy, rename) in [(false, false), (true, false), (false, true)] {
        let (_dir, paths, input, _) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let i:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"test","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let s = catalog::reconcile(&paths, &[i]).unwrap().remove(0);
        let changed = Arc::new(AtomicBool::new(false));
        let flag = changed.clone();
        let cp = paths.clone();
        let cs = s.clone();
        let server=scripted_server(&a.endpoint(),move |req|{
            match req["method"].as_str().unwrap() {
                "thread/read"=>{
                    assert_eq!(req["params"]["includeTurns"],false);
                    if rename && !flag.swap(true,Ordering::SeqCst) {catalog::rename(&cp,&cs.id,&cs.generation(),"changed").unwrap();}
                    Some(json!({"thread":{"id":req["params"]["threadId"],"status":{"type":if busy {"active"} else {"idle"}},"history":"SECRET"}}))
                },
                "thread/turns/list"=>{assert_eq!(req["params"]["itemsView"],"summary");assert_eq!(req["params"]["limit"],1);Some(json!({"data":[{"id":"turn-1","status":"completed","private":"SECRET"}]}))},
                other=>panic!("Readiness must never call {other}"),
            }
        }).await;
        let out = observe_public(&paths, &s.id, &s.generation()).await;
        server.abort();
        if rename {
            assert!(out.unwrap_err().to_string().contains("STALE_TARGET"));
        } else {
            let out = out.unwrap();
            assert_eq!(out["state"], if busy { "working" } else { "idle" });
            assert_eq!(out["actions"]["submit"]["available"], !busy);
            assert_eq!(out["actions"]["queue"]["available"], true);
            assert!(!out.to_string().contains("SECRET"));
        }
    }
}
