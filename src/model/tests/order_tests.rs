use super::super::*;
#[test]
fn order_migration_is_alphabetical_idempotent_and_appends_new_accounts() {
    let mut state = State::default();
    for n in ["b", "a", "c"] {
        state
            .accounts
            .insert(n.into(), Account::new(n.into(), String::new()));
    }
    assert_eq!(state.normalize_order(), ["a", "b", "c"]);
    assert!(state.normalize_order().is_empty());
    state
        .accounts
        .insert("0new".into(), Account::new("0new".into(), String::new()));
    state.normalize_order();
    let names = |s: &State| {
        s.ordered()
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&state), ["a", "b", "c", "0new"]);
    assert!(state.reorder("0new", 0));
    assert_eq!(names(&state), ["0new", "a", "b", "c"]);
    assert!(!state.reorder("0new", 0));
    assert!(state.reorder("0new", 99));
    assert_eq!(names(&state), ["a", "b", "c", "0new"]);
}
