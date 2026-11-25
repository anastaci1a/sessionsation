//! The owner's emergency switch for native sends: the file `messaging/native-send.disabled`.
use super::*;

pub(in crate::messaging) fn sending_enabled(paths: &Paths) -> Result<bool> {
    match fs::symlink_metadata(paths.root.join("messaging/native-send.disabled")) {
        Ok(_) => Ok(false), // Any marker, including a symlink, disables. Never follow it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(_) => Err(Fault("native_sending_disabled")),
    }
}
#[cfg(test)]
mod tests;
