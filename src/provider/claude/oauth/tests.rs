//! Fixture credentials and a fake curl only; no real account, file or network.
use super::*;
use std::os::unix::fs::PermissionsExt;

pub(crate) const ACCESS: &str = "sk-ant-oat01-FIXTURE-ACCESS-old";
pub(crate) const REFRESH: &str = "sk-ant-ort01-FIXTURE-REFRESH-old";
pub(crate) const NEW_ACCESS: &str = "sk-ant-oat01-FIXTURE-ACCESS-new";
pub(crate) const NEW_REFRESH: &str = "sk-ant-ort01-FIXTURE-REFRESH-new";
pub(crate) const ORG: &str = "6f1f3d5e-8a52-4b7e-9c1d-2e3f4a5b6c7d";

/// A disposable account home and a fake curl that records argv and answers from files
/// (`<kind>-status`, `<kind>-body`, `<kind>-sleep`, `<kind>-exit`, kinds token/usage/claim).
pub(crate) struct Fake {
    pub tmp: tempfile::TempDir,
    pub home: PathBuf,
    pub dir: PathBuf,
    pub client: Client,
}
impl Fake {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = tmp.path().join("fake");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let journal = tmp.path().join("state/operations/account-reset-credits");
        let script = format!(
            r#"#!/bin/sh
d='{d}'
printf '%s\n' "$*" >> "$d/argv"
req="$d/req-$$"
cat > "$req"
url=$(sed -n 's/^url = "\(.*\)"$/\1/p' "$req")
case "$url" in
  *oauth/token*) k=token ;;
  *reset_rate_limits*) k=claim; ls '{j}' > "$d/journal-at-claim" 2>/dev/null; cat '{j}'/*.json >> "$d/journal-at-claim" 2>/dev/null ;;
  *oauth/usage*) k=usage ;;
  *) k=other ;;
esac
echo "$k" >> "$d/calls"
cat "$req" >> "$d/$k-requests"
echo '----' >> "$d/$k-requests"
[ -f "$d/$k-sleep" ] && sleep "$(cat "$d/$k-sleep")"
[ -f "$d/$k-exit" ] && exit "$(cat "$d/$k-exit")"
s=$(cat "$d/$k-status" 2>/dev/null || echo 500)
b=$(cat "$d/$k-body" 2>/dev/null)
printf '%s\n%s' "$b" "$s"
"#,
            d = dir.display(),
            j = journal.display()
        );
        let curl = dir.join("curl");
        fs::write(&curl, script).unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
        let mut client = Client::new(&curl);
        client.lock_attempts = 3;
        client.lock_pause = (Duration::from_millis(50), Duration::from_millis(100));
        client.max_time = Duration::from_secs(10);
        Self {
            tmp,
            home,
            dir,
            client,
        }
    }
    /// `.credentials.json` with extra fields that must survive a write-back.
    pub fn login(&self, expires_at_ms: i64) {
        let v = json!({"claudeAiOauth":{"accessToken":ACCESS,"refreshToken":REFRESH,"expiresAt":expires_at_ms,
            "scopes":["user:inference","user:profile","user:sessions:claude_code"],"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"},
            "mcpOAuth":{"server|abc":{"accessToken":"other-fixture"}}});
        let p = credentials_path(&self.home);
        fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            self.home.join(".claude.json"),
            json!({"oauthAccount":{"organizationUuid":ORG,"accountUuid":"x"},"projects":{}})
                .to_string(),
        )
        .unwrap();
    }
    pub fn answer(&self, kind: &str, status: u16, body: &str) {
        fs::write(self.dir.join(format!("{kind}-status")), status.to_string()).unwrap();
        fs::write(self.dir.join(format!("{kind}-body")), body).unwrap();
    }
    pub fn set(&self, file: &str, text: &str) {
        fs::write(self.dir.join(file), text).unwrap();
    }
    pub fn read(&self, file: &str) -> String {
        fs::read_to_string(self.dir.join(file)).unwrap_or_default()
    }
    pub fn calls(&self, kind: &str) -> usize {
        self.read("calls").lines().filter(|l| *l == kind).count()
    }
    pub fn renewed(&self) {
        self.answer(
            "token",
            200,
            &json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":28800,
                "scope":"user:inference user:profile user:sessions:claude_code"})
            .to_string(),
        );
    }
    pub fn file(&self) -> Value {
        serde_json::from_slice(&fs::read(credentials_path(&self.home)).unwrap()).unwrap()
    }
    /// No token in argv: every curl call is exactly `-q -K -`.
    pub fn assert_argv_clean(&self) {
        let argv = self.read("argv");
        assert!(!argv.is_empty(), "curl was never called");
        for line in argv.lines() {
            assert_eq!(line, "-q -K -", "argv must carry no request data");
        }
        for secret in [ACCESS, REFRESH, NEW_ACCESS, NEW_REFRESH] {
            assert!(!argv.contains(secret));
        }
    }
}
fn far() -> i64 {
    now_ms() + 3_600_000
}

#[test]
fn expiry_uses_claude_codes_five_minute_margin() {
    let now = 1_000_000_000;
    assert!(!expired(None, now));
    assert!(!expired(Some(now + EXPIRY_SKEW_MS + 1), now));
    assert!(expired(Some(now + EXPIRY_SKEW_MS), now));
    assert!(expired(Some(now - 1), now));
    assert_eq!(format!("{:?}", Secret::new(ACCESS)), "Secret(redacted)");
}

#[test]
fn curl_config_escapes_and_refuses_control_characters() {
    let c = Client::new("/x");
    let t = config_text(
        &c,
        "POST",
        "https://h/p",
        &[("A", "b\"c\\d")],
        Some("{\"k\":1}"),
    )
    .unwrap();
    assert!(t.contains("header = \"A: b\\\"c\\\\d\"\n"), "{t}");
    assert!(t.contains("data-raw = \"{\\\"k\\\":1}\"\n"), "{t}");
    assert!(t.contains("proto = \"=https\""));
    assert!(config_text(&c, "GET", "https://h/\n", &[], None).is_err());
}

#[tokio::test]
async fn a_valid_token_is_used_without_any_request() {
    let f = Fake::new();
    f.login(far());
    let t = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap();
    assert_eq!(t.expose(), ACCESS);
    assert_eq!(f.read("calls"), "");
}

#[tokio::test]
async fn an_expired_token_is_renewed_under_the_lock_and_written_back_preserving_fields() {
    let f = Fake::new();
    f.login(now_ms() + 60_000); // inside the 5-minute margin
    f.renewed();
    let t = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap();
    assert_eq!(t.expose(), NEW_ACCESS);
    assert_eq!(f.calls("token"), 1);
    let body = f.read("token-requests");
    assert!(
        body.contains("url = \"https://platform.claude.com/v1/oauth/token\""),
        "{body}"
    );
    assert!(
        body.contains("\\\"grant_type\\\":\\\"refresh_token\\\""),
        "{body}"
    );
    assert!(
        body.contains(&format!("\\\"client_id\\\":\\\"{CLIENT_ID}\\\"")),
        "{body}"
    );
    assert!(body.contains("user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins"), "{body}");
    let v = f.file();
    let o = &v["claudeAiOauth"];
    assert_eq!(o["accessToken"], NEW_ACCESS);
    assert_eq!(o["refreshToken"], NEW_REFRESH);
    assert!(o["expiresAt"].as_i64().unwrap() > now_ms() + 28_000_000);
    assert_eq!(o["subscriptionType"], "max");
    assert_eq!(o["rateLimitTier"], "default_claude_max_20x");
    assert_eq!(v["mcpOAuth"]["server|abc"]["accessToken"], "other-fixture");
    let mode = fs::metadata(credentials_path(&f.home))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // Locks released, no temp file left.
    assert!(!f.home.join(REFRESH_LOCK).exists());
    assert!(!f.home.join(STORAGE_LOCK).exists());
    assert!(!legacy_lock_path(&f.home).exists());
    let names: Vec<_> = fs::read_dir(&f.home)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        names.iter().all(|n| !n.to_string_lossy().contains(".tmp")),
        "{names:?}"
    );
    f.assert_argv_clean();
}

#[tokio::test]
async fn concurrent_renewals_refresh_once_and_the_second_uses_the_first_result() {
    let f = Fake::new();
    f.login(now_ms() - 1);
    f.renewed();
    f.set("token-sleep", "1");
    let mut client = f.client.clone();
    client.lock_attempts = 40;
    let (a, b) = tokio::join!(access_token(&client, &f.home, "claude:a", None), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        access_token(&client, &f.home, "claude:a", None).await
    });
    assert_eq!(a.unwrap().expose(), NEW_ACCESS);
    assert_eq!(b.unwrap().expose(), NEW_ACCESS);
    assert_eq!(
        f.calls("token"),
        1,
        "the second process must not refresh again"
    );
}

#[tokio::test]
async fn the_credentials_are_reread_after_taking_the_lock() {
    let f = Fake::new();
    f.login(now_ms() - 1);
    f.renewed();
    // Another process holds the lock and renews meanwhile.
    let lock = f.home.join(REFRESH_LOCK);
    fs::create_dir(&lock).unwrap();
    let mut client = f.client.clone();
    client.lock_attempts = 40;
    let home = f.home.clone();
    let sibling = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let v = json!({"claudeAiOauth":{"accessToken":"sk-ant-oat01-FIXTURE-SIBLING","refreshToken":"sk-ant-ort01-FIXTURE-SIBLING",
            "expiresAt":now_ms() + 3_600_000,"scopes":["user:inference"]}});
        fs::write(credentials_path(&home), v.to_string()).unwrap();
        fs::remove_dir(home.join(REFRESH_LOCK)).unwrap();
    });
    let t = access_token(&client, &f.home, "claude:a", None)
        .await
        .unwrap();
    sibling.await.unwrap();
    assert_eq!(t.expose(), "sk-ant-oat01-FIXTURE-SIBLING");
    assert_eq!(
        f.calls("token"),
        0,
        "the sibling's renewal is used, not repeated"
    );
}

#[tokio::test]
async fn a_held_lock_is_auth_stale_and_nothing_is_sent() {
    let f = Fake::new();
    f.login(now_ms() - 1);
    f.renewed();
    fs::create_dir(f.home.join(REFRESH_LOCK)).unwrap();
    let e = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap_err();
    let text = e.to_string();
    assert!(text.starts_with("AUTH_STALE"), "{text}");
    assert!(
        text.contains("A running Claude session on the account refreshes it"),
        "{text}"
    );
    assert!(!text.contains("FIXTURE"));
    assert_eq!(
        f.read("calls"),
        "",
        "no request while another process renews"
    );
    // The other holder's lock is untouched.
    assert!(f.home.join(REFRESH_LOCK).is_dir());
    // A held legacy lock is contention too.
    fs::remove_dir(f.home.join(REFRESH_LOCK)).unwrap();
    fs::create_dir(legacy_lock_path(&f.home)).unwrap();
    let e = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("AUTH_STALE"));
    assert_eq!(f.read("calls"), "");
    assert!(
        !f.home.join(REFRESH_LOCK).exists(),
        "the primary lock is released"
    );
}

#[tokio::test]
async fn a_lock_left_by_a_dead_holder_is_taken_over_after_sixty_seconds() {
    let f = Fake::new();
    f.login(now_ms() - 1);
    f.renewed();
    let lock = f.home.join(REFRESH_LOCK);
    fs::create_dir(&lock).unwrap();
    let old = std::ffi::CString::new(lock.as_os_str().as_encoded_bytes()).unwrap();
    let past = libc::timespec {
        tv_sec: (now_ms() / 1000 - 120) as libc::time_t,
        tv_nsec: 0,
    };
    unsafe { libc::utimensat(libc::AT_FDCWD, old.as_ptr(), [past, past].as_ptr(), 0) };
    let t = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap();
    assert_eq!(t.expose(), NEW_ACCESS);
}

#[tokio::test]
async fn a_revoked_refresh_token_is_auth_required_and_the_file_is_kept() {
    let f = Fake::new();
    f.login(now_ms() - 1);
    f.answer(
        "token",
        400,
        r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#,
    );
    let before = fs::read(credentials_path(&f.home)).unwrap();
    let e = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("AUTH_REQUIRED"), "{e}");
    assert_eq!(fs::read(credentials_path(&f.home)).unwrap(), before);
    // A transient failure is AUTH_STALE (retryable).
    f.answer("token", 503, "busy");
    let e = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("AUTH_STALE"), "{e}");
    // No refresh token at all: AUTH_REQUIRED without a request.
    let calls = f.calls("token");
    fs::write(
        credentials_path(&f.home),
        json!({"claudeAiOauth":{"accessToken":ACCESS,"expiresAt":1}}).to_string(),
    )
    .unwrap();
    let e = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("AUTH_REQUIRED"), "{e}");
    assert_eq!(f.calls("token"), calls);
    fs::remove_file(credentials_path(&f.home)).unwrap();
    let e = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("AUTH_REQUIRED"), "{e}");
}

#[tokio::test]
async fn a_sibling_write_during_the_request_wins_the_compare_and_swap() {
    let f = Fake::new();
    f.login(now_ms() - 1);
    f.renewed();
    f.set("token-sleep", "1");
    let home = f.home.clone();
    let sibling = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut v: Value =
            serde_json::from_slice(&fs::read(credentials_path(&home)).unwrap()).unwrap();
        v["claudeAiOauth"]["accessToken"] = json!("sk-ant-oat01-FIXTURE-SIBLING");
        v["claudeAiOauth"]["refreshToken"] = json!("sk-ant-ort01-FIXTURE-SIBLING");
        fs::write(credentials_path(&home), v.to_string()).unwrap();
    });
    let t = access_token(&f.client, &f.home, "claude:a", None)
        .await
        .unwrap();
    sibling.await.unwrap();
    assert_eq!(t.expose(), "sk-ant-oat01-FIXTURE-SIBLING");
    assert_eq!(
        f.file()["claudeAiOauth"]["refreshToken"],
        "sk-ant-ort01-FIXTURE-SIBLING"
    );
}

#[tokio::test]
async fn a_refused_token_is_renewed_even_before_expiry_unless_already_replaced() {
    let f = Fake::new();
    f.login(far());
    f.renewed();
    let t = access_token(&f.client, &f.home, "claude:a", Some(&Secret::new(ACCESS)))
        .await
        .unwrap();
    assert_eq!(t.expose(), NEW_ACCESS);
    assert_eq!(f.calls("token"), 1);
    // The refused token is no longer stored: use the stored one, no request.
    let t = access_token(&f.client, &f.home, "claude:a", Some(&Secret::new(ACCESS)))
        .await
        .unwrap();
    assert_eq!(t.expose(), NEW_ACCESS);
    assert_eq!(f.calls("token"), 1);
}

#[tokio::test]
async fn the_bearer_goes_on_stdin_with_claude_codes_headers() {
    let f = Fake::new();
    f.login(far());
    f.answer("usage", 200, "{}");
    let r = api(
        &f.client,
        &Secret::new(ACCESS),
        "GET",
        "/api/oauth/usage?cedar_ember=1&skip_spend=1",
        None,
    )
    .await
    .unwrap();
    assert_eq!((r.status, r.body.as_str()), (200, "{}"));
    let req = f.read("usage-requests");
    assert!(
        req.contains(&format!("header = \"Authorization: Bearer {ACCESS}\"")),
        "{req}"
    );
    assert!(req.contains("header = \"anthropic-beta: oauth-2025-04-20\""));
    assert!(req.contains("header = \"User-Agent: claude-cli/2.1.314 (external, cli)\""));
    assert!(req.contains(
        "url = \"https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1\""
    ));
    f.assert_argv_clean();
    // curl failures are classified without output.
    f.set("usage-exit", "7");
    assert!(matches!(
        api(
            &f.client,
            &Secret::new(ACCESS),
            "GET",
            "/api/oauth/usage",
            None
        )
        .await,
        Err(SendError::NotSent(_))
    ));
    f.set("usage-exit", "28");
    assert!(matches!(
        api(
            &f.client,
            &Secret::new(ACCESS),
            "GET",
            "/api/oauth/usage",
            None
        )
        .await,
        Err(SendError::NoAnswer(_))
    ));
}
