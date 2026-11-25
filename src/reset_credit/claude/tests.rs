//! Fixture homes and the fake curl of `oauth::tests`; no network.
use super::*;
use crate::provider::claude::oauth::tests::{ACCESS, Fake, NEW_ACCESS, ORG};

fn status_body() -> Value {
    json!({"five_hour":{"utilization":100.0,"resets_at":"2026-11-27T05:00:00Z"},
        "cedar_ember":{"eligible":true,"at_limit":true,"exhausted":["five_hour"],
        "grants":[
            {"id":"grant_5h_a","label":"5-hour reset","resets_total":2,"resets_left":2,"starts_at":"2026-11-20T00:00:00Z",
             "ends_at":"2026-11-27T12:30:00.000Z","clears":["five_hour"],"paused":false,"usable_now":true,"type":"session"},
            {"id":"grant_wk","resets_total":1,"resets_left":1,"ends_at":"2026-11-28T00:00:00+02:00","clears":["seven_day"],
             "paused":true,"usable_now":false},
            {"id":"grant-fable","resets_left":1,"clears":["seven_day_overage_included"],"gives_back_weekly":true,"usable_now":true},
            {"id":"BAD ID","resets_left":5},
            {"id":"no_left"}
        ],
        "next_grant_id":"grant_5h_a","weekly_resets_at":"2026-11-30T00:00:00Z","cooldown_until":null}})
}
fn claim_ok(result: &str) -> String {
    json!({"result":result,"reason":null,"resets_left":1,"cleared":["five_hour"],"weekly_resets_at":"2026-11-30T00:00:00Z"})
        .to_string()
}
fn far() -> i64 {
    (crate::daemon::now() + 3600) * 1000
}
fn paths(f: &Fake) -> Paths {
    Paths::new(Some(f.tmp.path().join("state"))).unwrap()
}
fn claim<'a>(f: &'a Fake, paths: &'a Paths, credit: Option<&str>, id: &str) -> Claim<'a> {
    Claim {
        paths,
        client: &f.client,
        home: &f.home,
        name: "a",
        credit_id: credit.map(str::to_owned),
        kind: None,
        request_id: id.into(),
    }
}

#[test]
fn a_cedar_ember_block_becomes_reset_credits() {
    let c = parse_status(&status_body(), 7).unwrap().unwrap();
    // Only resets_left of grants that are not paused: 2 + 1 (the paused weekly is left out).
    assert_eq!(c.available, 3);
    assert_eq!(c.observed_at, 7);
    assert_eq!(c.eligible, Some(true));
    assert_eq!(c.at_limit, Some(true));
    assert_eq!(c.next_grant_id.as_deref(), Some("grant_5h_a"));
    assert_eq!(c.weekly_resets_at, Some(1_795_996_800));
    assert_eq!(c.cooldown_until, None);
    let rows = c.credits.as_ref().unwrap();
    assert_eq!(rows.len(), 3, "malformed grants are skipped");
    assert_eq!(rows[0].id, "grant_5h_a");
    assert_eq!(rows[0].status.as_deref(), Some("available"));
    assert_eq!(rows[0].kind.as_deref(), Some("session"));
    assert_eq!(rows[0].expires_at, Some(1_795_782_600));
    assert_eq!(
        (rows[0].resets_left, rows[0].resets_total),
        (Some(2), Some(2))
    );
    assert_eq!(rows[1].status.as_deref(), Some("paused"));
    assert_eq!(rows[1].kind.as_deref(), Some("weekly"));
    assert_eq!(rows[1].expires_at, Some(1_795_816_800), "offset applied");
    assert_eq!(rows[2].kind.as_deref(), Some("fable"));
    assert_eq!(rows[2].expires_at, None);
    // The serialized shape keeps the Codex keys and adds the Claude ones.
    let v = json!(c);
    for k in [
        "available",
        "observed_at",
        "credits",
        "eligible",
        "next_grant_id",
    ] {
        assert!(v.get(k).is_some(), "{k}");
    }
    assert!(v["credits"][0].get("id").is_some() && v["credits"][0].get("expires_at").is_some());
    // Grant choice.
    assert_eq!(choose(&c, None, 0).as_deref(), Some("grant_5h_a"));
    assert_eq!(choose(&c, Some("fable"), 0).as_deref(), Some("grant-fable"));
    assert_eq!(choose(&c, Some("weekly"), 0), None, "paused is not usable");
    assert_eq!(choose(&c, Some("session"), 1_795_782_600), None, "ended");
    // No block: not offered.
    assert!(parse_status(&json!({"five_hour":{}}), 1).unwrap().is_none());
    assert!(
        parse_status(
            &json!({"cedar_ember":{"eligible":false,"ineligible_reason":"tier"}}),
            1
        )
        .unwrap()
        .is_some_and(|c| c.available == 0 && c.ineligible_reason.as_deref() == Some("tier"))
    );
}

