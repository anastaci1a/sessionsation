use super::*;

#[test]
fn only_a_valid_scale_changes_anything() {
    for bad in [
        None,
        Some(""),
        Some("0"),
        Some("1"),
        Some("x"),
        Some("-3"),
        Some("1001"),
    ] {
        assert_eq!(parse(bad), 1, "{bad:?}");
    }
    assert_eq!(parse(Some(" 10 ")), 10);
    assert_eq!(parse(Some("1000")), 1000);
}

#[test]
fn compression_has_floors_and_never_lengthens() {
    let ms = Duration::from_millis;
    let floor = ms(5);
    assert_eq!(
        compress(Duration::from_secs(5), 1, floor),
        Duration::from_secs(5)
    );
    assert_eq!(compress(Duration::from_secs(5), 10, floor), ms(500));
    assert_eq!(compress(ms(15), 10, floor), ms(5));
    assert_eq!(compress(ms(3), 10, floor), ms(3));
    let one = Duration::from_secs(1);
    assert_eq!(compress(Duration::from_secs(5), 10, one), one);
    assert_eq!(
        compress(Duration::from_secs(60), 10, one),
        Duration::from_secs(6)
    );
    assert_eq!(compress(ms(500), 10, one), ms(500));
}
