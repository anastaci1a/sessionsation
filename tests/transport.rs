use serde_json::json;
use sessionsation::rpc::{Rpc, read_frame};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn rejects_truncated_and_invalid_frames() {
    assert!(
        read_frame(&mut tokio::io::BufReader::new(&b"{\"id\":1}"[..]))
            .await
            .is_err()
    );
    assert!(
        read_frame(&mut tokio::io::BufReader::new(&b"nope\n"[..]))
            .await
            .is_err()
    );
    assert!(
        read_frame(&mut tokio::io::BufReader::new(&b""[..]))
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn rejects_oversized_frames_before_unbounded_allocation() {
    let buf = vec![b'x'; 16 * 1024 * 1024 + 1];
    assert!(
        read_frame(&mut tokio::io::BufReader::new(buf.as_slice()))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn fake_app_server_round_trip_and_graceful_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = tmp.path().join("fake-codex");
    std::fs::write(&binary,r##"#!/bin/sh
while IFS= read -r line; do
 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
 case "$line" in
  *'"method":"initialize"'*) printf '{"id":%s,"result":{"userAgent":"fake/1"}}\n' "$id" ;;
  *'"method":"account/read"'*) printf '{"id":%s,"result":{"account":null}}\n' "$id" ;;
  *'"method":"fail"'*) printf '{"id":%s,"error":{"code":401,"message":"SECRET_SENTINEL"}}\n' "$id" ;;
 esac
done
"##).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let rpc = Rpc::spawn(
        binary.to_str().unwrap(),
        &tmp.path().join("home"),
        tmp.path(),
    )
    .await
    .unwrap();
    assert!(rpc.request("account/read", json!({})).await.unwrap()["account"].is_null());
    let error = rpc
        .request("fail", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("SECRET_SENTINEL"));
    assert!(error.contains("401"));
    rpc.stop_idle().await.unwrap();
}
