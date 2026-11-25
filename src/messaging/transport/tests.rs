use super::*;
#[test]
fn full_read_budget_scales_with_the_frame_bound() {
    let base = Duration::from_secs(3);
    // An ordinary 1 MiB connection keeps essentially its base budget.
    assert!(frame_budget(base, FRAME_LIMIT) < base + Duration::from_millis(100));
    // A 64 MiB steer frame gets the time to move it at the budgeted floor.
    assert_eq!(
        frame_budget(base, STEER_FRAME_LIMIT),
        base + Duration::from_secs(4)
    );
    assert!(frame_budget(base, CHECKPOINT_FRAME_LIMIT) > base);
}
