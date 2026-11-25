use super::*;
#[test]
fn launch_permissions_default_to_yolo_and_parse_both_modes() {
    assert_eq!(
        Config::default().launch_permissions,
        LaunchPermissions::Yolo
    );
    let legacy: Config = serde_json::from_str("{}").unwrap();
    assert_eq!(legacy.launch_permissions, LaunchPermissions::Yolo);
    let chosen: Config =
        serde_json::from_value(serde_json::json!({"launch_permissions":"default"})).unwrap();
    assert_eq!(chosen.launch_permissions, LaunchPermissions::Default);
    assert!(
        serde_json::from_value::<Config>(serde_json::json!({"launch_permissions":"ask"})).is_err()
    );
    assert_eq!(
        serde_json::to_value(Config::default()).unwrap()["launch_permissions"],
        "yolo"
    );
}
#[test]
fn reuse_idle_tabs_defaults_on_and_is_a_strict_boolean() {
    assert!(Config::default().reuse_idle_tabs);
    let legacy: Config = serde_json::from_str("{}").unwrap();
    assert!(legacy.reuse_idle_tabs);
    let off: Config = serde_json::from_value(serde_json::json!({"reuse_idle_tabs":false})).unwrap();
    assert!(!off.reuse_idle_tabs);
    assert!(serde_json::from_value::<Config>(serde_json::json!({"reuse_idle_tabs":"no"})).is_err());
}
