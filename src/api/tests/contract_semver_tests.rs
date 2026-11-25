use super::super::*;
#[test]
fn api_version_negotiation_accepts_the_same_major_only() {
    for ok in ["1", "1.0", "1.25", "1.99.7", "1.25.0"] {
        assert_eq!(parse_api_version(ok), Ok(1), "{ok}");
    }
    for bad in ["0", "2", "2.0.0", "", "1.", "1.x", "1.2.3.4", "-1", "01x"] {
        assert!(parse_api_version(bad).is_err(), "{bad}");
    }
}
#[test]
fn contract_version_matches_revision_and_parts() {
    assert_eq!(
        CONTRACT_VERSION,
        format!("{CONTRACT_MAJOR}.{CONTRACT_MINOR}.{CONTRACT_PATCH}")
    );
    let v = version().unwrap();
    assert_eq!(v["contract_version"], CONTRACT_VERSION);
    assert_eq!(v["contract_revision"], CONTRACT_MINOR);
    let c = capabilities();
    assert_eq!(c["contract_version"], CONTRACT_VERSION);
    assert_eq!(c["contract_revision"], CONTRACT_MINOR);
}
