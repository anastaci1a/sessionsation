use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, path::Path};

fn auth(home: &Path) -> Result<Value> {
    let path = home.join("auth.json");
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "unsafe credential file"
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn read_identity(home: &Path) -> Result<String> {
    let value = auth(home)?;
    let identity = value["tokens"]["account_id"]
        .as_str()
        .context("no ChatGPT account identity")?;
    ensure!(!identity.is_empty(), "empty account identity");
    Ok(identity.to_owned())
}
/// Contract 1.73.6: `account_id:user` from the same read, the user being the unverified
/// `id_token` payload's `https://api.openai.com/auth`.`chatgpt_user_id`, else `sub`; only
/// `account_id` (which can be a whole workspace) without one. Only ever fingerprinted.
pub fn upstream_account(home: &Path) -> Option<String> {
    let value = auth(home).ok()?;
    let account = value["tokens"]["account_id"]
        .as_str()
        .filter(|a| !a.is_empty())?;
    let claims = value["tokens"]["id_token"]
        .as_str()
        .and_then(|t| t.split('.').nth(1))
        .and_then(base64url)
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    let user = claims["https://api.openai.com/auth"]["chatgpt_user_id"]
        .as_str()
        .or(claims["sub"].as_str())
        .filter(|u| !u.is_empty());
    Some(user.map_or_else(|| account.to_owned(), |u| format!("{account}:{u}")))
}
fn base64url(text: &str) -> Option<Vec<u8>> {
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6 | u32::from(v)) & 0xFFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
