use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, path::Path};

pub fn read_identity(home: &Path) -> Result<String> {
    let path = home.join("auth.json");
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "unsafe credential file"
    );
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    let identity = value["tokens"]["account_id"]
        .as_str()
        .context("no ChatGPT account identity")?;
    ensure!(!identity.is_empty(), "empty account identity");
    Ok(identity.to_owned())
}