#[test]
fn malformed_status_answers_are_errors() {
    for bad in [
        json!([]),
        json!("x"),
        json!({"cedar_ember":{"grants":[]}}),
        json!({"cedar_ember":{"eligible":"yes"}}),
        json!({"cedar_ember":5}),
    ] {
        let e = parse_status(&bad, 1).unwrap_err();
        assert!(
            e.to_string().starts_with("RESET_CREDITS_UNREADABLE"),
            "{bad}: {e}"
        );
    }
    assert_eq!(parse_time("2026-11-27T05:00:00Z"), Some(1_795_755_600));
    for bad in [
        "",
        "2026-11-27",
        "2026-13-01T00:00:00Z",
        "2026-11-27T05:00:00",
        "x026-11-27T05:00:00Z",
    ] {
        assert_eq!(parse_time(bad), None, "{bad}");
    }
    assert_eq!(parse_kind("5-hour").unwrap(), "session");
    assert_eq!(parse_kind("Weekly").unwrap(), "weekly");
    assert!(parse_kind("monthly").is_err());
    assert!(valid_grant_id("grant_5h-a") && !valid_grant_id("Grant") && !valid_grant_id(""));
    assert!(valid_request_id(&uuid::Uuid::new_v4().to_string()));
}

#[test]
fn claim_answers_map_to_the_shared_outcomes() {
    assert_eq!(classify("reset", false), ("complete", Some("reset")));
    assert_eq!(
        classify("not_limited", false),
        ("complete", Some("nothingToReset"))
    );
    assert_eq!(
        classify("already_used", true),
        ("complete", Some("alreadyRedeemed"))
    );
    assert_eq!(
        classify("already_used", false),
        ("complete", Some("noCredit"))
    );
    assert_eq!(
        classify("ineligible", false),
        ("complete", Some("noCredit"))
    );
    assert_eq!(classify("cooldown", false), ("complete", Some("cooldown")));
    assert_eq!(classify("unavailable", false), ("uncertain", None));
    assert_eq!(classify("something_new", false), ("uncertain", None));
}

#[tokio::test]
async fn a_status_read_is_cached_and_a_bad_answer_is_reported() {
    let f = Fake::new();
    f.login(far());
    f.answer("usage", 200, &status_body().to_string());
    let c = read(&f.client, &f.home, "claude:a", false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.available, 3);
    assert_eq!(usage_field(&f.home, crate::daemon::now())["available"], 3);
    read(&f.client, &f.home, "claude:a", false).await.unwrap();
    assert_eq!(f.calls("usage"), 1, "reused within {CACHE_SECONDS} s");
    f.answer("usage", 200, "<html>");
    let e = read(&f.client, &f.home, "claude:a", true)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("RESET_CREDITS_UNREADABLE"), "{e}");
    f.answer("usage", 500, "{}");
    let e = read(&f.client, &f.home, "claude:a", true)
        .await
        .unwrap_err();
    assert!(
        e.to_string().starts_with("RESET_CREDITS_READ_FAILED"),
        "{e}"
    );
    assert_eq!(
        usage_field(&f.home, crate::daemon::now())["available"],
        3,
        "a failed read keeps the last value"
    );
    f.assert_argv_clean();
}

