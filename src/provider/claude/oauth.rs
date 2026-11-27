//! Claude OAuth for the two limit-reset calls (contract 1.60.0).
//!
//! The only place sessionsation reads a Claude account's `.credentials.json`. The access
//! token is held in memory as a [`Secret`] and leaves the process only on curl's stdin
//! (`curl -q -K -`), never in argv, the environment, logs, journals, errors or results.
//!
//! Renewal follows Claude Code 2.1.314 exactly, so the two never fight over a refresh
//! token (a refresh rotates it; a lost rotation logs every session of the account out):
//!
//! - renew only when the token expires within [`EXPIRY_SKEW_MS`] (Claude Code's `KC`:
//!   `now + 300000 >= expiresAt`) or after a 401 for this token; never proactively;
//! - take Claude Code's refresh lock first: the proper-lockfile directory
//!   `<config dir>/.oauth_refresh.lock` (stale after 60 s, 5 attempts 1-2 s apart), then
//!   the legacy `<realpath(config dir)>.lock`;
//! - under the lock, re-read `.credentials.json`: when the access token changed, another
//!   process renewed it and that token is used (no second refresh);
//! - write back under `<config dir>/.storage-write.lock` (stale after 15 s) with Claude
//!   Code's compare-and-swap: when the stored refresh token is no longer the one posted,
//!   a sibling's newer write wins and its token is adopted. Every other field of the file
//!   is preserved; the file is replaced atomically with mode 0600.
//!
//! A revoked refresh token (`invalid_grant`) is `AUTH_REQUIRED` (log in again). A
//! contended lock or a transient failure is `AUTH_STALE`, retryable: a running Claude
//! session on the account renews the token itself, and the next attempt renews it here.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWriteExt;

/// Claude Code's `BASE_API_URL` (host `api` of its first-party client).
pub const API_BASE: &str = "https://api.anthropic.com";
/// Claude Code's `TOKEN_URL`.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code's plain usage read (`fetchUtilization`, read `plain`; contract 1.62.0). The
/// answer's windows (`five_hour`, `seven_day`, ...) are `{utilization, resets_at}`:
/// utilization is the percentage used (0-100, or null), resets_at an ISO 8601 time (or null).
pub const USAGE_PATH: &str = "/api/oauth/usage";
/// Claude Code's `CLIENT_ID` (used when the stored login names no `clientId`).
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// `anthropic-beta` sent with every OAuth bearer request.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Claude Code's first-party `User-Agent` (`sU()`), for the version these calls mirror.
pub const USER_AGENT: &str = "claude-cli/2.1.314 (external, cli)";
/// Claude Code's expiry margin: a token expiring within 5 minutes is renewed.
pub const EXPIRY_SKEW_MS: i64 = 300_000;
pub const CREDENTIALS_FILE: &str = ".credentials.json";
pub const REFRESH_LOCK: &str = ".oauth_refresh.lock";
pub const STORAGE_LOCK: &str = ".storage-write.lock";
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(60);
const STORAGE_LOCK_STALE: Duration = Duration::from_secs(15);
/// The default-client refresh scopes (`bMt()` with plugins registered).
const REFRESH_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
    "user:plugins",
];
/// Scopes kept from the stored login when present (`Jao`).
const KEPT_SCOPES: &[&str] = &["user:projects:read", "user:projects:write"];

