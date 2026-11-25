use super::super::*;
use serde_json::json;
fn list() -> Vec<Value> {
    vec![
        json!({"workspace_id":"wR","label":"localica"}),
        json!({"workspace_id":"w21","label":"sessionstuffs"}),
    ]
}
#[test]
fn a_space_resolves_by_id_then_by_visible_name() {
    let l = list();
    assert_eq!(resolve_space(&l, "wR").as_deref(), Some("wR"));
    // A stale stored UUID that is also never a name: not found.
    assert_eq!(
        resolve_space(&l, "5ad10912-0000-4000-8000-000000000000"),
        None
    );
    assert_eq!(resolve_space(&l, "sessionstuffs").as_deref(), Some("w21"));
    assert_eq!(resolve_space(&l, "LocaLica").as_deref(), Some("wR"));
}
#[test]
fn an_existing_new_space_name_is_reused_in_every_request_shape() {
    let l = list();
    let existing = crate::launch::Space::New {
        label: "localica".into(),
    };
    assert_eq!(
        crate::launch::normalize_space(&l, &existing),
        crate::launch::Space::Existing { id: "wR".into() }
    );
    let gone = crate::launch::Space::Existing {
        id: "localica".into(),
    };
    assert_eq!(
        crate::launch::normalize_space(&l, &gone),
        crate::launch::Space::Existing { id: "wR".into() }
    );
    let fresh = crate::launch::Space::New {
        label: "brand new".into(),
    };
    assert_eq!(crate::launch::normalize_space(&l, &fresh), fresh);
}
