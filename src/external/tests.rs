use super::*;
mod job_cache_tests;
mod job_records;
mod unit;

pub(super) fn copy_rollout(source: &Path, home: &Path, target_home: &Path) -> Result<()> {
    let relative = source
        // raw-path: test helper on a fixture home
        .strip_prefix(home)
        .context("rollout outside source home")?;
    ensure!(relative.starts_with("sessions"), "invalid rollout location");
    let target = target_home.join(relative);
    private_dir(target.parent().context("missing rollout parent")?)?;
    if target.exists() {
        ensure!(
            !fs::symlink_metadata(&target)?.file_type().is_symlink(),
            "destination rollout is a symlink"
        );
        let old = fs::read(&target)?;
        let current = fs::read(source)?;
        ensure!(
            current.starts_with(&old),
            "destination rollout diverged; refusing overwrite"
        );
        if current.len() > old.len() {
            atomic_write(&target, &current)?;
        }
        return Ok(());
    }
    let mut temp = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
    std::io::copy(&mut File::open(source)?, &mut temp)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(&target)?;
    File::open(target.parent().unwrap())?.sync_all()?;
    Ok(())
}
