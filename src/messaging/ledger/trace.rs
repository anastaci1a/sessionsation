//! Test-only statement recorder (rusqlite `trace`): every ledger connection of
//! the current thread records the SQL it runs.
use std::cell::RefCell;
thread_local! {
    static STATEMENTS: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}
fn record(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event {
        STATEMENTS.with(|s| {
            if let Some(v) = s.borrow_mut().as_mut() {
                v.push(sql.to_owned());
            }
        });
    }
}
pub(super) fn install(db: &rusqlite::Connection) {
    db.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(record),
    );
}
/// Run `f` and return the statements ledger connections ran on this thread.
pub(in crate::messaging) fn statements(f: impl FnOnce()) -> Vec<String> {
    STATEMENTS.with(|s| *s.borrow_mut() = Some(Vec::new()));
    f();
    STATEMENTS.with(|s| s.borrow_mut().take().unwrap_or_default())
}
