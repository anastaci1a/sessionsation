//! Direct child launches must survive atomic replacement of the installation.
//! Do not use this path in shell commands or persisted service definitions.
pub(crate) fn path() -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // exec resolves this in the forked child, which still runs our image.
        // current_exe() instead returns an unusable "... (deleted)" pathname
        // after an update. Keep the worker on the parent's exact code version.
        Ok(std::path::PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe()
}

#[cfg(test)]
mod tests;
