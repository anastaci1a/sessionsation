use super::super::*;

/// Session names across every Space of a fake catalog (names are bare in the catalog).
const NAMES: &[&str] = &["LEAD", "Résumé", "watcher"];

fn pick_in(
    names: Option<&[String]>,
    field: Option<&str>,
    env: Option<&str>,
) -> (String, Option<&'static str>, Vec<String>) {
    let mut warned = vec![];
    let c = choose_sender(None, names, field, env, |m| warned.push(m));
    assert_eq!(c.ignored, None);
    assert_eq!(c.sender.mode, "prompting");
    assert_eq!(c.sender.canonical, None);
    (c.sender.display_name().to_owned(), c.prefixed, warned)
}

fn pick(field: Option<&str>, env: Option<&str>) -> (String, Option<&'static str>, Vec<String>) {
    let names: Vec<String> = NAMES.iter().map(|&n| n.into()).collect();
    pick_in(Some(&names), field, env)
}

#[test]
fn the_request_field_beats_the_env_var() {
    assert_eq!(
        pick(Some("resource-guard"), Some("env-guard")),
        ("resource-guard".into(), None, vec![])
    );
    // A field given wins even over an invalid env var, which is then not consulted.
    assert_eq!(
        pick(Some("resource-guard"), Some("bad name")),
        ("resource-guard".into(), None, vec![])
    );
    assert_eq!(
        pick(None, Some("env-guard")),
        ("env-guard".into(), None, vec![])
    );
    assert_eq!(pick(None, None).0, "sessionsation");
    assert_eq!(pick(None, Some("")).0, "sessionsation");
}

#[test]
fn an_invalid_env_var_is_ignored_with_a_warning() {
    for bad in ["bad name", "-x", "a:b", &"a".repeat(41)] {
        let (name, prefixed, warned) = pick(None, Some(bad));
        assert_eq!((name.as_str(), prefixed), ("sessionsation", None));
        assert_eq!(warned.len(), 1);
        assert!(
            warned[0].starts_with("ignoring SESSIONSATION_SENDER_COMPONENT="),
            "{warned:?}"
        );
    }
}

#[test]
fn a_component_colliding_with_a_session_name_is_prefixed() {
    for (component, display) in [
        ("lead", "sessionsation:lead"),
        ("LEAD", "sessionsation:LEAD"),
        ("Watcher", "sessionsation:Watcher"),
        // Not a collision: only whole names count.
        ("lead-2", "lead-2"),
        ("watch", "watch"),
    ] {
        let collides = display.starts_with("sessionsation:");
        // From the request field: the fallback is reported as a fact, no warning.
        assert_eq!(
            pick(Some(component), None),
            (
                display.into(),
                collides.then_some(COMPONENT_PREFIXED_COLLISION),
                vec![]
            ),
            "{component}"
        );
        // From the env var: a stderr warning, no fact.
        let (name, prefixed, warned) = pick(None, Some(component));
        assert_eq!((name.as_str(), prefixed), (display, None));
        assert_eq!(warned.len(), usize::from(collides), "{warned:?}");
        if collides {
            assert!(warned[0].contains("collides"), "{warned:?}");
            assert!(warned[0].contains(&format!("\"{display}\"")), "{warned:?}");
        }
    }
}

#[test]
fn sessionsation_itself_is_prefixed() {
    for component in ["sessionsation", "SessionSation"] {
        assert_eq!(
            pick_in(Some(&[]), Some(component), None),
            (
                format!("sessionsation:{component}"),
                Some(COMPONENT_PREFIXED_COLLISION),
                vec![]
            )
        );
    }
    // An empty catalog renders anything else bare.
    assert_eq!(pick_in(Some(&[]), Some("lead"), None).0, "lead");
}

#[test]
fn an_unreadable_catalog_fails_safe_to_the_prefix() {
    assert_eq!(
        pick_in(None, Some("resource-guard"), None),
        (
            "sessionsation:resource-guard".into(),
            Some(COMPONENT_PREFIXED_COLLISION),
            vec![]
        )
    );
    let (name, _, warned) = pick_in(None, None, Some("resource-guard"));
    assert_eq!(name, "sessionsation:resource-guard");
    assert_eq!(warned.len(), 1);
}