/// A token. Never printed: `Debug` is redacted and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(redacted)")
    }
}
impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// `claudeAiOauth` of `.credentials.json` (only what the calls need).
#[derive(Clone, Debug)]
pub struct Login {
    pub access: Secret,
    pub refresh: Option<Secret>,
    pub expires_at_ms: Option<i64>,
    pub scopes: Vec<String>,
    pub client_id: Option<String>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
/// Claude Code's `KC(expiresAt)`: no expiry is never expired.
pub fn expired(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    expires_at_ms.is_some_and(|e| now_ms + EXPIRY_SKEW_MS >= e)
}

pub fn credentials_path(home: &Path) -> PathBuf {
    home.join(CREDENTIALS_FILE)
}
/// The stored login, or None when the file or its `claudeAiOauth.accessToken` is missing.
pub fn read_login(home: &Path) -> Result<Option<Login>> {
    let bytes = match fs::read(credentials_path(home)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("the Claude credentials file could not be read"),
    };
    let value: Value =
        serde_json::from_slice(&bytes).context("the Claude credentials file is not JSON")?;
    Ok(login_of(&value))
}
fn login_of(value: &Value) -> Option<Login> {
    let o = &value["claudeAiOauth"];
    let access = o["accessToken"].as_str().filter(|t| !t.is_empty())?;
    Some(Login {
        access: Secret::new(access),
        refresh: o["refreshToken"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(Secret::new),
        expires_at_ms: o["expiresAt"].as_i64(),
        scopes: o["scopes"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        client_id: o["clientId"]
            .as_str()
            .filter(|c| !c.is_empty())
            .map(str::to_owned),
    })
}
/// `oauthAccount.organizationUuid` of the account's `.claude.json` (not a secret).
pub fn organization_uuid(home: &Path) -> Option<String> {
    oauth_account_uuid(home, "organizationUuid")
}
/// `oauthAccount.accountUuid` of the same file (contract 1.73.6; only ever fingerprinted).
pub fn account_uuid(home: &Path) -> Option<String> {
    oauth_account_uuid(home, "accountUuid")
}
fn oauth_account_uuid(home: &Path, key: &str) -> Option<String> {
    let bytes = fs::read(home.join(".claude.json")).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v["oauthAccount"][key]
        .as_str()
        .filter(|u| uuid::Uuid::parse_str(u).is_ok())
        .map(str::to_owned)
}

// ---------------------------------------------------------------- transport

/// How a request is sent. Tests point `curl` at a fake and shorten the lock waits.
#[derive(Clone, Debug)]
pub struct Client {
    pub curl: PathBuf,
    pub api_base: String,
    pub token_url: String,
    pub max_time: Duration,
    /// Attempts and pause range for Claude Code's refresh lock (5, 1-2 s).
    pub lock_attempts: u32,
    pub lock_pause: (Duration, Duration),
}
impl Client {
    pub fn new(curl: impl Into<PathBuf>) -> Self {
        Self {
            curl: curl.into(),
            api_base: API_BASE.into(),
            token_url: TOKEN_URL.into(),
            max_time: Duration::from_secs(25),
            lock_attempts: 5,
            lock_pause: (Duration::from_millis(1000), Duration::from_millis(2000)),
        }
    }
}
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}
/// A request without an HTTP answer.
#[derive(Debug)]
pub enum SendError {
    /// curl never connected (DNS, refused, TLS handshake): nothing reached the server.
    NotSent(String),
    /// Timed out or failed after connecting: the server may have acted on it.
    NoAnswer(String),
}
impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(m) => write!(f, "not sent ({m})"),
            Self::NoAnswer(m) => write!(f, "no answer ({m})"),
        }
    }
}

