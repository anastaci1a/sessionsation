use super::super::*;
use crate::provider::ProviderId;

fn state() -> State {
    let mut s = State::default();
    s.insert_account(Account::new("work".into(), String::new()));
    s.insert_account(Account::with_provider(
        ProviderId::Claude,
        "work".into(),
        String::new(),
    ));
    s.insert_account(Account::with_provider(
        ProviderId::Gemini,
        "solo".into(),
        String::new(),
    ));
    s
}
#[test]
fn the_same_name_coexists_under_two_providers() {
    let s = state();
    assert_eq!(s.accounts.len(), 3);
    assert!(s.accounts.contains_key("work"), "Codex keeps its bare key");
    assert!(s.accounts.contains_key("claude:work"));
    assert_eq!(
        s.account(ProviderId::Codex, "work").unwrap().provider,
        ProviderId::Codex
    );
    assert_eq!(
        s.account(ProviderId::Claude, "work").unwrap().provider,
        ProviderId::Claude
    );
    assert!(s.account(ProviderId::Gemini, "work").is_none());
}
#[test]
fn bare_names_resolve_only_while_unambiguous() {
    let s = state();
    assert_eq!(
        s.resolve_account("solo", None).unwrap().provider,
        ProviderId::Gemini
    );
    let e = s.resolve_account("work", None).unwrap_err().to_string();
    assert!(e.starts_with("ACCOUNT_AMBIGUOUS"), "{e}");
    assert!(e.contains("codex:work") && e.contains("claude:work"), "{e}");
    assert_eq!(
        s.resolve_account("work", Some(ProviderId::Claude))
            .unwrap()
            .provider,
        ProviderId::Claude
    );
    let e = s
        .resolve_account("solo", Some(ProviderId::Codex))
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("ACCOUNT_PROVIDER_MISMATCH"), "{e}");
    let e = s.resolve_account("nobody", None).unwrap_err().to_string();
    assert!(e.starts_with("ACCOUNT_NOT_FOUND"), "{e}");
}
#[test]
fn ordering_and_reordering_use_unique_keys() {
    let mut s = state();
    assert_eq!(s.normalize_order().len(), 3);
    let before: Vec<_> = s.ordered().iter().map(|a| a.key()).collect();
    assert!(s.reorder("claude:work", 2));
    assert_eq!(s.ordered()[2].key(), "claude:work");
    assert_eq!(s.ordered().len(), before.len());
}
#[test]
fn legacy_registries_are_rekeyed_once_per_provider() {
    let mut value = serde_json::json!({"accounts":{
        "work":{"name":"work","provider":"codex"},
        "home":{"name":"home","provider":"claude"},
        "g":{"name":"g","provider":"gemini"}}});
    let mut moved = crate::store::migrate_account_keys(&mut value);
    moved.sort();
    assert_eq!(moved, ["claude:home", "gemini:g"]);
    let keys: Vec<_> = value["accounts"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["claude:home", "gemini:g", "work"]);
    assert!(crate::store::migrate_account_keys(&mut value).is_empty());
}
