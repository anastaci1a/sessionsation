use std::{fs, process::Command};

#[test]
fn child_launch_survives_atomic_install_replacement() {
    const MARKER: &str = "SWITCHER_TEST_REPLACED_EXECUTABLE";
    if std::env::var_os(MARKER).is_some() {
        let installed = std::env::current_exe().unwrap();
        let replacement = installed.with_extension("replacement");
        fs::write(&replacement, b"not an executable").unwrap();
        fs::rename(replacement, &installed).unwrap();
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .arg("--list")
                .output()
                .is_err()
        );
        let child = Command::new(super::path().unwrap())
            .arg("--list")
            .output()
            .unwrap();
        assert!(child.status.success());
        assert!(
            String::from_utf8_lossy(&child.stdout)
                .contains("child_launch_survives_atomic_install_replacement")
        );
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let installed = fixture.path().join("switcher-test");
    fs::copy(std::env::current_exe().unwrap(), &installed).unwrap();
    let result = Command::new(installed)
        .args([
            "--exact",
            "self_executable::tests::child_launch_survives_atomic_install_replacement",
            "--nocapture",
        ])
        .env(MARKER, "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
