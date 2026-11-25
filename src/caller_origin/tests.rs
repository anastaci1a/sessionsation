use super::*;
mod catalog_tests;
mod codex_tests;
mod component_tests;
mod mode_tests;
mod unit;

/// Walk the parents of `start` (inclusive) and report whether one is an attachment.
pub(super) fn chain_hits(
    start: u32,
    attachments: &[(u32, u64)],
    stat: impl Fn(u32) -> Option<(u32, u64)>,
) -> bool {
    chain_match(start, attachments, stat).is_some()
}
