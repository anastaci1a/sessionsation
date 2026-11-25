use super::*;
#[test]
fn yolo_starts_pass_the_yolo_approval_mode_and_default_passes_none() {
    let config = Config::default();
    let id = "11111111-1111-4111-8111-111111111111";
    assert_eq!(
        new_argv(&config, id, LaunchPermissions::Yolo),
        ["gemini", "--session-id", id, "--approval-mode", "yolo"]
    );
    assert_eq!(
        resume_argv(&config, id, LaunchPermissions::Yolo),
        ["gemini", "-r", id, "--approval-mode", "yolo"]
    );
    assert_eq!(
        new_argv(&config, id, LaunchPermissions::Default),
        ["gemini", "--session-id", id]
    );
    assert_eq!(
        resume_argv(&config, id, LaunchPermissions::Default),
        ["gemini", "-r", id]
    );
}