#[tokio::test]
async fn a_401_renews_once_and_a_second_401_is_auth_stale() {
    let f = Fake::new();
    f.login(far());
    f.renewed();
    f.answer("usage", 401, r#"{"type":"error"}"#);
    let e = read(&f.client, &f.home, "claude:a", true)
        .await
        .unwrap_err();
    let text = e.to_string();
    assert!(text.starts_with("AUTH_STALE"), "{text}");
    assert!(!text.contains("FIXTURE"));
    assert_eq!(
        f.calls("usage"),
        2,
        "sent again once, with the renewed token"
    );
    assert_eq!(f.calls("token"), 1);
    let reqs = f.read("usage-requests");
    assert!(reqs.contains(ACCESS) && reqs.contains(NEW_ACCESS));
    f.assert_argv_clean();
}

#[tokio::test]
async fn a_claim_is_journaled_before_sending_and_a_retry_reuses_the_request_id() {
    let f = Fake::new();
    f.login(far());
    let paths = paths(&f);
    f.answer("usage", 200, &status_body().to_string());
    f.answer("claim", 200, &claim_ok("reset"));
    let id = uuid::Uuid::new_v4().to_string();
    let v = redeem(claim(&f, &paths, None, &id)).await.unwrap();
    assert_eq!(v["outcome"], "reset");
    assert_eq!(
        v["credit_id"], "grant_5h_a",
        "next_grant_id when none is named"
    );
    assert_eq!(v["attempts"], 1);
    assert_eq!(v["claude"]["cleared"], json!(["five_hour"]));
    assert_eq!(v["rate_limits_after"]["reset_credits"]["available"], 3);
    assert_eq!(v["operation"]["provider"], "claude");
    // The journal held the attempt while the claim was in flight.
    let seen = f.read("journal-at-claim");
    assert!(seen.contains(&format!("{id}.json")), "{seen}");
    assert!(seen.contains("\"phase\": \"requesting\""), "{seen}");
    assert!(!seen.contains("FIXTURE"), "no token in the journal");
    let claims = f.read("claim-requests");
    assert!(
        claims.contains(&format!(
            "url = \"https://api.anthropic.com/api/organizations/{ORG}/reset_rate_limits\""
        )),
        "{claims}"
    );
    assert!(
        claims.contains("\\\"program\\\":\\\"cedar_ember\\\""),
        "{claims}"
    );
    assert!(
        claims.contains(&format!("\\\"request_id\\\":\\\"{id}\\\"")),
        "{claims}"
    );
    let record = super::super::Record::read(&paths, &id).unwrap();
    assert_eq!(
        (record.phase.as_str(), record.outcome.as_deref()),
        ("complete", Some("reset"))
    );
    assert_eq!(record.provider, ProviderId::Claude);

    // The same request id: the same grant again (even though the status now offers
    // another), one more attempt, and already_used confirms the first send.
    let mut other = status_body();
    other["cedar_ember"]["next_grant_id"] = json!("grant-fable");
    f.answer("usage", 200, &other.to_string());
    f.answer("claim", 200, &claim_ok("already_used"));
    let v = redeem(claim(&f, &paths, None, &id)).await.unwrap();
    assert_eq!(v["outcome"], "alreadyRedeemed");
    assert_eq!(v["credit_id"], "grant_5h_a");
    assert_eq!(v["attempts"], 2);
    assert_eq!(f.read("claim-requests").matches(&id).count(), 2);
    assert_eq!(f.read("claim-requests").matches("grant_5h_a").count(), 2);
    // Another grant under the same id is refused before anything is sent.
    let e = redeem(claim(&f, &paths, Some("grant-fable"), &id))
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("REQUEST_ID_CONFLICT"), "{e}");
    assert_eq!(f.calls("claim"), 2);
    assert_eq!(super::super::Record::list(&paths).unwrap().len(), 1);
    f.assert_argv_clean();
}

#[tokio::test]
async fn claim_failures_are_journaled_honestly() {
    let f = Fake::new();
    f.login(far());
    f.renewed();
    let paths = paths(&f);
    // 401 twice: renewed once, same request id, then AUTH_STALE; nothing claimed.
    f.answer("claim", 401, "{}");
    let id = uuid::Uuid::new_v4().to_string();
    let e = redeem(claim(&f, &paths, Some("grant_5h_a"), &id))
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("AUTH_STALE"), "{e}");
    assert_eq!(f.calls("claim"), 2);
    assert_eq!(f.calls("token"), 1);
    let r = super::super::Record::read(&paths, &id).unwrap();
    assert_eq!(r.phase, "failed");
    // Unreadable 200: uncertain, retry with the same id.
    f.answer("claim", 200, "not json");
    let id = uuid::Uuid::new_v4().to_string();
    let e = redeem(claim(&f, &paths, Some("grant_5h_a"), &id))
        .await
        .unwrap_err();
    assert!(
        e.to_string().starts_with("RESET_CREDIT_OUTCOME_UNKNOWN"),
        "{e}"
    );
    assert!(e.to_string().contains(&id));
    assert_eq!(
        super::super::Record::read(&paths, &id).unwrap().phase,
        "uncertain"
    );
    // `unavailable`: the API could not confirm it.
    f.answer("claim", 200, &claim_ok("unavailable"));
    let id = uuid::Uuid::new_v4().to_string();
    let e = redeem(claim(&f, &paths, Some("grant_5h_a"), &id))
        .await
        .unwrap_err();
    assert!(
        e.to_string().starts_with("RESET_CREDIT_OUTCOME_UNKNOWN"),
        "{e}"
    );
    // 429: refused, nothing claimed.
    f.answer("claim", 429, "{}");
    let id = uuid::Uuid::new_v4().to_string();
    let e = redeem(claim(&f, &paths, Some("grant_5h_a"), &id))
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("RESET_CREDIT_FAILED"), "{e}");
    assert_eq!(
        super::super::Record::read(&paths, &id).unwrap().phase,
        "failed"
    );
    // A timeout after sending: uncertain.
    f.set("claim-exit", "28");
    let id = uuid::Uuid::new_v4().to_string();
    let e = redeem(claim(&f, &paths, Some("grant_5h_a"), &id))
        .await
        .unwrap_err();
    assert!(
        e.to_string().starts_with("RESET_CREDIT_OUTCOME_UNKNOWN"),
        "{e}"
    );
    // No organization: AUTH_REQUIRED, nothing journaled or sent.
    fs_remove(&f.home.join(".claude.json"));
    let calls = f.calls("claim");
    let id = uuid::Uuid::new_v4().to_string();
    let e = redeem(claim(&f, &paths, Some("grant_5h_a"), &id))
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("AUTH_REQUIRED"), "{e}");
    assert!(!super::super::Record::exists(&paths, &id));
    assert_eq!(f.calls("claim"), calls);
    for line in super::super::Record::list(&paths).unwrap() {
        assert!(
            !json!(super::super::public(&line))
                .to_string()
                .contains("FIXTURE")
        );
    }
    f.assert_argv_clean();
}

