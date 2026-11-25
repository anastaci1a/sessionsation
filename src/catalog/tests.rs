mod identity_tests;
mod reservation_tests;
mod settled_tests;
mod space_move_tests;
mod stable_identity_tests;
mod unit;

thread_local! {
    pub(super) static SETTLED_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(super) fn settled_hit(hit: bool) {
    if hit {
        SETTLED_HITS.with(|h| h.set(h.get() + 1));
    }
}
