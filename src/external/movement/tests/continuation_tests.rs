use super::super::*;
#[test]
fn public_evidence_requires_exact_client_turn_and_summary() {
    let mut page = json!({"data":[{"id":"turn","status":"inProgress","itemsView":"summary","items":[{"type":"userMessage","clientId":"job"}]}]});
    assert_eq!(
        continuation_observation(&page, "job", "turn"),
        Some("in_progress")
    );
    assert_eq!(continuation_observation(&page, "other", "turn"), None);
    assert_eq!(continuation_observation(&page, "job", "old"), None);
    page["data"][0]["status"] = json!("failed");
    assert_eq!(
        continuation_observation(&page, "job", "turn"),
        Some("failed")
    );
    page["data"][0]["itemsView"] = json!("full");
    assert_eq!(continuation_observation(&page, "job", "turn"), None);
}
