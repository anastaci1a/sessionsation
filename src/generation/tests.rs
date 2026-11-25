//! Golden generation vectors, recorded on main 9e1f72b (before `generation.rs`
//! owned a hasher). A stored `expected_generation` or account `revision` stays
//! valid only while these bytes never change.

use crate::catalog::Session;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// A Codex catalog entry as the catalog stores it (attached, unscoped).
fn codex_entry() -> Value {
    json!({
        "id": "6f1c2d4e-1111-4a2b-8c3d-000000000001",
        "name": "worker",
        "provider": "codex",
        "account": "work",
        "conversation_id": "019a0000-0000-7000-8000-000000000001",
        "revision": 7,
        "attachment": {"host": "herdr", "target": "herdr/1/2", "pid": 4242, "process_start": 987654},
    })
}

/// A Claude entry with a directory and continued-from conversations.
fn claude_entry() -> Value {
    json!({
        "id": "6f1c2d4e-2222-4a2b-8c3d-000000000002",
        "name": "lead",
        "provider": "claude",
        "account": "main",
        "conversation_id": "11111111-1111-4111-8111-111111111111",
        "revision": 3,
        "attachment": null,
        "directory": "/work/project",
        "previous_conversation_ids": ["22222222-2222-4222-8222-222222222222"],
    })
}

fn session(mut v: Value, space: Option<&str>) -> Session {
    if let Some(space) = space {
        v["space"] = json!(space);
    }
    serde_json::from_value(v).unwrap()
}

/// The messaging attachment formula (`messaging::Attachment::generation`) with fixed parts.
fn attachment_formula(parts: Value) -> String {
    format!("{:x}", Sha256::digest(parts.to_string().as_bytes()))
}

/// The account `revision` formula (`accounts.rs`, both fingerprints).
fn revision_formula(fingerprint: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(fingerprint).unwrap())
    )
}

fn attachment_parts() -> Value {
    json!([
        "6f1c2d4e-1111-4a2b-8c3d-000000000001",
        "/home/user/.codex",
        4242,
        987654,
        "0c4f6a2e-5d1b-4c3a-9e8f-7a6b5c4d3e2f\n",
        [2049, 1234567, 1700000000, 123456789]
    ])
}

fn attachment_parts_without_endpoint() -> Value {
    json!([
        "6f1c2d4e-1111-4a2b-8c3d-000000000001",
        "/home/user/.codex",
        4242,
        987654,
        "",
        null
    ])
}

fn codex_fingerprint() -> Value {
    json!({"name":"work","enabled":true,"auth_required":false,
        "stored":[{"path":"auth.json","bytes":120}],"active":true,"loaded":[],"operations":[],
        "affected":[{"id":"6f1c2d4e-1111-4a2b-8c3d-000000000001","name":"worker","conversation_id":"019a0000-0000-7000-8000-000000000001"}]})
}

fn generic_fingerprint() -> Value {
    json!({"provider":"claude","name":"main","enabled":true,"auth_required":false,
        "config_home":"/home/user/.claude-main","history":[["11111111-1111-4111-8111-111111111111", 2048]],
        "active":false,"processes":[],"operations":[],"affected":[]})
}

pub(super) const GOLDEN: &[(&str, &str)] = &[
    (
        "catalog codex",
        "df26385655a52640ec0617ca697ad3df8a53a81faa8eb72e856b1d811ec624e3",
    ),
    (
        "catalog claude",
        "499b173e08902663730522724412b6001743fc25035e154da7da3d7545ccb4c3",
    ),
    ("account codex:work", "41d5e242a62a1242"),
    ("account claude:main", "6a71762828a88b31"),
    (
        "attachment",
        "e6b7d756d78c3bb0e3b0773952340933d39b2c77bf5097d3380fa659f9e4cdec",
    ),
    (
        "attachment without endpoint",
        "359caedcec1a62ed176bb7ef81f8eb3230f7ac33c7ac2d87ec5c4ef4fb4de2b1",
    ),
    (
        "revision codex",
        "cdfe5e3acca93c5ff107c5dd737035d65ef90e4ff9207b870a256788e2337d72",
    ),
    (
        "revision generic",
        "c3d340003be7961c494e4588b621161e83af9d61112a636165954118d77a1d17",
    ),
];

pub(super) fn golden(name: &str) -> &'static str {
    GOLDEN
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, hex)| *hex)
        .unwrap_or_else(|| panic!("no golden vector {name}"))
}

/// Every generation hasher, through the code path clients reach today.
fn computed() -> Vec<(&'static str, String)> {
    vec![
        ("catalog codex", session(codex_entry(), None).generation()),
        ("catalog claude", session(claude_entry(), None).generation()),
        (
            "account codex:work",
            crate::accounts::generation("codex", "work", None, None),
        ),
        (
            "account claude:main",
            crate::accounts::generation(
                "claude",
                "main",
                Some("Main"),
                Some("/home/user/.claude-main"),
            ),
        ),
        ("attachment", attachment_formula(attachment_parts())),
        (
            "attachment without endpoint",
            attachment_formula(attachment_parts_without_endpoint()),
        ),
        ("revision codex", revision_formula(&codex_fingerprint())),
        ("revision generic", revision_formula(&generic_fingerprint())),
    ]
}

#[test]
fn golden_vectors() {
    let computed = computed();
    assert_eq!(computed.len(), GOLDEN.len());
    for (name, hex) in computed {
        assert_eq!(hex, golden(name), "golden generation vector {name} changed");
    }
}

/// A Space label never enters the generation (catalog.rs: recording or healing a
/// space does not change it by itself).
#[test]
fn golden_vectors_ignore_the_space() {
    for (entry, name) in [
        (codex_entry(), "catalog codex"),
        (claude_entry(), "catalog claude"),
    ] {
        assert_eq!(session(entry, Some("localica")).generation(), golden(name));
    }
}

/// The owner reproduces every recorded vector from the same inputs.
#[test]
fn the_owner_reproduces_the_golden_vectors() {
    let of = |v: &Value| super::of(v).unwrap();
    assert_eq!(of(&attachment_parts()), golden("attachment"));
    assert_eq!(
        of(&attachment_parts_without_endpoint()),
        golden("attachment without endpoint")
    );
    assert_eq!(of(&codex_fingerprint()), golden("revision codex"));
    assert_eq!(of(&generic_fingerprint()), golden("revision generic"));
    assert_eq!(
        super::of(&session(codex_entry(), None)).unwrap(),
        golden("catalog codex")
    );
    assert_eq!(
        &of(&json!(["codex", "work", null, null]))[..16],
        golden("account codex:work")
    );
}