/// A curl config line value: double-quoted with `\` and `"` escaped. Control
/// characters are refused (none occur in tokens, URLs or serialized JSON).
fn quoted(s: &str) -> std::result::Result<String, SendError> {
    if s.chars().any(char::is_control) {
        return Err(SendError::NotSent("refused a control character".into()));
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
fn config_text(
    client: &Client,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> std::result::Result<String, SendError> {
    let mut c = String::new();
    c.push_str("silent\nshow-error\n");
    c.push_str("proto = \"=https\"\n");
    c.push_str(&format!(
        "connect-timeout = {}\n",
        10.min(client.max_time.as_secs().max(1))
    ));
    c.push_str(&format!(
        "max-time = {}\n",
        client.max_time.as_secs().max(1)
    ));
    c.push_str(&format!("request = {}\n", quoted(method)?));
    c.push_str(&format!("url = {}\n", quoted(url)?));
    for (name, value) in headers {
        c.push_str(&format!(
            "header = {}\n",
            quoted(&format!("{name}: {value}"))?
        ));
    }
    if let Some(body) = body {
        c.push_str(&format!("data-raw = {}\n", quoted(body)?));
    }
    c.push_str("write-out = \"\\n%{http_code}\"\n");
    Ok(c)
}
/// One HTTP request through curl. The whole request (URL, headers with the bearer,
/// body) is curl's config on stdin; argv is only `-q -K -`.
pub async fn send(
    client: &Client,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> std::result::Result<Response, SendError> {
    let config = config_text(client, method, url, headers, body)?;
    let mut child = tokio::process::Command::new(&client.curl)
        .args(["-q", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| SendError::NotSent(format!("curl could not start: {}", e.kind())))?;
    if let Some(mut stdin) = child.stdin.take() {
        let written = stdin.write_all(config.as_bytes()).await;
        drop(stdin);
        if written.is_err() {
            return Err(SendError::NotSent("curl closed its input".into()));
        }
    }
    let waited = tokio::time::timeout(
        client.max_time + Duration::from_secs(5),
        child.wait_with_output(),
    )
    .await;
    let out = match waited {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(SendError::NoAnswer(format!("curl failed: {}", e.kind()))),
        Err(_) => return Err(SendError::NoAnswer("curl did not finish in time".into())),
    };
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        // 5 proxy, 6 host, 7 connect, 35 TLS connect: the request never left.
        return Err(if matches!(code, 2 | 5 | 6 | 7 | 35) {
            SendError::NotSent(format!("curl exit {code}"))
        } else {
            SendError::NoAnswer(format!("curl exit {code}"))
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text
        .rsplit_once('\n')
        .ok_or_else(|| SendError::NoAnswer("curl gave no status".into()))?;
    let status: u16 = status
        .trim()
        .parse()
        .map_err(|_| SendError::NoAnswer("curl gave no status".into()))?;
    if status == 0 {
        return Err(SendError::NoAnswer("no HTTP status".into()));
    }
    Ok(Response {
        status,
        body: body.to_owned(),
    })
}
/// An authenticated first-party API request (Claude Code's `Lt` with OAuth headers).
pub async fn api(
    client: &Client,
    token: &Secret,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> std::result::Result<Response, SendError> {
    let bearer = format!("Bearer {}", token.expose());
    let headers = [
        ("Authorization", bearer.as_str()),
        ("anthropic-beta", OAUTH_BETA),
        ("Content-Type", "application/json"),
        ("User-Agent", USER_AGENT),
    ];
    let body = body.map(Value::to_string);
    send(
        client,
        method,
        &format!("{}{path}", client.api_base),
        &headers,
        body.as_deref(),
    )
    .await
}

// ------------------------------------------------- proper-lockfile directory locks

/// A proper-lockfile lock: the directory exists while held; a holder keeps its mtime
/// fresh, so one older than `stale` belongs to a dead holder and is replaced.
pub struct DirLock {
    path: PathBuf,
}
impl DirLock {
    /// One attempt. Ok(None): held by someone else.
    pub fn try_acquire(path: &Path, stale: Duration) -> std::io::Result<Option<Self>> {
        for fresh_try in [false, true] {
            match fs::create_dir(path) {
                Ok(()) => {
                    return Ok(Some(Self {
                        path: path.to_path_buf(),
                    }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if fresh_try {
                        return Ok(None);
                    }
                    let modified = match fs::metadata(path).and_then(|m| m.modified()) {
                        Ok(m) => m,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e),
                    };
                    let age = SystemTime::now()
                        .duration_since(modified)
                        .unwrap_or_default();
                    if age < stale {
                        return Ok(None);
                    }
                    match fs::remove_dir(path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }
    /// Keep the lock fresh (proper-lockfile's `update`).
    pub fn touch(&self) {
        if let Ok(c) = std::ffi::CString::new(self.path.as_os_str().as_encoded_bytes()) {
            unsafe {
                libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0);
            }
        }
    }
}
impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}
fn pause(client: &Client) -> Duration {
    let (lo, hi) = client.lock_pause;
    let span = hi.saturating_sub(lo).as_millis() as u64;
    let jitter = if span == 0 {
        0
    } else {
        uuid::Uuid::new_v4().as_u128() as u64 % span
    };
    lo + Duration::from_millis(jitter)
}
/// The legacy lock beside the config dir: `<realpath(dir)>.lock`.
pub fn legacy_lock_path(home: &Path) -> PathBuf {
    // raw-path: Claude Code names this lock after Node's realpath of the config dir
    let real = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut s = real.into_os_string();
    s.push(".lock");
    PathBuf::from(s)
}
struct RefreshLock {
    _legacy: Option<DirLock>,
    primary: DirLock,
}
async fn acquire_refresh_lock(client: &Client, home: &Path) -> Result<Option<RefreshLock>> {
    let path = home.join(REFRESH_LOCK);
    for attempt in 1..=client.lock_attempts.max(1) {
        if let Some(primary) = DirLock::try_acquire(&path, REFRESH_LOCK_STALE)
            .context("Claude Code's refresh lock could not be taken")?
        {
            // Claude Code: a held legacy lock is contention; any other error is ignored.
            let legacy = match DirLock::try_acquire(&legacy_lock_path(home), REFRESH_LOCK_STALE) {
                Ok(Some(l)) => Some(l),
                Ok(None) => return Ok(None),
                Err(_) => None,
            };
            return Ok(Some(RefreshLock {
                _legacy: legacy,
                primary,
            }));
        }
        if attempt < client.lock_attempts {
            tokio::time::sleep(pause(client)).await;
        }
    }
    Ok(None)
}

// ------------------------------------------------------------------- renewal

pub fn stale(account: &str, why: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "AUTH_STALE: the Claude access token of {account} could not be renewed right now ({why}). A running Claude session on the account refreshes it itself, and the next attempt renews it under Claude Code's refresh lock; retry shortly"
    )
}
fn required(account: &str, why: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "AUTH_REQUIRED: {account} needs a new Claude login ({why}); run `sessionsation api account login {account}`"
    )
}

/// The stored access token when it is not due for renewal, else None. Never renews: the
/// supervisor's background usage reads use it, so only a caller's own request (or Claude
/// Code itself) ever rotates a refresh token (contract 1.62.0).
pub fn current_token(home: &Path) -> Option<Secret> {
    let login = read_login(home).ok().flatten()?;
    (!expired(login.expires_at_ms, now_ms())).then_some(login.access)
}

/// [`api`] with one renewal after a 401 for this token (Claude Code's `refreshOAuth`
/// retry). The renewal follows [`access_token`]'s rules.
pub async fn authed(
    client: &Client,
    home: &Path,
    account: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<std::result::Result<Response, SendError>> {
    let token = access_token(client, home, account, None).await?;
    let first = api(client, &token, method, path, body).await;
    if !matches!(&first, Ok(r) if r.status == 401) {
        return Ok(first);
    }
    let renewed = access_token(client, home, account, Some(&token)).await?;
    let second = api(client, &renewed, method, path, body).await;
    if matches!(&second, Ok(r) if r.status == 401) {
        return Err(stale(
            account,
            "the API refused the renewed token (HTTP 401)",
        ));
    }
    Ok(second)
}

/// A usable access token for the account. `failed` is a token the API just refused
/// (401): it is renewed even when not yet expired, unless another process already did.
pub async fn access_token(
    client: &Client,
    home: &Path,
    account: &str,
    failed: Option<&Secret>,
) -> Result<Secret> {
    let login = read_login(home)
        .map_err(|e| stale(account, &format!("{e}")))?
        .ok_or_else(|| required(account, "no Claude login is stored in the account home"))?;
    match failed {
        None if !expired(login.expires_at_ms, now_ms()) => return Ok(login.access),
        Some(f) if login.access != *f => return Ok(login.access),
        _ => {}
    }
    if login.refresh.is_none() {
        return Err(required(account, "the stored login has no refresh token"));
    }
    let entry = failed.cloned().unwrap_or(login.access);
    refresh(client, home, account, &entry, failed.is_some()).await
}

fn refresh_scopes(login: &Login) -> Vec<String> {
    if login.client_id.is_some() {
        return login.scopes.clone();
    }
    let mut s: Vec<String> = REFRESH_SCOPES.iter().map(|s| (*s).to_owned()).collect();
    for kept in KEPT_SCOPES {
        if login.scopes.iter().any(|x| x == kept) && !s.iter().any(|x| x == kept) {
            s.push((*kept).to_owned());
        }
    }
    s
}
fn oauth_error(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).ok()?["error"]
        .as_str()
        .map(str::to_owned)
}
/// After a failed refresh: another process may have renewed meanwhile.
fn renewed_meanwhile(home: &Path, entry: &Secret) -> Option<Secret> {
    read_login(home)
        .ok()
        .flatten()
        .filter(|l| l.access != *entry)
        .map(|l| l.access)
}

async fn refresh(
    client: &Client,
    home: &Path,
    account: &str,
    entry: &Secret,
    force: bool,
) -> Result<Secret> {
    let Some(lock) = acquire_refresh_lock(client, home)
        .await
        .map_err(|e| stale(account, &format!("{e}")))?
    else {
        return Err(stale(
            account,
            "another process holds Claude Code's refresh lock",
        ));
    };
    // Re-read under the lock (Claude Code's race rule).
    let login = read_login(home)
        .map_err(|e| stale(account, &format!("{e}")))?
        .ok_or_else(|| required(account, "no Claude login is stored in the account home"))?;
    if login.access != *entry {
        return Ok(login.access);
    }
    if !force && !expired(login.expires_at_ms, now_ms()) {
        return Ok(login.access);
    }
    let Some(posted) = login.refresh.clone() else {
        return Err(required(account, "the stored login has no refresh token"));
    };
    let path = credentials_path(home);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(stale(
            account,
            "the credentials file is a symlink; sessionsation does not write through it",
        ));
    }
    let client_id = login.client_id.clone().unwrap_or_else(|| CLIENT_ID.into());
    let scopes = refresh_scopes(&login);
    let mut answer = None;
    for (n, scopes) in [scopes, login.scopes.clone()].into_iter().enumerate() {
        lock.primary.touch();
        let body = json!({"grant_type":"refresh_token","refresh_token":posted.expose(),
            "client_id":client_id,"scope":scopes.join(" ")})
        .to_string();
        let sent = send(
            client,
            "POST",
            &client.token_url,
            &[
                ("Content-Type", "application/json"),
                ("User-Agent", USER_AGENT),
            ],
            Some(&body),
        )
        .await;
        lock.primary.touch();
        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                if let Some(t) = renewed_meanwhile(home, entry) {
                    return Ok(t);
                }
                return Err(stale(account, &format!("the token endpoint gave {e}")));
            }
        };
        let error = oauth_error(&response.body);
        // Claude Code's invalid-scope fallback: once, with the stored scopes.
        if n == 0
            && response.status == 400
            && error.as_deref() == Some("invalid_scope")
            && login.client_id.is_none()
            && login.scopes.iter().any(|s| s == "user:inference")
        {
            continue;
        }
        answer = Some((response, error));
        break;
    }
    let Some((response, error)) = answer else {
        return Err(stale(account, "the token endpoint refused the scopes"));
    };
    if response.status != 200 {
        if let Some(t) = renewed_meanwhile(home, entry) {
            return Ok(t);
        }
        if error.as_deref() == Some("invalid_grant") {
            return Err(required(
                account,
                "the refresh token was revoked or expired",
            ));
        }
        return Err(stale(
            account,
            &format!("the token endpoint answered HTTP {}", response.status),
        ));
    }
    let v: Value = serde_json::from_str(&response.body)
        .map_err(|_| stale(account, "the token endpoint answered unreadable JSON"))?;
    let (Some(access), Some(expires_in)) = (
        v["access_token"].as_str().filter(|t| !t.is_empty()),
        v["expires_in"].as_f64().filter(|s| s.is_finite()),
    ) else {
        return Err(stale(
            account,
            "the token endpoint answer has no access_token or expires_in",
        ));
    };
    let fresh = Fresh {
        access: Secret::new(access),
        refresh: v["refresh_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(Secret::new)
            .unwrap_or_else(|| posted.clone()),
        expires_at_ms: now_ms() + (expires_in * 1000.) as i64,
        scopes: v["scope"].as_str().map(|s| {
            s.split(' ')
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect()
        }),
    };
    lock.primary.touch();
    let adopted = save(home, &posted, &fresh)
        .await
        .map_err(|e| stale(account, &format!("{e:#}")))?;
    drop(lock);
    Ok(adopted.unwrap_or(fresh.access))
}

struct Fresh {
    access: Secret,
    refresh: Secret,
    expires_at_ms: i64,
    scopes: Option<Vec<String>>,
}

/// Claude Code's compare-and-swap save (`Teo`), under its storage write lock. Returns
/// the sibling's access token when a newer write already replaced the posted one.
async fn save(home: &Path, posted: &Secret, fresh: &Fresh) -> Result<Option<Secret>> {
    let lock_path = home.join(STORAGE_LOCK);
    let mut held = None;
    for attempt in 0..=10u64 {
        if let Some(l) = DirLock::try_acquire(&lock_path, STORAGE_LOCK_STALE)? {
            held = Some(l);
            break;
        }
        tokio::time::sleep(Duration::from_millis((100 * (attempt + 1)).min(1000))).await;
    }
    // The new refresh token exists only in memory now; losing it would log the
    // account out, so it is written even when the storage lock stayed busy.
    let _held = held;
    let path = credentials_path(home);
    let bytes = fs::read(&path).context("re-reading the credentials file failed")?;
    let mut file: Value =
        serde_json::from_slice(&bytes).context("the credentials file is not JSON")?;
    let stored = file["claudeAiOauth"]["refreshToken"].as_str().unwrap_or("");
    if !file["claudeAiOauth"].is_object() {
        bail!("the credentials file has no claudeAiOauth");
    }
    if !stored.is_empty() && stored != posted.expose() {
        return Ok(file["claudeAiOauth"]["accessToken"]
            .as_str()
            .map(Secret::new));
    }
    let o = &mut file["claudeAiOauth"];
    o["accessToken"] = json!(fresh.access.expose());
    o["refreshToken"] = json!(fresh.refresh.expose());
    o["expiresAt"] = json!(fresh.expires_at_ms);
    if let Some(scopes) = &fresh.scopes
        && !scopes.is_empty()
    {
        o["scopes"] = json!(scopes);
    }
    write_private(&path, &serde_json::to_vec(&file)?)?;
    Ok(None)
}
/// Atomic replace, mode 0600, same owner directory; never through a symlink.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("missing parent")?;
    let tmp = dir.join(format!(
        ".credentials.json.sessionsation-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let written = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        if let Ok(m) = fs::metadata(path) {
            // Keep the owner the file had (root-run tools write for their user).
            let _ = std::os::unix::fs::fchown(&f, Some(m.uid()), Some(m.gid()));
        }
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.context("writing the renewed credentials failed")
}

#[cfg(test)]
pub(crate) mod tests;
