use super::*;
#[test]
fn identifiers_are_provider_qualified_and_mismatches_are_explicit() {
    assert_eq!(
        AccountRef::parse("claude:work").unwrap(),
        AccountRef {
            provider: ProviderId::Claude,
            name: "work".into()
        }
    );
    let bare = AccountRef::parse("work").unwrap_err().to_string();
    assert!(bare.contains("ACCOUNT_PROVIDER_REQUIRED"), "{bare}");
    assert!(
        AccountRef::parse("gpt:work")
            .unwrap_err()
            .to_string()
            .contains("UNKNOWN_PROVIDER")
    );
    assert!(AccountRef::parse("claude:../x").is_err());
    assert_eq!(
        AccountRef::parse("gemini:me").unwrap().provider,
        ProviderId::Gemini
    );
    let enrolled = [("work", ProviderId::Codex), ("home", ProviderId::Claude)];
    assert_eq!(
        resolve(enrolled, "work").unwrap().provider,
        ProviderId::Codex
    );
    assert_eq!(
        resolve(enrolled, "claude:home").unwrap().provider,
        ProviderId::Claude
    );
    let both = [("work", ProviderId::Codex), ("work", ProviderId::Claude)];
    let ambiguous = resolve(both, "work").unwrap_err().to_string();
    assert!(ambiguous.starts_with("ACCOUNT_AMBIGUOUS"), "{ambiguous}");
    assert_eq!(
        resolve(both, "claude:work").unwrap().provider,
        ProviderId::Claude
    );
    let mismatch = resolve(enrolled, "claude:work").unwrap_err().to_string();
    assert!(mismatch.contains("ACCOUNT_PROVIDER_MISMATCH"), "{mismatch}");
    assert!(mismatch.contains("codex:work"), "{mismatch}");
    assert!(
        resolve(enrolled, "codex:missing")
            .unwrap_err()
            .to_string()
            .contains("ACCOUNT_NOT_FOUND")
    );
    assert!(
        AccountRef::parse("claude:work")
            .unwrap()
            .require(ProviderId::Codex)
            .unwrap_err()
            .to_string()
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
}
#[test]
fn collisions_name_the_existing_owner() {
    let mut a = Account::new("work".into(), String::new());
    a.provider = ProviderId::Claude;
    let e = collision(&a).to_string();
    assert!(e.starts_with("ACCOUNT_NAME_EXISTS"), "{e}");
    assert!(e.contains("claude:work"), "{e}");
}
