use super::*;

#[test]
fn what_is_left_in_the_input_is_reported_exactly() {
    let main = |s: &str| {
        Ok(View::Main {
            draft: s.into(),
            busy: false,
        })
    };
    assert_eq!(left_in_input(&main(""), "d", "m"), ("empty", Some(0)));
    assert_eq!(left_in_input(&main("d"), "d", "m"), ("draft", Some(1)));
    assert_eq!(
        left_in_input(&main("a b"), "d", "a\nb"),
        ("message", Some(3))
    );
    assert_eq!(left_in_input(&main("xy"), "d", "m"), ("other", Some(2)));
    assert_eq!(
        left_in_input(&Err("TERMINAL_VIEW_NOT_MAIN"), "d", "m"),
        ("unknown", None)
    );
}
