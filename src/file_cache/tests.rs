use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn parse_counting<'a>(n: &'a AtomicUsize) -> impl FnOnce(&[u8]) -> Result<String, ()> + 'a {
    move |b| {
        n.fetch_add(1, Ordering::SeqCst);
        Ok(String::from_utf8_lossy(b).into_owned())
    }
}

#[test]
fn a_settled_file_is_parsed_once_and_a_rewrite_is_seen() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.json");
    let c = FileCache::<String>::new();
    let n = AtomicUsize::new(0);
    trust_fresh_stamps(true);
    fs::write(&p, "one").unwrap();
    for _ in 0..3 {
        assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "one");
    }
    assert_eq!(n.load(Ordering::SeqCst), 1);
    // Rewritten in place with a different size.
    fs::write(&p, "two!").unwrap();
    assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "two!");
    assert_eq!(n.load(Ordering::SeqCst), 2);
    // Replaced by rename (new inode).
    let tmp = d.path().join("tmp");
    fs::write(&tmp, "333").unwrap();
    fs::rename(&tmp, &p).unwrap();
    assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "333");
    assert_eq!(n.load(Ordering::SeqCst), 3);
    // And served from the cache again afterwards.
    assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "333");
    assert_eq!(n.load(Ordering::SeqCst), 3);
    trust_fresh_stamps(false);
}

#[test]
fn a_just_written_file_is_never_served_from_the_cache() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.json");
    let c = FileCache::<String>::new();
    let n = AtomicUsize::new(0);
    fs::write(&p, "one").unwrap();
    assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "one");
    fs::write(&p, "two").unwrap(); // same size, same second
    assert_eq!(*c.load(&p, parse_counting(&n)).unwrap(), "two");
    assert_eq!(n.load(Ordering::SeqCst), 2);
    assert!(c.is_empty());
}

#[test]
fn errors_are_not_cached_and_removed_files_leave_the_cache() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.json");
    let c = FileCache::<String>::new();
    assert!(matches!(
        c.load(&p, |_| Ok::<_, ()>(String::new())),
        Err(LoadError::Io(_))
    ));
    trust_fresh_stamps(true);
    fs::write(&p, "x").unwrap();
    assert!(matches!(
        c.load(&p, |_| Err::<String, _>(())),
        Err(LoadError::Parse(()))
    ));
    assert!(c.load(&p, |_| Ok::<_, ()>("x".into())).is_ok());
    assert_eq!(c.len(), 1);
    c.retain_dir(d.path(), &std::collections::HashSet::new());
    assert!(c.is_empty());
    trust_fresh_stamps(false);
}

thread_local! {
    static TEST_WINDOW: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}
pub(super) fn racy_seconds(default: i64) -> i64 {
    TEST_WINDOW.with(|w| w.get()).unwrap_or(default)
}
/// Tests cannot back-date a file's ctime: let this thread's cache trust every stamp
/// (`true`), or restore the real window (`false`).
pub(crate) fn trust_fresh_stamps(on: bool) {
    TEST_WINDOW.with(|w| w.set(on.then_some(i64::MIN / 2)));
}
impl<T> FileCache<T> {
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