#[tokio::test]
async fn no_usable_grant_sends_nothing() {
    let f = Fake::new();
    f.login(far());
    let paths = paths(&f);
    f.answer(
        "usage",
        200,
        &json!({"cedar_ember":{"eligible":true,"grants":[]}}).to_string(),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let v = redeem(claim(&f, &paths, None, &id)).await.unwrap();
    assert_eq!(
        (v["outcome"].as_str(), v["sent"].as_bool()),
        (Some("noCredit"), Some(false))
    );
    assert_eq!(f.calls("claim"), 0);
    assert!(!super::super::Record::exists(&paths, &id));
}

fn fs_remove(p: &Path) {
    std::fs::remove_file(p).unwrap();
}

/// Contract 1.65.0: a monitor read racing this account's own redeem. Every step runs under the
/// recorder's lock; the claim counts itself in the stored status, so the other read neither
/// reports the claimed grant as a reset made elsewhere nor stores a status older than it, and
/// its windows only count when its request was sent after the claim was answered.
#[test]
fn a_concurrent_read_during_a_redeem_is_not_an_external_redeem() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let t0 = 1_800_000_000;
    let r7 = "2027-01-20T08:00:00Z";
    let r7s = parse_time(r7).unwrap();
    let body = |left: u64, week: f64| {
        json!({"seven_day":{"utilization":week,"resets_at":r7},
            "cedar_ember":{"eligible":true,"grants":[
                {"id":"launch-fable","resets_left":left,"clears":["seven_day_overage_included"],
                 "gives_back_weekly":true,"usable_now":left > 0}]}})
    };
    let week = |at: i64| crate::provider::claude::project_usage(home, at).0["weekly"].clone();
    assert!(absorb(home, &body(1, 20.0), t0, None).credits.is_ok());
    assert_eq!(week(t0)["used_percent"], 20.0);
    // The claim is answered at t0 + 10.
    let started = claimed(home, "launch-fable", Some(0), t0 + 10, &[]).unwrap();
    assert_eq!(started, vec!["seven_day".to_owned()]);
    assert_eq!(cached(home).unwrap().1.unwrap().available, 0);
    let redeemed = week(t0 + 10)["cycle"].clone();
    // A monitor read sent before the answer (t0 + 5) arrives after it: nothing stored, its
    // higher pre-claim windows do not close the cycle.
    let stale = absorb(home, &body(1, 20.0), t0 + 5, None);
    assert!(stale.credits.is_ok());
    assert_eq!(cached(home).unwrap().1.unwrap().available, 0, "not stored");
    // A monitor read sent after the answer sees the lower count: not an external redeem.
    absorb(home, &body(0, 10.0), t0 + 20, None).windows.unwrap();
    let w = week(t0 + 21);
    assert_eq!(w["used_percent"], 10.0);
    assert_eq!(w["cycle"], redeemed);
    assert_eq!(w["cycle_reason"], "redeem");
    assert_eq!(w["resets_at"], r7s);
}
