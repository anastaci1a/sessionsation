//! Enforcement harness for owned roles (centralization design §4, step S0).
//!
//! Test-only (`#[cfg(test)] mod guard;` in `lib.rs`). It scans production
//! sources under `src/` (test files, inline `#[cfg(test)]` items, comment lines
//! and this module itself excluded), counts every ratchet counter of
//! [`counters::COUNTERS`] plus the documentation counters, and compares them with
//! `guard/baseline.json`, whose signed ledger must balance against the pinned
//! `initial` values. Roles in [`roles::ROLES`] that are `Enforced` reject any
//! site outside their owners without an adjacent `// guard-allow(<role>): <why>`.
//! The documentation counters also have Enforced hard caps
//! ([`counters::CAP_INITIAL`] plus `cap:` ledger entries).
//! Method-call roles are enforced by clippy `disallowed-methods` (`clippy.toml`)
//! once they flip to `Enforced`.
//!
//! Run `cargo test --lib guard::measure -- --ignored --nocapture` to print the
//! current value of every counter.

mod counters;
mod roles;

use counters::{
    CAP, CAP_INITIAL, COUNTERS, Counter, DOC_BYTES, DOC_INITIAL, Kind, LEDGER_PREFIX_DIGEST,
    MODULE_INITIAL, MODULE_LINES, MODULES, SHRINK_MARKER_INDEX, TABLES_DIGEST, UNLISTED_OK,
};
use regex::Regex;
use roles::{Mech, Mode, ROLES, ROLES_DIGEST, Role};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------- scanner

/// One remaining source line (1-based `no`). `comment` lines (first non-blank
/// characters `//`) are skipped by every check except the marker checks.
#[derive(Debug)]
struct Line {
    no: usize,
    text: String,
    comment: bool,
    /// For a line naming `clippy::disallowed_methods`: the whole attribute,
    /// normalized to one line (rustfmt wraps long `allow(.., reason = ..)`).
    attr: Option<String>,
}

impl Line {
    /// The text marker checks read: the normalized attribute, else the line.
    fn marker_text(&self) -> &str {
        self.attr.as_deref().unwrap_or(&self.text)
    }
}

#[derive(Debug)]
struct Source {
    /// `src/...` with `/` separators.
    path: String,
    /// `wc -l` of the whole file.
    raw_lines: usize,
    /// Lines of the file's `}`-ended `#[cfg(test)]` items ([`Kind::InlineTestLines`]).
    inline_test_lines: usize,
    lines: Vec<Line>,
}

impl Source {
    fn line(&self, no: usize) -> Option<&Line> {
        self.lines
            .binary_search_by_key(&no, |l| l.no)
            .ok()
            .map(|i| &self.lines[i])
    }
}

fn manifest_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// §4.4 rule 1: test files, `/tests/` directories and the guard itself.
fn skipped(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name == "tests.rs"
        || rel.split('/').any(|c| c == "tests")
        || rel == "src/guard.rs"
        || rel.starts_with("src/guard/")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Every scanned source under `<root>/src`, sorted by path.
fn walk(root: &Path) -> Vec<Source> {
    let mut files = vec![];
    rust_files(&root.join("src"), &mut files);
    let mut out: Vec<Source> = files
        .into_iter()
        .filter_map(|f| {
            let rel = rel_path(root, &f);
            if skipped(&rel) {
                return None;
            }
            let text =
                std::fs::read_to_string(&f).unwrap_or_else(|e| panic!("guard: read {rel}: {e}"));
            Some(source(&rel, &text))
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn sources() -> &'static [Source] {
    static SOURCES: OnceLock<Vec<Source>> = OnceLock::new();
    SOURCES.get_or_init(|| walk(&manifest_root()))
}

fn source(rel: &str, text: &str) -> Source {
    let stripped = strip_tests(text);
    let mut lines: Vec<Line> = stripped
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| Line {
            no: i + 1,
            text: l.to_owned(),
            comment: l.trim_start().starts_with("//"),
            attr: None,
        })
        .collect();
    for i in 0..lines.len() {
        if !lines[i].comment && lines[i].text.contains("clippy::disallowed_methods") {
            lines[i].attr = Some(attribute_around(&lines, i));
        }
    }
    Source {
        path: rel.to_owned(),
        raw_lines: text.bytes().filter(|&b| b == b'\n').count(),
        inline_test_lines: inline_test_lines(text),
        lines,
    }
}

/// The attribute containing line `i`, joined over the lines rustfmt wraps it
/// into (from the `#[`/`#![` line to the line ending in `]`) and normalized to
/// the one-line spelling `allow(clippy::disallowed_methods, reason = "..")`.
fn attribute_around(lines: &[Line], i: usize) -> String {
    let opens = |l: &Line| {
        let t = l.text.trim_start();
        t.starts_with("#[") || t.starts_with("#![")
    };
    let mut from = i;
    while from > 0 && i - from < ATTR_SPAN && !opens(&lines[from]) {
        from -= 1;
    }
    if !opens(&lines[from]) {
        from = i;
    }
    let mut to = i;
    while to + 1 < lines.len() && to - i < ATTR_SPAN && !lines[to].text.trim_end().ends_with(']') {
        to += 1;
    }
    let joined = lines[from..=to]
        .iter()
        .map(|l| l.text.trim())
        .collect::<Vec<_>>()
        .join(" ");
    joined
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(",)", ")")
}

/// Lines an attribute may be wrapped over on either side of the lint name.
const ATTR_SPAN: usize = 6;

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn blank(out: &mut [u8], from: usize, to: usize) {
    for x in &mut out[from..to] {
        if *x != b'\n' {
            *x = b' ';
        }
    }
}

/// §4.4 rule 2: the text with every string, raw string, byte string, char and
/// byte-char literal and every comment replaced by spaces (newlines kept), so
/// brackets and attributes can be found. Lifetimes and labels stay code.
fn skeleton(text: &str) -> Vec<u8> {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = b[i..].iter().position(|&x| x == b'\n').map_or(n, |p| i + p);
            blank(&mut out, i, end);
            i = end;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            let mut j = i;
            while j < n {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            let j = j.min(n);
            blank(&mut out, i, j);
            i = j;
            continue;
        }
        let raw_prefix = match c {
            b'r' => Some(1),
            b'b' | b'c' if b.get(i + 1) == Some(&b'r') => Some(2),
            _ => None,
        };
        if let Some(skip) = raw_prefix
            && (i == 0 || !is_ident(b[i - 1]))
        {
            let mut j = i + skip;
            let mut hashes = 0;
            while j < n && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < n && b[j] == b'"' {
                let mut k = j + 1;
                let end = loop {
                    if k >= n {
                        break n;
                    }
                    if b[k] == b'"'
                        && b.get(k + 1..k + 1 + hashes)
                            .is_some_and(|h| h.iter().all(|&x| x == b'#'))
                    {
                        break k + 1 + hashes;
                    }
                    k += 1;
                };
                blank(&mut out, i, end);
                i = end;
                continue;
            }
        }
        if c == b'"' {
            let mut k = i + 1;
            while k < n && b[k] != b'"' {
                if b[k] == b'\\' {
                    k += 1;
                }
                k += 1;
            }
            let end = (k + 1).min(n);
            blank(&mut out, i, end);
            i = end;
            continue;
        }
        if c == b'\'' {
            if b.get(i + 1) == Some(&b'\\') {
                // Escaped char literal: skip the escaped character, then up to `'`.
                let mut k = i + 3;
                while k < n && b[k] != b'\'' && b[k] != b'\n' {
                    k += 1;
                }
                let end = (k + 1).min(n);
                blank(&mut out, i, end);
                i = end;
                continue;
            }
            if let Some(ch) = text[i + 1..].chars().next() {
                let len = ch.len_utf8();
                if ch != '\'' && b.get(i + 1 + len) == Some(&b'\'') {
                    blank(&mut out, i, i + 2 + len);
                    i += 2 + len;
                    continue;
                }
            }
            // A lifetime or a label.
            i += 1;
            continue;
        }
        i += 1;
    }
    out
}

fn is_test_cfg(attr: &str) -> bool {
    // `cfg(any(test, x))` is not stripped: that item also ships when `x` holds.
    attr == "#[cfg(test)]" || attr.starts_with("#[cfg(all(test,")
}

/// §4.4 rule 3: the end (exclusive) of the item starting at `from`: the first
/// `;` or `,` at bracket depth 0 (`,` ends a test-only match arm, field or
/// variant) or the `}` closing the first `{` opened at depth 0. A `,` inside
/// generics (`impl<A, B>`) ends the item early, which keeps more lines counted.
fn item_end(sk: &[u8], from: usize) -> usize {
    let mut depth = 0i32;
    for (k, &c) in sk.iter().enumerate().skip(from) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth < 0 {
                    return k;
                }
                if depth == 0 && c == b'}' {
                    return k + 1;
                }
            }
            b';' | b',' if depth == 0 => return k + 1,
            _ => {}
        }
    }
    sk.len()
}

/// The byte span (attribute start, item end) of every `#[cfg(test)]` /
/// `#[cfg(all(test, ..))]` item of the skeleton `sk`, outermost first.
fn test_items(sk: &[u8]) -> Vec<(usize, usize)> {
    let n = sk.len();
    let mut out = vec![];
    let mut i = 0;
    while i < n {
        if sk[i] != b'#' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && sk[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= n || sk[j] != b'[' {
            i += 1;
            continue;
        }
        let mut depth = 0;
        let mut k = j;
        while k < n {
            match sk[k] {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        if k >= n {
            break;
        }
        let attr: String = sk[i..=k]
            .iter()
            .filter(|c| !c.is_ascii_whitespace())
            .map(|&c| c as char)
            .collect();
        if is_test_cfg(&attr) {
            let end = item_end(sk, k + 1);
            out.push((i, end));
            i = end;
        } else {
            i = k + 1;
        }
    }
    out
}

/// The text with every `#[cfg(test)]` / `#[cfg(all(test, ..))]` item
/// (attribute included) replaced by spaces.
fn strip_tests(text: &str) -> String {
    let sk = skeleton(text);
    let mut out = text.as_bytes().to_vec();
    for (from, to) in test_items(&sk) {
        blank(&mut out, from, to);
    }
    String::from_utf8(out).expect("guard: blanking keeps UTF-8")
}

/// SHRINK §6.1: the lines (attribute line to closing line) of every test-only
/// item whose end is a `}` (a body: `mod x { }`, `fn`, `impl`, `struct { }`).
/// Items ending in `;` or `,` (`mod tests;`, `use`, a field or variant) are not
/// counted.
fn inline_test_lines(text: &str) -> usize {
    let sk = skeleton(text);
    test_items(&sk)
        .into_iter()
        .filter(|&(_, to)| to > 0 && sk[to - 1] == b'}')
        .map(|(from, to)| sk[from..to].iter().filter(|&&b| b == b'\n').count() + 1)
        .sum()
}

// ---------------------------------------------------------------- matching

/// Path prefix match; a `*` matches exactly one path segment.
fn path_matches(path: &str, pat: &str) -> bool {
    match pat.split_once('*') {
        Some((head, tail)) => {
            let Some(rest) = path.strip_prefix(head) else {
                return false;
            };
            match rest.find('/') {
                Some(k) => rest[k..].starts_with(tail),
                None => false,
            }
        }
        None => path.starts_with(pat),
    }
}

fn counter_applies(c: &Counter, path: &str) -> bool {
    (c.files.is_empty() || c.files.contains(&path))
        && c.scope.is_none_or(|s| path.starts_with(s))
        && !c.exclude.iter().any(|e| path_matches(path, e))
}

fn role_applies(r: &Role, path: &str) -> bool {
    r.scope.is_none_or(|s| path.starts_with(s)) && !r.owners.iter().any(|o| path_matches(path, o))
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("guard: bad regex {pattern}: {e}"))
}

fn role(name: &str) -> Option<&'static Role> {
    ROLES.iter().find(|r| r.name == name)
}

fn permitted_of(c: &Counter) -> Vec<Regex> {
    c.role
        .and_then(role)
        .map(|r| r.permitted.iter().map(|p| re(p)).collect())
        .unwrap_or_default()
}

fn blank_permitted<'t>(text: &'t str, permitted: &[Regex]) -> std::borrow::Cow<'t, str> {
    let mut t = std::borrow::Cow::Borrowed(text);
    for p in permitted {
        if p.is_match(&t) {
            t = std::borrow::Cow::Owned(p.replace_all(&t, "").into_owned());
        }
    }
    t
}

#[derive(Debug)]
struct Measured {
    value: usize,
    /// `path:line` per site (Sites/Markers) or the distinct values (Distinct).
    sites: Vec<String>,
}

fn measure_one(c: &Counter, sources: &[Source], docs: &[Doc]) -> Measured {
    let applicable = || sources.iter().filter(|s| counter_applies(c, &s.path));
    match c.kind {
        Kind::Lines => Measured {
            value: applicable().map(|s| s.raw_lines).sum(),
            sites: vec![],
        },
        Kind::InlineTestLines => Measured {
            value: applicable().map(|s| s.inline_test_lines).sum(),
            sites: vec![],
        },
        Kind::Sites | Kind::Markers => {
            let rx = re(c.regex);
            let permitted = permitted_of(c);
            let mut sites = vec![];
            for s in applicable() {
                for l in &s.lines {
                    if c.kind == Kind::Sites && l.comment {
                        continue;
                    }
                    if rx.is_match(&blank_permitted(
                        if c.kind == Kind::Markers {
                            l.marker_text()
                        } else {
                            &l.text
                        },
                        &permitted,
                    )) {
                        sites.push(format!("{}:{}", s.path, l.no));
                    }
                }
            }
            Measured {
                value: sites.len(),
                sites,
            }
        }
        Kind::Distinct => {
            let rx = re(c.regex);
            let mut seen = BTreeSet::new();
            for s in applicable() {
                for l in s.lines.iter().filter(|l| !l.comment) {
                    for m in rx.find_iter(&l.text) {
                        seen.insert(m.as_str().replace([':', '"'], ""));
                    }
                }
            }
            Measured {
                value: seen.len(),
                sites: seen.into_iter().collect(),
            }
        }
        Kind::DocBytesTotal => Measured {
            value: docs.iter().map(|d| d.bytes).sum(),
            sites: vec![],
        },
        Kind::DocDuplicateParagraphs => {
            let dups = duplicate_paragraphs(docs);
            Measured {
                value: dups.iter().map(|(_, files)| files.len() - 1).sum(),
                sites: dups
                    .iter()
                    .map(|(p, files)| format!("{}: {}", files.join(", "), first_chars(p, 60)))
                    .collect(),
            }
        }
    }
}

/// `file` is `<module>.rs` or lies under `<module>/`.
fn in_module(file: &str, module: &str) -> bool {
    file.strip_prefix(module)
        .is_some_and(|rest| rest == ".rs" || rest.starts_with('/'))
}

/// Every counter by name, including one `doc_bytes:<path>` per doc file and one
/// `module_lines:<path>` per module of [`MODULES`].
fn measure_all(sources: &[Source], docs: &[Doc]) -> BTreeMap<String, Measured> {
    let mut out: BTreeMap<String, Measured> = COUNTERS
        .iter()
        .map(|c| (c.name.to_owned(), measure_one(c, sources, docs)))
        .collect();
    for (path, _) in MODULES {
        out.insert(
            format!("{MODULE_LINES}{path}"),
            Measured {
                value: sources
                    .iter()
                    .filter(|s| in_module(&s.path, path))
                    .map(|s| s.raw_lines)
                    .sum(),
                sites: vec![],
            },
        );
    }
    for d in docs {
        out.insert(
            format!("{DOC_BYTES}{}", d.path),
            Measured {
                value: d.bytes,
                sites: vec![],
            },
        );
    }
    out
}

fn initial_of(name: &str) -> Option<usize> {
    if let Some(path) = name.strip_prefix(DOC_BYTES) {
        return Some(
            DOC_INITIAL
                .iter()
                .find(|(p, _)| *p == path)
                .map_or(0, |(_, v)| *v),
        );
    }
    if let Some(path) = name.strip_prefix(MODULE_LINES) {
        return MODULES.iter().any(|(p, _)| *p == path).then(|| {
            MODULE_INITIAL
                .iter()
                .find(|(p, _)| *p == path)
                .map_or(0, |(_, v)| *v)
        });
    }
    COUNTERS.iter().find(|c| c.name == name).map(|c| c.initial)
}

/// Counters under an Enforced hard cap: `src_lines` (SHRINK §6.2, falls-only)
/// and every documentation counter.
fn capped(name: &str) -> bool {
    matches!(name, "src_lines" | "inline_test_lines")
        || name.starts_with(DOC_BYTES)
        || COUNTERS.iter().any(|c| {
            c.name == name && matches!(c.kind, Kind::DocBytesTotal | Kind::DocDuplicateParagraphs)
        })
}

/// Pinned cap of `name` (`CAP_INITIAL`; 0 for a doc file not listed there).
fn cap_initial_of(name: &str) -> Option<usize> {
    if let Some((_, v)) = CAP_INITIAL.iter().find(|(n, _)| *n == name) {
        return Some(*v);
    }
    name.starts_with(DOC_BYTES).then_some(0)
}

/// `doc_caps_hold` failures: a capped counter above its cap, or without one.
fn cap_violations(actual: &BTreeMap<String, Measured>, base: &Baseline) -> Vec<String> {
    let mut out = vec![];
    for (name, m) in actual.iter().filter(|(n, _)| capped(n)) {
        let Some(cap) = base.caps.get(name) else {
            out.push(format!(
                "guard: counter {name} is {} and has no cap (Enforced)\n\
                 add guard/baseline.json \"caps\" {{\"{name}\": {{\"value\": N}}}} and a ledger entry \
                 {{counter: \"{CAP}{name}\", by: +N, reason, considered, step}}",
                m.value
            ));
            continue;
        };
        if m.value as i64 > cap.value {
            let mut msg = format!(
                "guard: counter {name} is {}, above its cap {} (Enforced)",
                m.value, cap.value
            );
            for s in &m.sites {
                msg.push_str(&format!("\n  {s}"));
            }
            msg.push_str(&format!(
                "\ncut the text, or raise the cap: a ledger entry {{counter: \"{CAP}{name}\", by: {:+}, \
                 reason, considered (what was cut instead), step}} and \"caps\" value = {}",
                m.value as i64 - cap.value,
                m.value
            ));
            out.push(msg);
        }
    }
    out
}

// ---------------------------------------------------------------- docs

#[derive(Debug)]
struct Doc {
    path: String,
    bytes: usize,
    text: String,
}

/// Directories never walked for documentation: Git metadata, build trees and
/// vendored crate registries.
fn skip_doc_dir(name: &str) -> bool {
    name == ".git"
        || name.starts_with("target")
        || name == "registry"
        || name.starts_with("cargo-registry")
        || name == "node_modules"
}

fn md_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if !skip_doc_dir(&name) {
                md_files(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

/// Git-tracked `*.md` paths, or `None` when Git cannot answer (for example a
/// worktree checked inside the Docker image, whose `.git` points outside the mount).
fn tracked_md(root: &Path) -> Option<BTreeSet<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.md"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect(),
    )
}

/// Root-level files and files under dot-directories are where untracked
/// scratch notes land.
fn scratch_location(rel: &str) -> bool {
    !rel.contains('/') || rel.split('/').any(|c| c.starts_with('.'))
}

/// Whether a found doc file is project documentation. Files with a counter
/// (`known`) always count. Otherwise Git decides (tracked files only); without
/// Git, every file counts except untracked-looking scratch at the root or in
/// dot-directories.
fn doc_counts(rel: &str, known: &BTreeSet<String>, tracked: Option<&BTreeSet<String>>) -> bool {
    known.contains(rel)
        || match tracked {
            Some(t) => t.contains(rel),
            None => !scratch_location(rel),
        }
}

/// Project-owned Markdown: every `*.md` in the repository outside `.git`,
/// `target*` and vendored registries, filtered by [`doc_counts`].
fn docs_in(root: &Path, known: &BTreeSet<String>, tracked: Option<&BTreeSet<String>>) -> Vec<Doc> {
    let mut files = vec![];
    md_files(root, &mut files);
    let mut out: Vec<Doc> = files
        .into_iter()
        .filter_map(|f| {
            let path = rel_path(root, &f);
            if !doc_counts(&path, known, tracked) {
                return None;
            }
            let bytes =
                std::fs::read(&f).unwrap_or_else(|e| panic!("guard: read {}: {e}", f.display()));
            Some(Doc {
                path,
                bytes: bytes.len(),
                text: String::from_utf8_lossy(&bytes).into_owned(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Doc paths that have a counter: `DOC_INITIAL` and the baseline's
/// `doc_bytes:<path>` entries.
fn known_docs() -> BTreeSet<String> {
    let mut known: BTreeSet<String> = DOC_INITIAL.iter().map(|(p, _)| (*p).to_owned()).collect();
    if let Ok(base) = load_baseline(&manifest_root()) {
        known.extend(
            base.counters
                .keys()
                .filter_map(|k| k.strip_prefix(DOC_BYTES))
                .map(str::to_owned),
        );
    }
    known
}

fn docs() -> &'static [Doc] {
    static DOCS: OnceLock<Vec<Doc>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let root = manifest_root();
        docs_in(&root, &known_docs(), tracked_md(&root).as_ref())
    })
}

const MIN_PARAGRAPH: usize = 160;

/// Collapse whitespace, lowercase, drop emphasis markers and backticks.
fn normalize_paragraph(p: &str) -> String {
    let s: String = p
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`'))
        .collect();
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn paragraphs(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur: Vec<&str> = vec![];
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(normalize_paragraph(&cur.join("\n")));
                cur.clear();
            }
        } else {
            cur.push(line);
        }
    }
    out.retain(|p| p.chars().count() >= MIN_PARAGRAPH);
    out
}

/// Normalized paragraphs found in more than one doc file, with those files;
/// most widely shared (then longest) first.
fn duplicate_paragraphs(docs: &[Doc]) -> Vec<(String, Vec<String>)> {
    let mut files_of: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for d in docs {
        for p in paragraphs(&d.text) {
            files_of.entry(p).or_default().insert(d.path.clone());
        }
    }
    let mut out: Vec<(String, Vec<String>)> = files_of
        .into_iter()
        .filter(|(_, f)| f.len() > 1)
        .map(|(p, f)| (p, f.into_iter().collect()))
        .collect();
    out.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then(b.0.len().cmp(&a.0.len()))
            .then(a.0.cmp(&b.0))
    });
    out
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------- baseline

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    roles_digest: String,
    /// sha256 (hex) of the whole of src/guard/counters.rs.
    counters_digest: String,
    counters: BTreeMap<String, BaselineCounter>,
    /// Enforced hard caps (`counters::CAP_INITIAL` + the `cap:` ledger entries).
    caps: BTreeMap<String, BaselineCounter>,
    ledger: Vec<Entry>,
    enforced: BTreeMap<String, String>,
    /// Shrink program: step label -> release label (SHRINK §6.2).
    steps: BTreeMap<String, String>,
    /// Shrink program: counter ceilings per release (SHRINK §6.3).
    trajectory: Vec<Trajectory>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trajectory {
    counter: String,
    /// Date the release is due (informational).
    by: String,
    max: i64,
    /// Release label, resolved from the step through `steps`.
    step: String,
}

/// Who agreed to a rise of a falls-only counter.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Who {
    Operator,
    Lead,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Agreed {
    who: Who,
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineCounter {
    value: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    counter: String,
    by: i64,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    considered: String,
    #[serde(default)]
    step: String,
    /// `"shrink"` for the entries of the shrink program (SHRINK §6.2).
    #[serde(default)]
    program: Option<String>,
    /// Required for a rise of a falls-only counter after the shrink marker.
    #[serde(default)]
    agreed_by: Option<Agreed>,
}

fn load_baseline(root: &Path) -> Result<Baseline, String> {
    let path = root.join("guard/baseline.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("guard: {} is missing or unreadable: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("guard: {} is malformed: {e}", path.display()))
}

fn baseline() -> Baseline {
    load_baseline(&manifest_root()).unwrap_or_else(|e| panic!("{e}"))
}

/// `counters_match_baseline` failures for these measurements.
fn counter_mismatches(actual: &BTreeMap<String, Measured>, base: &Baseline) -> Vec<String> {
    let mut out = vec![];
    for (name, m) in actual {
        let Some(b) = base.counters.get(name) else {
            out.push(format!(
                "guard: counter {name} is {}, but guard/baseline.json has no such counter\n\
                 add it with value = {} and a ledger entry {{counter, by: +{}, reason, considered, step}}",
                m.value, m.value, m.value
            ));
            continue;
        };
        if m.value as i64 != b.value {
            let mut msg = format!(
                "guard: counter {name} is {}, baseline value {}; sites:",
                m.value, b.value
            );
            for s in &m.sites {
                msg.push_str(&format!("\n  {s}"));
            }
            msg.push_str(&format!(
                "\nadd a ledger entry {{counter, by: {:+}, reason, considered, step}} and set value = {}, or remove the site",
                m.value as i64 - b.value,
                m.value
            ));
            out.push(msg);
        }
    }
    for (name, b) in &base.counters {
        if actual.contains_key(name) {
            continue;
        }
        if name.starts_with(DOC_BYTES) {
            // A removed doc file measures 0.
            if b.value != 0 {
                out.push(format!(
                    "guard: counter {name} is 0 (file removed), baseline value {}\n\
                     add a ledger entry {{counter, by: {:+}, reason, considered, step}} and set value = 0",
                    b.value, -b.value
                ));
            }
        } else {
            out.push(format!(
                "guard: guard/baseline.json lists counter {name}, which the guard does not define"
            ));
        }
    }
    out
}

/// `ledger_balances` failures.
fn ledger_problems(base: &Baseline) -> Vec<String> {
    let mut out = vec![];
    for (i, e) in base.ledger.iter().enumerate() {
        let at = format!("guard: ledger entry {i} ({})", e.counter);
        let cap = e.counter.strip_prefix(CAP);
        match cap {
            Some(name) if !base.caps.contains_key(name) => {
                out.push(format!("{at} names a cap missing from \"caps\""));
            }
            None if !base.counters.contains_key(&e.counter) => {
                out.push(format!("{at} names a counter missing from \"counters\""));
            }
            _ => {}
        }
        if e.by == 0 && !is_shrink_marker(e) {
            out.push(format!("{at} has by: 0"));
        }
        if e.reason.trim().is_empty() {
            out.push(format!("{at} has no reason"));
        }
        if e.step.trim().is_empty() {
            out.push(format!("{at} has no step"));
        }
        let considered = e.considered.trim();
        let unconsidered = considered.is_empty() || considered == "n/a";
        if e.by > 0 && cap.is_some() && unconsidered {
            out.push(format!(
                "{at} raises the cap (by: {:+}) without \"considered\" (what was cut instead)",
                e.by
            ));
        } else if e.by > 0 && cap.is_none() && !size_counter(&e.counter) && unconsidered {
            out.push(format!(
                "{at} raises the counter (by: {:+}) without \"considered\" (what was tried instead)",
                e.by
            ));
        }
    }
    for (name, c) in &base.caps {
        let Some(initial) = cap_initial_of(name) else {
            out.push(format!(
                "guard: cap {name} has no pinned initial in src/guard/counters.rs CAP_INITIAL"
            ));
            continue;
        };
        let key = format!("{CAP}{name}");
        let sum: i64 = base
            .ledger
            .iter()
            .filter(|e| e.counter == key)
            .map(|e| e.by)
            .sum();
        if c.value != initial as i64 + sum {
            out.push(format!(
                "guard: cap {name} value {} != initial {initial} + ledger {sum}",
                c.value
            ));
        }
    }
    for name in base.counters.keys().filter(|n| capped(n)) {
        if !base.caps.contains_key(name) {
            out.push(format!(
                "guard: counter {name} has no cap in guard/baseline.json \"caps\""
            ));
        }
    }
    for (name, b) in &base.counters {
        let Some(initial) = initial_of(name) else {
            out.push(format!(
                "guard: counter {name} has no pinned initial in src/guard/counters.rs"
            ));
            continue;
        };
        let sum: i64 = base
            .ledger
            .iter()
            .filter(|e| &e.counter == name)
            .map(|e| e.by)
            .sum();
        if b.value != initial as i64 + sum {
            out.push(format!(
                "guard: counter {name} value {} != initial {initial} + ledger {sum}",
                b.value
            ));
        }
    }
    for c in COUNTERS {
        if !base.counters.contains_key(c.name) {
            out.push(format!(
                "guard: counter {} is missing from guard/baseline.json",
                c.name
            ));
        }
    }
    out.extend(shrink_problems(base));
    out
}

/// The marker entry that opens the falls-only rule: `src_lines`, `by: 0`, step
/// `shrink-S0`, program `shrink`.
fn is_shrink_marker(e: &Entry) -> bool {
    e.counter == "src_lines"
        && e.by == 0
        && e.step == "shrink-S0"
        && e.program.as_deref() == Some("shrink")
}

/// Counters whose rise needs `agreed_by` after the marker; a module counter only
/// while it already has lines (`prior > 0`: creating a listed module is exempt).
fn falls_only(counter: &str, prior: i64) -> bool {
    matches!(counter, "src_lines" | "cap:src_lines" | "inline_test_lines")
        || (counter.starts_with(MODULE_LINES)
            && (prior > 0 || initial_of(counter).is_some_and(|i| i > 0)))
}

/// SHRINK §6.2 failures: the marker (pinned index), `agreed_by` on every rise of a
/// falls-only counter after it, one `src_lines` entry per program step, program
/// steps listed in `steps`, and the budget cap of a newly created module.
fn shrink_problems(base: &Baseline) -> Vec<String> {
    let mut out = vec![];
    let markers: Vec<usize> = (0..base.ledger.len())
        .filter(|&i| is_shrink_marker(&base.ledger[i]))
        .collect();
    if markers != [SHRINK_MARKER_INDEX] {
        out.push(format!(
            "guard: the shrink marker entry (src_lines, by 0, step shrink-S0, program shrink) must be the single one, at ledger index {SHRINK_MARKER_INDEX}; found at {markers:?}"
        ));
    }
    let mut running: BTreeMap<&str, i64> = BTreeMap::new();
    let mut per_step: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, e) in base.ledger.iter().enumerate() {
        let prior = *running
            .entry(e.counter.as_str())
            .or_insert_with(|| initial_of(&e.counter).unwrap_or(0) as i64);
        *running.get_mut(e.counter.as_str()).expect("inserted") += e.by;
        let at = format!("guard: ledger entry {i} ({})", e.counter);
        if e.program.as_deref() == Some("shrink") {
            if !base.steps.contains_key(&e.step) {
                out.push(format!(
                    "{at}: step {:?} is not in guard/baseline.json \"steps\"",
                    e.step
                ));
            }
            if e.counter == "src_lines" {
                let n = per_step.entry(e.step.as_str()).or_insert(0);
                *n += 1;
                if *n > 1 {
                    out.push(format!(
                        "{at}: step {} has more than one src_lines entry (one squash commit, one line)",
                        e.step
                    ));
                }
            }
        }
        if !markers.is_empty() && i <= markers[0] {
            continue;
        }
        if e.by > 0 && falls_only(&e.counter, prior) {
            match &e.agreed_by {
                None => out.push(format!(
                    "{at} raises a falls-only counter (by: {:+}) without agreed_by {{who: operator|lead, ref}}",
                    e.by
                )),
                Some(a) if !ref_looks_valid(&a.reference) => {
                    out.push(format!(
                        "{at} has an agreed_by ({:?}) whose ref is not a message id or URL",
                        a.who
                    ));
                }
                Some(_) => {}
            }
        }
    }
    out
}

/// `agreed_by.ref`: a URL, or a message id (at least six id characters, no spaces).
fn ref_looks_valid(r: &str) -> bool {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| re(r"^(https?://\S+|[A-Za-z0-9][A-Za-z0-9._:/-]{5,})$"))
        .is_match(r)
}

/// sha256 of the ledger entries up to and including the shrink marker.
fn ledger_prefix_digest(base: &Baseline) -> String {
    let mut s = String::new();
    for e in base.ledger.iter().take(SHRINK_MARKER_INDEX + 1) {
        s.push_str(&format!(
            "{}|{}|{}|{}|{}|{:?}\n",
            e.counter, e.by, e.reason, e.considered, e.step, e.program
        ));
    }
    sha256_hex(s.as_bytes())
}
/// sha256 of the `steps` map and the `trajectory` rows.
fn tables_digest(base: &Baseline) -> String {
    let mut s = String::new();
    for (k, v) in &base.steps {
        s.push_str(&format!("step|{k}|{v}\n"));
    }
    for r in &base.trajectory {
        s.push_str(&format!(
            "row|{}|{}|{}|{}\n",
            r.counter, r.by, r.max, r.step
        ));
    }
    sha256_hex(s.as_bytes())
}
/// Pins: the ledger before the marker, the steps and the trajectory (a reviewed change of
/// `counters.rs`, itself pinned by `counters_digest`); trajectory rows must fall strictly
/// along their dates.
fn ratchet_pin_problems(base: &Baseline) -> Vec<String> {
    let mut out = vec![];
    if ledger_prefix_digest(base) != LEDGER_PREFIX_DIGEST {
        out.push(
            "guard: ledger entries up to the shrink marker changed; they are never edited"
                .to_owned(),
        );
    }
    if tables_digest(base) != TABLES_DIGEST {
        out.push("guard: guard/baseline.json steps or trajectory changed; update TABLES_DIGEST in src/guard/counters.rs (reviewed change)".to_owned());
    }
    let mut last: BTreeMap<&str, (&str, i64)> = BTreeMap::new();
    for r in &base.trajectory {
        if let Some((by, max)) = last.insert(&r.counter, (&r.by, r.max))
            && (r.by.as_str() <= by || r.max >= max)
            && !(r.max == 0 && max == 0)
        {
            out.push(format!(
                "guard: trajectory row {} {} {} does not fall strictly after {by} {max}",
                r.counter, r.by, r.max
            ));
        }
    }
    out
}

/// Every production file belongs to a listed module or to an allowlisted existing one.
fn unlisted_problems(sources: &[Source]) -> Vec<String> {
    sources
        .iter()
        .filter(|s| {
            !MODULES.iter().any(|(m, _)| in_module(&s.path, m))
                && !UNLISTED_OK.iter().any(|m| in_module(&s.path, m))
        })
        .map(|s| {
            format!(
                "guard: {} is in no module of counters::MODULES and not in UNLISTED_OK; list it (a budgeted module) or move it",
                s.path
            )
        })
        .collect()
}

/// Budget cap of the exemption: a listed module created by the program (initial
/// 0) may not exceed its budget.
fn module_budget_problems(actual: &BTreeMap<String, Measured>, base: &Baseline) -> Vec<String> {
    let mut out = vec![];
    for (path, budget) in MODULES {
        let name = format!("{MODULE_LINES}{path}");
        // New (initial 0) or recreated (its ledger value reached 0 once).
        let mut v = initial_of(&name).unwrap_or(0) as i64;
        let mut zero = v == 0;
        for e in base.ledger.iter().filter(|e| e.counter == name) {
            v += e.by;
            zero |= v == 0;
        }
        let new = zero;
        if let Some(m) = actual.get(&name)
            && new
            && m.value > *budget
        {
            out.push(format!(
                "guard: new module {path} has {} lines, above its budget {budget}",
                m.value
            ));
        }
    }
    out
}

/// SHRINK §6.3, release gate only: the measured counters against the trajectory
/// rows of the release named by `step` (`SESSIONSATION_RELEASE_STEP`). Unset or
/// `hotfix:*` skips; an unmapped step or a release without a row fails.
fn trajectory_problems(
    base: &Baseline,
    actual: &BTreeMap<String, Measured>,
    step: Option<&str>,
    contract: (&str, u32),
) -> Vec<String> {
    let Some(step) = step.map(str::trim).filter(|s| !s.is_empty()) else {
        return vec![];
    };
    if step.starts_with("hotfix:") {
        return if contract.1 > 0 {
            vec![]
        } else {
            vec![format!(
                "guard: a hotfix label needs a patch release; the contract is {}",
                contract.0
            )]
        };
    }
    let Some(release) = base
        .steps
        .get(step)
        .or_else(|| base.steps.values().find(|v| v.as_str() == step))
    else {
        return vec![format!(
            "guard: release step {step:?} is not in guard/baseline.json \"steps\""
        )];
    };
    if release != contract.0 {
        return vec![format!(
            "guard: step {step} is release {release}, but this build is contract {}",
            contract.0
        )];
    }
    let rows: Vec<&Trajectory> = base
        .trajectory
        .iter()
        .filter(|r| &r.step == release)
        .collect();
    if rows.is_empty() {
        return vec![format!(
            "guard: release {release} (step {step}) has no trajectory row in guard/baseline.json"
        )];
    }
    let mut out = vec![];
    for r in rows {
        match actual.get(&r.counter) {
            None => out.push(format!(
                "guard: trajectory row for {release} names unknown counter {}",
                r.counter
            )),
            Some(m) if m.value as i64 > r.max => out.push(format!(
                "guard: release {release} (step {step}): {} is {}, above the trajectory ceiling {} (due {})",
                r.counter, m.value, r.max, r.by
            )),
            Some(_) => {}
        }
    }
    out
}

// ---------------------------------------------------------------- roles

fn has_marker(text: &str, role: &str) -> bool {
    text.contains(&format!("guard-allow({role})"))
        || text.contains(&format!("disallowed_methods, reason = \"{role}:"))
}

/// A marker for `role` on the line itself or on the line directly above.
fn marked(s: &Source, l: &Line, role: &str) -> bool {
    has_marker(l.marker_text(), role)
        || s.line(l.no.wrapping_sub(1))
            .is_some_and(|a| has_marker(a.marker_text(), role))
}

fn owners_text(r: &Role) -> String {
    if r.owners.is_empty() {
        "(its owner module, not yet created)".to_owned()
    } else {
        r.owners.join(", ")
    }
}

/// Lines that are sites of `r`: its forbidden regexes (Regex roles) outside its
/// owners and scope, and the sites of its `Sites` counters.
fn role_sites<'a>(r: &Role, sources: &'a [Source]) -> Vec<(&'a Source, &'a Line)> {
    let permitted: Vec<Regex> = r.permitted.iter().map(|p| re(p)).collect();
    let forbidden: Vec<Regex> = match r.mech {
        Mech::Regex => r.forbidden.iter().map(|p| re(p)).collect(),
        Mech::Clippy => vec![],
    };
    let counters: Vec<(&Counter, Regex)> = COUNTERS
        .iter()
        .filter(|c| c.role == Some(r.name) && c.kind == Kind::Sites)
        .map(|c| (c, re(c.regex)))
        .collect();
    let mut out = vec![];
    for s in sources {
        let by_role = role_applies(r, &s.path);
        for l in s.lines.iter().filter(|l| !l.comment) {
            let text = blank_permitted(&l.text, &permitted);
            let hit = (by_role && forbidden.iter().any(|f| f.is_match(&text)))
                || counters
                    .iter()
                    .any(|(c, rx)| counter_applies(c, &s.path) && rx.is_match(&text));
            if hit {
                out.push((s, l));
            }
        }
    }
    out
}

/// `roles_have_one_owner` failures for one role.
fn role_violations(r: &Role, sources: &[Source]) -> Vec<String> {
    role_sites(r, sources)
        .into_iter()
        .filter(|(s, l)| !marked(s, l, r.name))
        .map(|(s, l)| {
            format!(
                "guard: role \"{}\" is owned by {}; a second implementation at {}:{}:\n    {}\n{} or add // guard-allow({}): <why> on this line or the line above",
                r.name,
                owners_text(r),
                s.path,
                l.no,
                l.text.trim(),
                r.hint,
                r.name
            )
        })
        .collect()
}

/// `allow_markers_carry_reasons` failures.
fn marker_problems(sources: &[Source]) -> Vec<String> {
    let marker = re(r"guard-allow\(([^)]*)\)(:?)(.*)");
    let clippy_allow = re(r#"allow\(clippy::disallowed_methods, reason = "([a-z-]+): *[^" ]"#);
    let test_allow = re(r#"cfg_attr\(test, allow\(clippy::disallowed_methods, reason = "[^" ]"#);
    let mut out = vec![];
    for s in sources {
        for l in &s.lines {
            let at = format!("guard: {}:{}", s.path, l.no);
            for cap in marker.captures_iter(&l.text) {
                let name = cap[1].trim();
                let Some(r) = role(name) else {
                    out.push(format!("{at}: guard-allow({name}) names no role"));
                    continue;
                };
                let reason = cap[3].trim();
                if &cap[2] != ":" || reason.is_empty() {
                    out.push(format!("{at}: guard-allow({name}) has no reason"));
                    continue;
                }
                // Adjacent: the marked line is this code line, or the next line
                // when the marker stands on a comment line of its own.
                let target = if l.comment {
                    s.line(l.no + 1).filter(|n| !n.comment)
                } else {
                    Some(l)
                };
                let sites = role_sites(r, std::slice::from_ref(s));
                let adjacent = target.is_some_and(|t| sites.iter().any(|(_, sl)| sl.no == t.no));
                if !adjacent {
                    out.push(format!(
                        "{at}: guard-allow({name}) is not adjacent to a {name} site"
                    ));
                }
            }
            if let Some(attr) = &l.attr {
                if test_allow.is_match(attr) {
                    continue;
                }
                match clippy_allow.captures(attr) {
                    Some(cap) if role(&cap[1]).is_some() => {}
                    Some(cap) => out.push(format!(
                        "{at}: allow(clippy::disallowed_methods, reason = \"{}: ..\") names no role",
                        &cap[1]
                    )),
                    None => out.push(format!(
                        "{at}: allow(clippy::disallowed_methods) has no reason; write \
                         allow(clippy::disallowed_methods, reason = \"<role>: <why>\")"
                    )),
                }
            }
        }
    }
    out
}

fn roles_digest_of(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let begin = "\n// guard-roles-begin\n";
    let start = text
        .find(begin)
        .expect("guard: roles.rs lacks // guard-roles-begin")
        + begin.len();
    let end = start
        + text[start..]
            .find("// guard-roles-end")
            .expect("guard: roles.rs lacks // guard-roles-end");
    Sha256::digest(&text.as_bytes()[start..end])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Size counters: their rises need no `considered`.
fn size_counter(name: &str) -> bool {
    name.starts_with(DOC_BYTES)
        || name.starts_with(MODULE_LINES)
        || COUNTERS.iter().any(|c| {
            c.name == name
                && matches!(
                    c.kind,
                    Kind::Lines | Kind::InlineTestLines | Kind::DocBytesTotal
                )
        })
}

/// `roles_table_is_pinned` / `counters_table_is_pinned` failures: the roles
/// table (between its markers) and the whole counters file (`initial`, regex,
/// `exclude`, `files`, `scope`, `DOC_INITIAL`) must match their pinned digests.
fn pin_problems(base: &Baseline, roles_rs: &str, counters_rs: &str) -> Vec<String> {
    let mut out = vec![];
    let roles = roles_digest_of(roles_rs);
    if roles != ROLES_DIGEST || roles != base.roles_digest {
        out.push(format!(
            "guard: roles table changed; update ROLES_DIGEST and guard/baseline.json (reviewed change)\n  \
             sha256 between the markers: {roles}\n  ROLES_DIGEST: {ROLES_DIGEST}\n  baseline roles_digest: {}",
            base.roles_digest
        ));
    }
    let counters = sha256_hex(counters_rs.as_bytes());
    if counters != base.counters_digest {
        out.push(format!(
            "guard: counters table changed; update counters_digest in guard/baseline.json (reviewed change: \
             an initial, regex, exclude, files, scope or DOC_INITIAL edit can hide sites)\n  \
             sha256 of src/guard/counters.rs: {counters}\n  baseline counters_digest: {}",
            base.counters_digest
        ));
    }
    out
}

/// Method paths listed in `clippy.toml` `disallowed-methods`.
fn clippy_paths(toml: &str) -> BTreeSet<String> {
    let Some(at) = toml.find("disallowed-methods") else {
        return BTreeSet::new();
    };
    let rest = &toml[at..];
    let Some(open) = rest.find('[') else {
        return BTreeSet::new();
    };
    let mut depth = 0;
    let mut close = rest.len();
    for (i, c) in rest[open..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let array = &rest[open + 1..close];
    let item = re(r#"(?:(\w+)\s*=\s*)?"([^"]*)""#);
    item.captures_iter(array)
        .filter(|c| c.get(1).is_none_or(|k| k.as_str() == "path"))
        .map(|c| c[2].to_owned())
        .collect()
}

/// `clippy_roles_are_configured` failures.
fn clippy_problems(paths: &BTreeSet<String>) -> Vec<String> {
    let mut out = vec![];
    for r in ROLES.iter().filter(|r| r.mech == Mech::Clippy) {
        for p in r.forbidden {
            match (r.mode, paths.contains(*p)) {
                (Mode::Enforced, false) => out.push(format!(
                    "guard: clippy.toml lacks {p} required by enforced role \"{}\"",
                    r.name
                )),
                (Mode::Counted, true) => out.push(format!(
                    "guard: clippy.toml lists {p} but role \"{}\" is still Counted",
                    r.name
                )),
                _ => {}
            }
        }
    }
    for p in paths {
        if !ROLES
            .iter()
            .any(|r| r.mech == Mech::Clippy && r.forbidden.contains(&p.as_str()))
        {
            out.push(format!(
                "guard: clippy.toml lists {p}, which no clippy role owns"
            ));
        }
    }
    out
}

fn fail_on(problems: Vec<String>) {
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n\n"));
}

// ---------------------------------------------------------------- tests

#[test]
fn counters_match_baseline() {
    let base = baseline();
    fail_on(counter_mismatches(&measure_all(sources(), docs()), &base));
}

#[test]
fn ledger_balances() {
    fail_on(ledger_problems(&baseline()));
}

/// Enforced: documentation sizes and duplicates stay within their hard caps.
#[test]
fn doc_caps_hold() {
    fail_on(cap_violations(&measure_all(sources(), docs()), &baseline()));
}

#[test]
fn doc_caps_are_enforced() {
    let base = baseline();
    let cap = base.caps["doc_bytes:docs/cli.md"].value;
    let fake = |name: &str, value: i64| {
        let mut m = measure_all(&[], &[]);
        m.insert(
            name.to_owned(),
            Measured {
                value: value as usize,
                sites: vec![],
            },
        );
        m
    };
    // At the cap passes; one byte above fails with the ready-to-paste entry.
    assert!(cap_violations(&fake("doc_bytes:docs/cli.md", cap), &base).is_empty());
    let v = cap_violations(&fake("doc_bytes:docs/cli.md", cap + 1), &base);
    assert_eq!(v.len(), 1, "{v:#?}");
    assert!(v[0].starts_with(&format!(
        "guard: counter doc_bytes:docs/cli.md is {}, above its cap {cap} (Enforced)",
        cap + 1
    )));
    assert!(v[0].contains("{counter: \"cap:doc_bytes:docs/cli.md\", by: +1,"));
    // A duplicated paragraph exceeds the zero cap.
    assert_eq!(
        cap_violations(&fake("doc_duplicate_paragraphs", 1), &base).len(),
        1
    );
    // A new doc file has no cap (and a pinned cap of 0).
    let v = cap_violations(&fake("doc_bytes:docs/new.md", 10), &base);
    assert!(v[0].starts_with("guard: counter doc_bytes:docs/new.md is 10 and has no cap"));
    assert_eq!(cap_initial_of("doc_bytes:docs/new.md"), Some(0));
    assert_eq!(cap_initial_of("src_lines"), Some(103063));
    // Raising a cap needs a balancing ledger entry with a real "considered".
    let mut base = baseline();
    base.caps.get_mut("doc_bytes:docs/cli.md").unwrap().value += 100;
    let p = ledger_problems(&base);
    assert!(
        p.iter()
            .any(|m| m.starts_with("guard: cap doc_bytes:docs/cli.md value")),
        "{p:#?}"
    );
    base.ledger.push(Entry {
        counter: "cap:doc_bytes:docs/cli.md".into(),
        by: 100,
        reason: "x".into(),
        considered: "n/a".into(),
        step: "S9".into(),
        ..Default::default()
    });
    let p = ledger_problems(&base);
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(p[0].contains("raises the cap (by: +100) without \"considered\""));
    base.ledger.last_mut().unwrap().considered = "cut the duplicated example".into();
    assert!(ledger_problems(&base).is_empty());
    // A cap entry for an unknown cap and a capped counter without a cap.
    base.ledger.push(Entry {
        counter: "cap:doc_bytes:docs/none.md".into(),
        by: -1,
        reason: "x".into(),
        considered: String::new(),
        step: "S9".into(),
        ..Default::default()
    });
    base.caps.remove("doc_bytes:AGENTS.md");
    let p = ledger_problems(&base);
    assert!(
        p.iter()
            .any(|m| m.contains("names a cap missing from \"caps\"")),
        "{p:#?}"
    );
    assert!(
        p.contains(
            &"guard: counter doc_bytes:AGENTS.md has no cap in guard/baseline.json \"caps\""
                .to_owned()
        ),
        "{p:#?}"
    );
}

const ROLES_RS: &str = include_str!("guard/roles.rs");
const COUNTERS_RS: &str = include_str!("guard/counters.rs");

#[test]
fn roles_table_is_pinned() {
    let p = pin_problems(&baseline(), ROLES_RS, COUNTERS_RS);
    fail_on(
        p.into_iter()
            .filter(|m| m.starts_with("guard: roles"))
            .collect(),
    );
}

#[test]
fn counters_table_is_pinned() {
    let p = pin_problems(&baseline(), ROLES_RS, COUNTERS_RS);
    fail_on(
        p.into_iter()
            .filter(|m| m.starts_with("guard: counters"))
            .collect(),
    );
}

#[test]
fn pinned_tables_detect_edits() {
    let base = baseline();
    assert!(pin_problems(&base, ROLES_RS, COUNTERS_RS).is_empty());
    // Raising an initial, widening an exclude, adding a DOC_INITIAL row or
    // narrowing a regex each change the counters digest.
    for (from, to) in [
        ("initial: 93238,", "initial: 93239,"),
        (
            "&[\"src/paths.rs\", \"src/sync.rs\"],",
            "&[\"src/paths.rs\", \"src/sync.rs\", \"src/daemon.rs\"],",
        ),
        (
            "    (\"AGENTS.md\", 103),",
            "    (\"AGENTS.md\", 103),\n    (\"NOTES.md\", 50),",
        ),
        (r#""Command::new\(|"#, r#""Command::new\(x|"#),
    ] {
        assert!(COUNTERS_RS.contains(from), "fixture text missing: {from}");
        let edited = COUNTERS_RS.replacen(from, to, 1);
        let p = pin_problems(&base, ROLES_RS, &edited);
        assert_eq!(p.len(), 1, "{from}: {p:#?}");
        assert!(
            p[0].starts_with("guard: counters table changed;"),
            "{}",
            p[0]
        );
    }
    // A roles edit between the markers changes the roles digest.
    let edited = ROLES_RS.replacen("mode: Mode::Enforced", "mode: Mode::Counted", 1);
    let p = pin_problems(&base, &edited, COUNTERS_RS);
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(p[0].starts_with("guard: roles table changed;"));
}

#[test]
fn allow_markers_carry_reasons() {
    fail_on(marker_problems(sources()));
}

#[test]
fn owners_exist() {
    let root = manifest_root();
    let mut out = vec![];
    for r in ROLES {
        for o in r.owners {
            if !root.join(o.trim_end_matches('/')).exists() {
                out.push(format!(
                    "guard: owner {o} for role \"{}\" does not exist",
                    r.name
                ));
            }
        }
    }
    fail_on(out);
}

#[test]
fn clippy_roles_are_configured() {
    let path = manifest_root().join("clippy.toml");
    let toml =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("guard: {}: {e}", path.display()));
    fail_on(clippy_problems(&clippy_paths(&toml)));
}

/// Design §4.7 (from S1a): every provider answers every [`Op`] (a refusal always names its
/// reason), `OP_KEYS` names each capability key exactly once, and the ops table agrees with
/// `provider::capabilities` (the `api capabilities` source).
///
/// [`Op`]: crate::provider::ops::Op
#[test]
fn provider_ops_table_is_total() {
    use crate::provider::{
        ProviderId, capabilities,
        ops::{OP_KEYS, Op, ops},
    };
    let mut out = vec![];
    let keys: BTreeSet<&str> = OP_KEYS.iter().map(|(_, k)| *k).collect();
    if keys.len() != OP_KEYS.len() || Op::ALL.len() != OP_KEYS.len() {
        out.push("guard: OP_KEYS names a capability key twice".to_owned());
    }
    for p in ProviderId::ALL {
        let o = ops(p);
        if o.id() != p {
            out.push(format!("guard: ops({p}) answers for {}", o.id()));
        }
        let table = serde_json::to_value(capabilities(p)).unwrap();
        let listed: BTreeSet<&str> = table
            .as_object()
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        for k in listed.difference(&keys) {
            out.push(format!("guard: capability key {k} of {p} has no Op"));
        }
        for (op, key) in OP_KEYS {
            assert_eq!(op.key(), key);
            let c = o.capability(op);
            if !c.supported && c.reason.is_none() {
                out.push(format!("guard: provider {p} has no row for Op::{op:?}"));
            }
            if serde_json::to_value(&c).unwrap() != table[key] {
                out.push(format!(
                    "guard: provider {p} Op::{op:?} disagrees with capabilities().{key}"
                ));
            }
        }
    }
    fail_on(out);
}

/// `ledger_read_is_read_only`: in the ledger owner's text, `LedgerRead` must exist
/// with private fields only, and neither an `impl` block for it nor the `read` query
/// module it delegates to may run a write or hand out the connection.
fn ledger_read_problems(rel: &str, text: &str) -> Vec<String> {
    let code = strip_tests(text);
    let sk = String::from_utf8(skeleton(&code)).expect("guard: blanking keeps UTF-8");
    let line_of = |at: usize| code[..at].matches('\n').count() + 1;
    // The `{ … }` body that follows a header match, as a byte range.
    let body = |from: usize| -> Option<(usize, usize)> {
        let open = from + sk[from..].find(['{', ';'])?;
        if sk.as_bytes()[open] != b'{' {
            return None;
        }
        let mut depth = 0;
        for (k, c) in sk.bytes().enumerate().skip(open) {
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((open, k + 1));
                    }
                }
                _ => {}
            }
        }
        None
    };
    let write = re(
        r"(?i-u)\bexecute(_batch)?\s*\(|\btransaction|\bpragma_update|\b(insert|update|delete|create|drop|alter|replace|vacuum|attach)\b",
    );
    let scan = |what: &str, (from, to): (usize, usize), exposes: bool| {
        let mut out = vec![];
        for m in write.find_iter(&code[from..to]) {
            out.push(format!(
                "guard: LedgerRead has a write path at {rel}:{} ({what}: `{}`)",
                line_of(from + m.start()),
                m.as_str()
            ));
        }
        if exposes {
            for m in re(r"\bConnection\b").find_iter(&sk[from..to]) {
                out.push(format!(
                    "guard: LedgerRead hands out its connection at {rel}:{}",
                    line_of(from + m.start())
                ));
            }
        }
        out
    };
    let mut out = vec![];
    let structs: Vec<_> = re(r"(?m)^\s*(pub(\([^)]*\))?\s+)?struct\s+LedgerRead\b")
        .find_iter(&sk)
        .filter_map(|m| body(m.end()))
        .collect();
    let impls: Vec<_> = re(r"(?m)^\s*impl\b[^{;]*\bLedgerRead\b")
        .find_iter(&sk)
        .filter_map(|m| body(m.end()))
        .collect();
    let reads: Vec<_> = re(r"(?m)^\s*(pub(\([^)]*\))?\s+)?mod\s+read\s*\{")
        .find_iter(&sk)
        .filter_map(|m| body(m.end() - 1))
        .collect();
    if structs.len() != 1 || impls.is_empty() {
        out.push(format!(
            "guard: {rel} must define struct LedgerRead and impl blocks for it (found {} structs, {} impls)",
            structs.len(),
            impls.len()
        ));
    }
    for &(from, to) in &structs {
        for m in re(r"\bpub\b").find_iter(&sk[from..to]) {
            out.push(format!(
                "guard: LedgerRead has a public field at {rel}:{}",
                line_of(from + m.start())
            ));
        }
    }
    for &range in &impls {
        out.extend(scan("impl LedgerRead", range, true));
    }
    for &range in &reads {
        out.extend(scan("mod read", range, false));
    }
    out
}

#[test]
fn ledger_read_is_read_only() {
    let rel = "src/messaging/ledger.rs";
    let Ok(text) = std::fs::read_to_string(manifest_root().join(rel)) else {
        panic!("guard: {rel} (the ledger owner, defining LedgerRead) does not exist");
    };
    assert!(
        text.contains("mod read {"),
        "guard: {rel} has no `mod read` query module"
    );
    fail_on(ledger_read_problems(rel, &text));
}

#[test]
fn ledger_read_check_catches_writes() {
    let ok = "pub(super) struct LedgerRead {\n    db: Connection,\n}\n\
              mod read {\n    pub(super) fn get(db: &Connection) -> R { db.query_row(\"SELECT receipt FROM receipts\", [], |r| r.get(0)) }\n}\n\
              impl Reads for LedgerRead {\n    fn get(&self) -> R { read::get(&self.db) }\n}\n\
              #[cfg(test)]\nimpl LedgerRead {\n    fn t(&self) { self.db.execute(\"DELETE FROM x\", []); }\n}\n";
    assert!(
        ledger_read_problems("f.rs", ok).is_empty(),
        "{:?}",
        ledger_read_problems("f.rs", ok)
    );
    let cases = [
        (
            ok.replace(
                "read::get(&self.db) }",
                "read::get(&self.db) }\n    fn w(&self) { self.db.execute(\"x\", []); }",
            ),
            "write path at f.rs:9",
        ),
        (
            ok.replace(
                "SELECT receipt FROM receipts",
                "UPDATE receipts SET receipt=1",
            ),
            "mod read: `UPDATE`",
        ),
        (
            ok.replace("    db: Connection,", "    pub db: Connection,"),
            "public field at f.rs:2",
        ),
        (
            format!(
                "{ok}impl std::ops::Deref for LedgerRead {{\n    type Target = Connection;\n}}\n"
            ),
            "hands out its connection",
        ),
        (
            ok.replace("impl Reads for LedgerRead", "impl Reads for Ledger"),
            "found 1 structs, 0 impls",
        ),
        (
            ok.replace("struct LedgerRead", "struct Reader"),
            "found 0 structs",
        ),
    ];
    for (text, want) in cases {
        let p = ledger_read_problems("f.rs", &text);
        assert!(
            p.iter().any(|m| m.contains(want)),
            "want {want:?} in {p:?}\n{text}"
        );
    }
}

#[test]
fn roles_have_one_owner() {
    let mut out = vec![];
    for r in ROLES.iter().filter(|r| r.mode == Mode::Enforced) {
        out.extend(role_violations(r, sources()));
    }
    fail_on(out);
}

#[test]
fn enforced_roles_match_baseline() {
    let base = baseline();
    let code: BTreeSet<&str> = ROLES
        .iter()
        .filter(|r| r.mode == Mode::Enforced)
        .map(|r| r.name)
        .collect();
    let file: BTreeSet<&str> = base.enforced.keys().map(String::as_str).collect();
    assert_eq!(
        code, file,
        "guard: Enforced roles in src/guard/roles.rs differ from guard/baseline.json \"enforced\""
    );
    for (r, step) in &base.enforced {
        assert!(
            !step.trim().is_empty(),
            "guard: enforced role \"{r}\" names no step"
        );
    }
}

#[test]
fn role_counter_map_is_consistent() {
    let mut out = vec![];
    let mut names = BTreeSet::new();
    for c in COUNTERS {
        if !names.insert(c.name) {
            out.push(format!("guard: counter {} is defined twice", c.name));
        }
        if c.name.starts_with(DOC_BYTES) {
            out.push(format!(
                "guard: counter {} uses the reserved doc_bytes: prefix",
                c.name
            ));
        }
        if let Some(rn) = c.role {
            match role(rn) {
                Some(r) if r.counters.contains(&c.name) => {}
                Some(_) => out.push(format!(
                    "guard: role \"{rn}\" does not list its counter {}",
                    c.name
                )),
                None => out.push(format!(
                    "guard: counter {} names unknown role \"{rn}\"",
                    c.name
                )),
            }
        }
        if matches!(c.kind, Kind::Sites | Kind::Markers | Kind::Distinct) {
            re(c.regex);
        }
    }
    let mut role_names = BTreeSet::new();
    for r in ROLES {
        if !role_names.insert(r.name) {
            out.push(format!("guard: role \"{}\" is defined twice", r.name));
        }
        for cn in r.counters {
            if !COUNTERS
                .iter()
                .any(|c| c.name == *cn && c.role == Some(r.name))
            {
                out.push(format!(
                    "guard: role \"{}\" lists counter {cn}, which is not its counter",
                    r.name
                ));
            }
        }
        let markers = format!("{}_allow_markers", r.name.replace('-', "_"));
        if !r.counters.contains(&markers.as_str()) {
            out.push(format!(
                "guard: role \"{}\" lacks counter {markers}",
                r.name
            ));
        }
        if r.mech == Mech::Regex {
            for f in r.forbidden {
                re(f);
            }
        }
        for p in r.permitted {
            re(p);
        }
    }
    fail_on(out);
}

/// Prints every counter's current value, the doc duplicates and the roles digest.
#[test]
#[ignore = "measurement helper: cargo test --lib guard::measure -- --ignored --nocapture"]
fn measure() {
    let docs = docs();
    // GUARD_SITES=<counter>[,<counter>..] also lists those counters' sites.
    let show = std::env::var("GUARD_SITES").unwrap_or_default();
    let show: BTreeSet<&str> = show.split(',').collect();
    let actual = measure_all(sources(), docs);
    for (name, m) in &actual {
        println!("{name} {}", m.value);
        if show.contains(name.as_str()) {
            for s in &m.sites {
                println!("  {s}");
            }
        }
    }
    println!("roles_digest {}", roles_digest_of(ROLES_RS));
    println!("counters_digest {}", sha256_hex(COUNTERS_RS.as_bytes()));
    println!("files {}", sources().len());
    println!("ledger_prefix_digest {}", ledger_prefix_digest(&baseline()));
    println!("tables_digest {}", tables_digest(&baseline()));
    for s in sources() {
        if !MODULES.iter().any(|(m, _)| in_module(&s.path, m)) {
            println!("unlisted {}", s.path);
        }
    }
    for (p, files) in duplicate_paragraphs(docs).iter().take(10) {
        println!("dup [{}] {}", files.join(", "), first_chars(p, 60));
    }
    if let Ok(base) = load_baseline(&manifest_root()) {
        for (name, cap) in &base.caps {
            let now = actual.get(name).map_or(0, |m| m.value);
            println!("cap {name} {} (measured {now})", cap.value);
        }
        for v in cap_violations(&actual, &base) {
            println!("\n{v}");
        }
        let (entries, values) = drift(&actual, &base);
        if !entries.is_empty() {
            println!("\n// guard/baseline.json \"counters\": set these values");
            for v in values {
                println!("{v}");
            }
            println!(
                "\n// guard/baseline.json \"ledger\": append these entries (fill in the TODOs)"
            );
            println!("{}", entries.join(",\n"));
        }
    }
}

/// Ready-to-paste ledger entries and `"counters"` values for every counter whose
/// measured value differs from the baseline.
fn drift(actual: &BTreeMap<String, Measured>, base: &Baseline) -> (Vec<String>, Vec<String>) {
    let mut now: BTreeMap<&str, i64> = actual
        .iter()
        .map(|(n, m)| (n.as_str(), m.value as i64))
        .collect();
    for n in base.counters.keys() {
        if n.starts_with(DOC_BYTES) {
            now.entry(n.as_str()).or_insert(0);
        }
    }
    let mut entries = vec![];
    let mut values = vec![];
    for (name, value) in now {
        let was = base.counters.get(name).map_or(0, |b| b.value);
        if value == was {
            continue;
        }
        let by = value - was;
        let considered = if by < 0 || size_counter(name) {
            "n/a"
        } else {
            "TODO: what was tried instead of a new site"
        };
        let entry = serde_json::json!({
            "counter": name,
            "by": by,
            "reason": "TODO",
            "considered": considered,
            "step": "TODO",
        });
        entries.push(format!("    {entry}"));
        values.push(format!(
            "    {}: {{ \"value\": {value} }},",
            serde_json::Value::from(name)
        ));
    }
    (entries, values)
}

/// Why the skipped file `rel` is not test-only, if it is not: it must be
/// declared (`mod name;`) only behind `#[cfg(test)]`/`#[cfg(all(test, ..))]`,
/// or by a declaring file that is itself skipped and test-only.
fn skip_problem(root: &Path, rel: &str, depth: usize) -> Option<String> {
    let (dir, file) = rel.rsplit_once('/')?;
    let (dir, name) = match file.strip_suffix(".rs")? {
        "mod" => dir.rsplit_once('/')?,
        stem => (dir, stem),
    };
    let candidates: Vec<String> = if dir == "src" {
        vec!["src/lib.rs".into(), "src/main.rs".into()]
    } else {
        vec![format!("{dir}.rs"), format!("{dir}/mod.rs")]
    };
    let decl = re(&format!(r"(?m)^\s*(pub(\([^)]*\))?\s+)?mod\s+{name}\s*;"));
    let mut declared_in = vec![];
    for c in &candidates {
        let Ok(text) = std::fs::read_to_string(root.join(c)) else {
            continue;
        };
        if !decl.is_match(&text) {
            continue;
        }
        declared_in.push(c.clone());
        if decl.is_match(&strip_tests(&text)) {
            if skipped(c) && depth < 8 && skip_problem(root, c, depth + 1).is_none() {
                continue;
            }
            return Some(format!(
                "guard: {rel} is skipped as test code but {c} declares `mod {name};` without #[cfg(test)]"
            ));
        }
    }
    if declared_in.is_empty() {
        return Some(format!(
            "guard: {rel} is skipped as test code but no `mod {name};` declares it in {}",
            candidates.join(" or ")
        ));
    }
    None
}

#[test]
fn skipped_files_are_test_only() {
    let root = manifest_root();
    let mut files = vec![];
    rust_files(&root.join("src"), &mut files);
    let mut out = vec![];
    for f in files {
        let rel = rel_path(&root, &f);
        if skipped(&rel) {
            out.extend(skip_problem(&root, &rel, 0));
        }
    }
    fail_on(out);
}

#[test]
fn skipped_file_fixtures() {
    let dir = tempfile::tempdir().unwrap();
    for (f, text) in [
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\n#[cfg(test)]\nmod guard;\n",
        ),
        ("src/a.rs", "#[cfg(test)]\nmod tests;\n"),
        ("src/a/tests.rs", "mod deep;\n"),
        ("src/a/tests/deep.rs", ""),
        ("src/b.rs", "mod tests;\n"),
        ("src/b/tests.rs", ""),
        ("src/c/tests.rs", ""),
        ("src/guard.rs", "mod roles;\n"),
        ("src/guard/roles.rs", ""),
    ] {
        let p = dir.path().join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
    }
    let check = |rel: &str| skip_problem(dir.path(), rel, 0);
    assert_eq!(check("src/a/tests.rs"), None);
    assert_eq!(check("src/a/tests/deep.rs"), None);
    assert_eq!(check("src/guard.rs"), None);
    assert_eq!(check("src/guard/roles.rs"), None);
    assert_eq!(
        check("src/b/tests.rs").unwrap(),
        "guard: src/b/tests.rs is skipped as test code but src/b.rs declares `mod tests;` without #[cfg(test)]"
    );
    assert!(
        check("src/c/tests.rs")
            .unwrap()
            .contains("no `mod tests;` declares it")
    );
}

// --------------------------------------------------- self-check (§4.4 a–m)

/// Line numbers of non-comment lines matching `PATTERN` in a fixture.
fn hits(text: &str) -> Vec<usize> {
    let s = source("src/fixture.rs", text);
    let rx = re("PATTERN");
    s.lines
        .iter()
        .filter(|l| !l.comment && rx.is_match(&l.text))
        .map(|l| l.no)
        .collect()
}

const FIXTURE_ROLE: Role = Role {
    name: "fixture",
    owners: &["src/owner.rs"],
    scope: None,
    forbidden: &["PATTERN"],
    permitted: &[r"\bowner::\w+"],
    counters: &[],
    mech: Mech::Regex,
    mode: Mode::Enforced,
    hint: "use owner::f(..)",
};

#[test]
fn self_check() {
    // (a) inside an inline test module: ignored.
    assert_eq!(
        hits("fn a() {}\n#[cfg(test)]\nmod t {\n    fn f() { PATTERN }\n}\nPATTERN\n"),
        [6],
        "(a)"
    );
    // (b) `mod tests;` ends at its `;`: the next line is scanned.
    assert_eq!(hits("#[cfg(test)]\nmod tests;\nPATTERN\n"), [3], "(b)");
    // (c) a test-only statement ends at its `;`.
    assert_eq!(
        hits(
            "fn main() {\n    #[cfg(test)]\n    tests::die_check(PATTERN);\n    PATTERN;\n    #[cfg(test)] tests::die_check(PATTERN); PATTERN;\n}\n"
        ),
        [4, 5],
        "(c)"
    );
    // (d) cfg(all(test, ..)) items: ignored; cfg(any(test, x)) ships when x holds: kept.
    assert_eq!(
        hits(
            "#[cfg(all(test, unix))]\nfn f() {\n    PATTERN\n}\n#[cfg(any(test, feature = \"x\"))]\nfn g() { PATTERN }\nPATTERN\n"
        ),
        [6, 7],
        "(d)"
    );
    // (n) a `,` at depth 0 ends a test-only match arm, field or variant; the
    // production arms, fields and variants after it are scanned.
    assert_eq!(
        hits(concat!(
            "match x {\n",
            "    #[cfg(test)]\n",
            "    Some(1) => PATTERN_TEST,\n",
            "    Some(2) => PATTERN_PROD,\n",
            "    #[cfg(test)]\n",
            "    Some(3) => { PATTERN_TEST }\n",
            "    _ => PATTERN_PROD,\n",
            "}\n",
            "struct S {\n",
            "    #[cfg(test)]\n",
            "    probe: PATTERN_TEST,\n",
            "    real: PATTERN_PROD,\n",
            "}\n",
            "let s = S {\n",
            "    #[cfg(test)]\n",
            "    probe: vec![PATTERN_TEST, 2],\n",
            "    real: PATTERN_PROD,\n",
            "};\n",
            "enum E {\n",
            "    #[cfg(test)]\n",
            "    Probe(PATTERN_TEST),\n",
            "    Real(PATTERN_PROD),\n",
            "}\n",
        ))
        .len(),
        5,
        "(n)"
    );
    let s = source(
        "src/fixture.rs",
        "match x {\n    #[cfg(test)]\n    Some(1) => PATTERN_TEST,\n    Some(2) => PATTERN_PROD,\n}\n",
    );
    assert!(
        s.lines.iter().all(|l| !l.text.contains("PATTERN_TEST")),
        "(n)"
    );
    // (e) cfg_attr(test, ..) items are production code.
    assert_eq!(
        hits("#[cfg_attr(test, derive(Debug))]\nstruct S {\n    p: PATTERN,\n}\n"),
        [3],
        "(e)"
    );
    // (f) cfg(not(test)) items are production code.
    assert_eq!(
        hits("#[cfg(not(test))]\nfn g() {\n    PATTERN\n}\n"),
        [3],
        "(f)"
    );
    // (g) char and byte-char literals do not move the brace depth or open strings.
    assert_eq!(
        hits(
            "#[cfg(test)]\nfn t() { let c = '{'; let q = '\"'; let b = b'{'; let e = '\\''; }\nPATTERN\nlet c = '{'; let q = '\"'; let b = b'{';\nPATTERN\n"
        ),
        [3, 5],
        "(g)"
    );
    // (h) lifetimes are not char literals.
    assert_eq!(
        hits(
            "fn f<'a>(x: &'a str) -> &'static str {\n    PATTERN\n}\n#[cfg(test)]\nmod t {\n    fn g<'a>() -> &'a str { PATTERN }\n}\nPATTERN\n"
        ),
        [2, 8],
        "(h)"
    );
    // (i) raw strings (any hash count) and plain strings with braces.
    assert_eq!(
        hits(concat!(
            "#[cfg(test)]\n",
            "fn t() { let s = r#\"{\"#; let t = \"}{\"; let u = br##\"\"#{\"##; }\n",
            "let s = r#\"{\"#; let t = \"}{\"; PATTERN PATTERN\n",
        )),
        [3],
        "(i)"
    );
    let s = source(
        "src/fixture.rs",
        "#[cfg(test)]\nfn t() { let s = r#\"{\"#; }\nlet p = PATTERN; let q = PATTERN;\n",
    );
    let counted = s
        .lines
        .iter()
        .filter(|l| l.text.contains("PATTERN"))
        .count();
    assert_eq!(counted, 1, "(i) one site per line");
    // (j) comment lines are ignored.
    assert_eq!(
        hits("// PATTERN\n    /// PATTERN\n//! PATTERN\nPATTERN\n"),
        [4],
        "(j)"
    );
    // (k) string literals on code lines are matched.
    assert_eq!(hits("let s = \"PATTERN_STRING\";\n"), [1], "(k)");
    // (l) markers: same line and the line directly above exempt; two lines above does not.
    let l = source(
        "src/x.rs",
        concat!(
            "let a = PATTERN; // guard-allow(fixture): same line\n",
            "// guard-allow(fixture): line above\n",
            "let b = PATTERN;\n",
            "// guard-allow(fixture): two lines above\n",
            "let c = 1;\n",
            "let d = PATTERN;\n",
            "let e = owner::PATTERN_CALL(x);\n",
        ),
    );
    let v = role_violations(&FIXTURE_ROLE, std::slice::from_ref(&l));
    assert_eq!(v.len(), 1, "(l) {v:#?}");
    assert!(
        v[0].contains("src/x.rs:6:\n    let d = PATTERN;"),
        "(l) {}",
        v[0]
    );
    let owned = source("src/owner.rs", "let a = PATTERN;\n");
    assert!(
        role_violations(&FIXTURE_ROLE, &[owned]).is_empty(),
        "(l) owner"
    );
    // (m) the guard's own files are never scanned.
    let dir = tempfile::tempdir().unwrap();
    let every = format!(
        "{}\n{}\n",
        ROLES
            .iter()
            .flat_map(|r| r.forbidden.iter())
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
        COUNTERS
            .iter()
            .map(|c| c.regex)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    for f in [
        "src/guard.rs",
        "src/guard/roles.rs",
        "src/guard/deep/x.rs",
        "src/tests.rs",
        "src/a/tests/x.rs",
        "src/a/tests.rs",
        "src/ok.rs",
    ] {
        let p = dir.path().join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &every).unwrap();
    }
    let walked: Vec<String> = walk(dir.path()).into_iter().map(|s| s.path).collect();
    assert_eq!(walked, ["src/ok.rs"], "(m)");
    assert!(
        sources().iter().all(|s| !s.path.starts_with("src/guard")),
        "(m) live tree"
    );
}

#[test]
fn marker_checks_catch_bad_markers() {
    let s = source(
        "src/x.rs",
        concat!(
            "// guard-allow(nosuchrole): why\n",
            "let a = 1;\n",
            "// guard-allow(oauth)\n",
            "let t = \".credentials.json\";\n",
            "// guard-allow(oauth): not next to a site\n",
            "let b = 2;\n",
            "#[allow(clippy::disallowed_methods)]\n",
            "#[allow(clippy::disallowed_methods, reason = \"nosuch: x\")]\n",
            "#[allow(clippy::disallowed_methods, reason = \"spawn: owner\")]\n",
            "#![cfg_attr(test, allow(clippy::disallowed_methods, reason = \"tests\"))]\n",
            "// guard-allow(oauth): capability text\n",
            "let ok = \".credentials.json\";\n",
            "#[allow(\n",
            "    clippy::disallowed_methods,\n",
            "    reason = \"spawn: rustfmt wraps long attributes over several lines\"\n",
            ")]\n",
            "#![cfg_attr(\n",
            "    test,\n",
            "    allow(\n",
            "        clippy::disallowed_methods,\n",
            "        reason = \"tests are exempt\"\n",
            "    )\n",
            ")]\n",
            "#[allow(\n",
            "    clippy::disallowed_methods,\n",
            ")]\n",
        ),
    );
    let p = marker_problems(std::slice::from_ref(&s));
    let expect = [
        "guard: src/x.rs:1: guard-allow(nosuchrole) names no role",
        "guard: src/x.rs:3: guard-allow(oauth) has no reason",
        "guard: src/x.rs:5: guard-allow(oauth) is not adjacent to a oauth site",
        "guard: src/x.rs:7: allow(clippy::disallowed_methods) has no reason",
        "guard: src/x.rs:8: allow(clippy::disallowed_methods, reason = \"nosuch: ..\") names no role",
        "guard: src/x.rs:25: allow(clippy::disallowed_methods) has no reason",
    ];
    assert_eq!(p.len(), expect.len(), "{p:#?}");
    for (got, want) in p.iter().zip(expect) {
        assert!(got.starts_with(want), "{got}\n!=\n{want}");
    }
    // Wrapped and one-line reasoned allows are both counted as spawn markers.
    let c = COUNTERS
        .iter()
        .find(|c| c.name == "spawn_allow_markers")
        .unwrap();
    assert_eq!(measure_one(c, std::slice::from_ref(&s), &[]).value, 2);
}

#[test]
fn stripper_edge_cases() {
    // Nested block comments, a `#[cfg(test)]` inside a string or comment, and a
    // `;` inside `[u8; 2]` do not confuse the item scanner.
    assert_eq!(
        hits(
            "/* a /* nested */ { */\nPATTERN\nlet s = \"#[cfg(test)] mod t {\";\nPATTERN\n// #[cfg(test)]\nPATTERN\n"
        ),
        [2, 4, 6]
    );
    assert_eq!(
        hits("#[cfg(test)]\nconst X: [u8; 2] = [1, PATTERN];\nPATTERN\n"),
        [3]
    );
    assert_eq!(
        hits("#[cfg(test)]\n#[derive(Debug)]\nstruct T { a: PATTERN }\nPATTERN\n"),
        [4]
    );
    assert!(path_matches(
        "src/provider/claude/ops.rs",
        "src/provider/*/ops.rs"
    ));
    assert!(!path_matches(
        "src/provider/claude/x/ops.rs",
        "src/provider/*/ops.rs"
    ));
    assert!(path_matches("src/host/herdr.rs", "src/host/"));
    assert!(!path_matches("src/hosting.rs", "src/host/"));
}

#[test]
fn doc_paragraph_normalization() {
    let long = "word ".repeat(40);
    let a = format!("# Title\n\n{long}\nmore *emphasis* `code`\n\nshort\n");
    let b = format!("intro\n\n{}  MORE emphasis CODE\n", long.to_uppercase());
    let c = format!("{long}\nmore emphasis code\n");
    let docs = [
        Doc {
            path: "a.md".into(),
            bytes: a.len(),
            text: a,
        },
        Doc {
            path: "b.md".into(),
            bytes: b.len(),
            text: b,
        },
        Doc {
            path: "c.md".into(),
            bytes: c.len(),
            text: c,
        },
    ];
    let d = duplicate_paragraphs(&docs);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].1, ["a.md", "b.md", "c.md"]);
    let c = COUNTERS
        .iter()
        .find(|c| c.kind == Kind::DocDuplicateParagraphs)
        .unwrap();
    assert_eq!(measure_one(c, &[], &docs).value, 2, "N files count N - 1");
    // Below 160 characters: not a paragraph that counts.
    assert!(paragraphs("short text\n").is_empty());
}

/// Evasion demo: a raw flock, a raw `Command::new`, a `ProviderId::X ==`
/// compare and a credential path outside their owners each fail the guard
/// with the documented message.
#[test]
fn guard_catches_new_sites() {
    let evil = source(
        "src/evil.rs",
        concat!(
            "use fs2::FileExt;\n",
            "fn f(file: &std::fs::File) { file.lock_exclusive().unwrap(); }\n",
            "fn g() { let _ = std::process::Command::new(\"true\"); }\n",
            "fn h(p: ProviderId) -> bool { p == ProviderId::Codex }\n",
            "fn i() -> &'static str { \"/home/x/.claude/.credentials.json\" }\n",
        ),
    );
    let mut all: Vec<Source> = walk(&manifest_root());
    all.push(evil);
    let base = baseline();
    let problems = counter_mismatches(&measure_all(&all, docs()), &base);
    let value = |n: &str| base.counters[n].value;
    for (counter, line) in [
        ("fs2_imports", 1),
        ("flock_sites", 2),
        ("spawn_sites", 3),
        ("provider_enum_dispatch", 4),
        ("oauth_sites", 5),
    ] {
        let head = format!(
            "guard: counter {counter} is {}, baseline value {}; sites:",
            value(counter) + 1,
            value(counter)
        );
        let p = problems
            .iter()
            .find(|p| p.starts_with(&head))
            .unwrap_or_else(|| panic!("no failure for {counter}: {problems:#?}"));
        assert!(p.contains(&format!("\n  src/evil.rs:{line}\n")), "{p}");
        assert!(p.ends_with(&format!(
            "add a ledger entry {{counter, by: +1, reason, considered, step}} and set value = {}, or remove the site",
            value(counter) + 1
        )));
    }
    // The five sites above, plus the 5 new source lines.
    assert_eq!(problems.len(), 6, "{problems:#?}");
    assert!(problems.iter().any(|p| p.starts_with(&format!(
        "guard: counter src_lines is {}, baseline value {}",
        value("src_lines") + 5,
        value("src_lines")
    ))));
    let oauth = role("oauth").unwrap();
    let v = role_violations(oauth, &all);
    assert_eq!(v.len(), 1, "{v:#?}");
    assert_eq!(
        v[0],
        "guard: role \"oauth\" is owned by src/provider/claude/oauth.rs, src/provider/claude/oauth/, src/reset_credit/claude.rs; \
         a second implementation at src/evil.rs:5:\n    fn i() -> &'static str { \"/home/x/.claude/.credentials.json\" }\n\
         use provider::claude::oauth::{access_token, api, usage, organization_uuid, stale} or add // guard-allow(oauth): <why> on this line or the line above"
    );
    // Calls through the owner's surface are not sites.
    let ok = source(
        "src/ok.rs",
        "let t = crate::provider::claude::oauth::access_token(&home)?;\n",
    );
    assert!(role_violations(oauth, &[ok]).is_empty());
}

#[test]
fn baseline_tampering_is_caught() {
    // Raising a value without a ledger entry, or a rise entry with no
    // "considered", does not balance.
    let mut base = baseline();
    let value = base.counters["spawn_sites"].value;
    base.counters.get_mut("spawn_sites").unwrap().value += 1;
    let p = ledger_problems(&base);
    let initial = initial_of("spawn_sites").unwrap();
    let sum = value - initial as i64;
    assert!(
        p.iter().any(|m| m
            == &format!(
                "guard: counter spawn_sites value {} != initial {initial} + ledger {sum}",
                value + 1
            )),
        "{p:#?}"
    );
    base.ledger.push(Entry {
        counter: "spawn_sites".into(),
        by: 1,
        reason: "x".into(),
        considered: String::new(),
        step: "S7".into(),
        ..Default::default()
    });
    let p = ledger_problems(&base);
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(p[0].contains("without \"considered\""));
    // A new doc file is a new counter.
    let mut fake = measure_all(&[], &[]);
    fake.insert(
        format!("{DOC_BYTES}docs/new.md"),
        Measured {
            value: 10,
            sites: vec![],
        },
    );
    let base = baseline();
    assert!(
        counter_mismatches(&fake, &base)
            .iter()
            .any(|m| m.starts_with("guard: counter doc_bytes:docs/new.md is 10, but guard/baseline.json has no such counter"))
    );
    assert_eq!(initial_of("doc_bytes:docs/new.md"), Some(0));
    let (entries, _) = drift(&fake, &base);
    assert!(
        entries
            .iter()
            .any(|e| e.contains("\"counter\":\"doc_bytes:docs/new.md\"")
                && e.contains("\"considered\":\"n/a\"")),
        "{entries:#?}"
    );
    // Clippy configuration: a Counted role's path present is refused, an
    // unknown path is refused.
    let paths: BTreeSet<String> =
        ["std::process::Command::new".to_owned(), "x::y".to_owned()].into();
    let p = clippy_problems(&paths);
    assert!(p.contains(&"guard: clippy.toml lists std::process::Command::new but role \"spawn\" is still Counted".to_owned()));
    assert!(p.contains(&"guard: clippy.toml lists x::y, which no clippy role owns".to_owned()));
    assert_eq!(
        clippy_paths(
            "disallowed-methods = [\n  { path = \"std::fs::rename\", reason = \"atomic writes\" },\n  \"std::fs::write\",\n]\n"
        ),
        ["std::fs::rename".to_owned(), "std::fs::write".to_owned()].into()
    );
}

#[test]
fn provider_dispatch_pattern() {
    let c = COUNTERS
        .iter()
        .find(|c| c.name == "provider_enum_dispatch")
        .unwrap();
    let rx = re(c.regex);
    for hit in [
        "if p == ProviderId::Codex {",
        "if ProviderId::Claude != p {",
        "if provider == Some(ProviderId::Gemini) {",
        "if p == crate::provider::ProviderId::Antigravity {",
        "ProviderId::Codex => 1,",
        "ProviderId::Codex | ProviderId::Claude => 1,",
        "Some(ProviderId::Gemini) if fast => 2,",
        "provider::ProviderId::Codex => 1,",
        "let yes = matches!(p, ProviderId::Codex);",
        "if let Some(ProviderId::Claude) = p {",
        "if let crate::provider::ProviderId::Claude = p {",
        // Tuple arms (review of S1: `plan::name`).
        "(ProviderId::Codex, \"plus\") => Some(\"ChatGPT Plus\"),",
        "(ProviderId::Gemini, _) => {",
    ] {
        assert!(rx.is_match(hit), "should match: {hit}");
    }
    let strings = re(COUNTERS
        .iter()
        .find(|c| c.name == "provider_string_compares")
        .unwrap()
        .regex);
    for hit in [
        "if s.provider == \"codex\" {",
        "\"claude\" | \"codex\" => x,",
        "r.provider.as_deref() == Some(\"gemini\")",
        "Some(\"gemini\") => {",
        "let n = id.strip_prefix(\"codex:\")?;",
    ] {
        assert!(strings.is_match(hit), "should match: {hit}");
    }
    for miss in [
        "Some(gemini::MODEL_RECORD_TYPE) => {",
        "let n = ops::supervised_name(id)?;",
    ] {
        assert!(!strings.is_match(miss), "should not match: {miss}");
    }
    for miss in [
        "let o = ops(ProviderId::Codex);",
        "let c = capabilities(ProviderId::Claude);",
        "let p = id.unwrap_or(ProviderId::Codex);",
        "x.require(ProviderId::Codex)?;",
    ] {
        assert!(!rx.is_match(miss), "should not match: {miss}");
    }
}

// ---------------------------------------------------------------- shrink ratchet (SHRINK §6)

/// A baseline with the marker at its pinned index and nothing after it.
fn shrink_fixture() -> Baseline {
    let mut base = baseline();
    base.ledger.truncate(SHRINK_MARKER_INDEX + 1);
    assert!(is_shrink_marker(&base.ledger[SHRINK_MARKER_INDEX]));
    base
}

fn program_entry(counter: &str, by: i64, step: &str) -> Entry {
    Entry {
        counter: counter.into(),
        by,
        reason: "x".into(),
        considered: "tried a smaller change".into(),
        step: step.into(),
        program: Some("shrink".into()),
        ..Default::default()
    }
}

fn agreed(who: Who) -> Option<Agreed> {
    Some(Agreed {
        who,
        reference: "msg-000001".into(),
    })
}

#[test]
fn shrink_marker_and_labels_are_pinned() {
    let base = baseline();
    assert!(is_shrink_marker(&base.ledger[SHRINK_MARKER_INDEX]));
    assert_eq!(
        base.ledger
            .iter()
            .filter(|e| e.program.as_deref() == Some("shrink") && e.by == 0)
            .count(),
        1
    );
    // Every step label maps to a release that has trajectory rows.
    for label in [
        "shrink-S0",
        "shrink-S1a",
        "shrink-S1b",
        "shrink-S1c",
        "shrink-S2",
        "shrink-S3",
        "shrink-S4a",
        "shrink-S4b",
        "shrink-S4c",
        "shrink-S5",
        "shrink-S6a",
        "shrink-S6b",
        "shrink-S7",
        "shrink-S8",
    ] {
        let release = base.steps.get(label);
        assert!(release.is_some(), "steps lacks {label}");
        if label != "shrink-S0" {
            let release = release.unwrap();
            assert!(
                base.trajectory.iter().any(|r| &r.step == release),
                "no trajectory row for {release} ({label})"
            );
        }
    }
    // The marker is the only by-0 entry and the baseline passes as it stands.
    assert!(shrink_problems(&base).is_empty());
}

#[test]
fn falls_only_rejects_a_rise_without_agreed_by() {
    let mut base = shrink_fixture();
    for counter in ["src_lines", "cap:src_lines", "inline_test_lines"] {
        let mut b = shrink_fixture();
        b.ledger.push(program_entry(counter, 5, "shrink-S1a"));
        let p = shrink_problems(&b);
        assert_eq!(p.len(), 1, "{counter}: {p:#?}");
        assert!(p[0].contains("without agreed_by"), "{p:#?}");
        // A fall, or a rise carrying agreed_by (either who), passes.
        b.ledger.last_mut().unwrap().by = -5;
        assert!(shrink_problems(&b).is_empty());
        b.ledger.last_mut().unwrap().by = 5;
        for who in [Who::Operator, Who::Lead] {
            b.ledger.last_mut().unwrap().agreed_by = agreed(who);
            assert!(shrink_problems(&b).is_empty(), "{counter} {who:?}");
        }
        b.ledger.last_mut().unwrap().agreed_by = Some(Agreed {
            who: Who::Lead,
            reference: " ".into(),
        });
        assert_eq!(shrink_problems(&b).len(), 1);
    }
    // An existing module (value > 0) is falls-only too.
    let existing = MODULES
        .iter()
        .map(|(p, _)| *p)
        .find(|p| initial_of(&format!("{MODULE_LINES}{p}")).unwrap() > 0)
        .expect("an existing module");
    base.ledger.push(program_entry(
        &format!("{MODULE_LINES}{existing}"),
        1,
        "shrink-S1a",
    ));
    assert_eq!(shrink_problems(&base).len(), 1);
    // Entries before the marker are exempt: the same rise earlier is not flagged.
    let mut early = baseline();
    early.ledger[0].by = 5;
    early.ledger[0].counter = "src_lines".into();
    assert!(shrink_problems(&early).is_empty());
    // A second src_lines entry in one program step is refused.
    let mut two = shrink_fixture();
    two.ledger
        .push(program_entry("src_lines", -1, "shrink-S1a"));
    two.ledger
        .push(program_entry("src_lines", -1, "shrink-S1a"));
    let p = shrink_problems(&two);
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(p[0].contains("more than one src_lines entry"));
    // A program step missing from "steps" is refused.
    let mut odd = shrink_fixture();
    odd.ledger
        .push(program_entry("src_lines", -1, "shrink-S99"));
    assert!(shrink_problems(&odd)[0].contains("not in guard/baseline.json \"steps\""));
}

#[test]
fn creating_a_listed_module_is_exempt() {
    let new = MODULES
        .iter()
        .map(|(p, b)| (*p, *b))
        .find(|(p, _)| initial_of(&format!("{MODULE_LINES}{p}")) == Some(0))
        .expect("a module not created yet");
    let name = format!("{MODULE_LINES}{}", new.0);
    let mut base = shrink_fixture();
    base.ledger.push(program_entry(&name, 100, "shrink-S3"));
    assert!(shrink_problems(&base).is_empty(), "creation is exempt");
    // The second rise: the module exists now, so it is falls-only.
    base.ledger.push(program_entry(&name, 10, "shrink-S3"));
    assert_eq!(shrink_problems(&base).len(), 1);
    base.ledger.last_mut().unwrap().agreed_by = agreed(Who::Operator);
    assert!(shrink_problems(&base).is_empty());
    // The budget still binds a new module.
    let mut m = measure_all(&[], &[]);
    m.insert(
        name.clone(),
        Measured {
            value: new.1,
            sites: vec![],
        },
    );
    assert!(module_budget_problems(&m, &baseline()).is_empty());
    m.get_mut(&name).unwrap().value += 1;
    assert_eq!(module_budget_problems(&m, &baseline()).len(), 1);
    // Unlisted paths have no counter at all.
    assert_eq!(initial_of(&format!("{MODULE_LINES}src/not_listed")), None);
}

#[test]
fn module_budgets_hold_for_new_modules() {
    fail_on(module_budget_problems(
        &measure_all(sources(), docs()),
        &baseline(),
    ));
}

#[test]
fn src_lines_cap_is_enforced() {
    let base = baseline();
    assert!(capped("src_lines"));
    let cap = base.caps["src_lines"].value;
    let fake = |value: i64| {
        let mut m = measure_all(&[], &[]);
        m.insert(
            "src_lines".to_owned(),
            Measured {
                value: value as usize,
                sites: vec![],
            },
        );
        m
    };
    assert!(cap_violations(&fake(cap), &base).is_empty());
    let v = cap_violations(&fake(cap + 1), &base);
    assert_eq!(v.len(), 1, "{v:#?}");
    assert!(v[0].contains("cap:src_lines"));
}

#[test]
fn trajectory_gates_only_the_release() {
    const C: (&str, u32) = ("1.69.0", 0);
    let base = baseline();
    let mut m = measure_all(&[], &[]);
    let set = |m: &mut BTreeMap<String, Measured>, name: &str, v: usize| {
        m.insert(
            name.to_owned(),
            Measured {
                value: v,
                sites: vec![],
            },
        );
    };
    let row = |step: &str| {
        base.trajectory
            .iter()
            .filter(|r| r.step == base.steps[step])
            .map(|r| (r.counter.clone(), r.max))
            .collect::<Vec<_>>()
    };
    // Unset and hotfix labels skip, even with absurd values.
    set(&mut m, "src_lines", 10_000_000);
    assert!(trajectory_problems(&base, &m, None, C).is_empty());
    assert!(trajectory_problems(&base, &m, Some(""), C).is_empty());
    assert!(trajectory_problems(&base, &m, Some("hotfix:1.69.1"), ("1.69.1", 1)).is_empty());
    // A hotfix label is refused on a minor release, and a step of another release fails.
    assert_eq!(trajectory_problems(&base, &m, Some("hotfix:x"), C).len(), 1);
    assert_eq!(
        trajectory_problems(&base, &m, Some("shrink-S3"), C).len(),
        1
    );
    // The release label resolves like its step (S2 is over its ceiling here).
    assert!(trajectory_problems(&base, &m, Some("1.69.0"), C)[0].contains("trajectory ceiling"));
    // A release step above its ceiling fails; at the ceiling passes.
    let rows = row("shrink-S2");
    assert!(!rows.is_empty());
    for (c, max) in &rows {
        set(&mut m, c, *max as usize);
    }
    assert!(trajectory_problems(&base, &m, Some("shrink-S2"), C).is_empty());
    let (c, max) = rows[0].clone();
    set(&mut m, &c, max as usize + 1);
    let p = trajectory_problems(&base, &m, Some("shrink-S2"), C);
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(p[0].contains("above the trajectory ceiling"));
    // An unmapped step fails; a release without a row fails (a missed row).
    assert_eq!(
        trajectory_problems(&base, &m, Some("shrink-S42"), C).len(),
        1
    );
    let mut missed = baseline();
    missed
        .trajectory
        .retain(|r| r.step != base.steps["shrink-S2"]);
    let p = trajectory_problems(&missed, &m, Some("shrink-S2"), C);
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(p[0].contains("has no trajectory row"));
}

/// The release gate: `SESSIONSATION_RELEASE_STEP=shrink-S2 scripts/check.sh`.
#[test]
fn trajectory_holds() {
    let step = std::env::var("SESSIONSATION_RELEASE_STEP").ok();
    fail_on(trajectory_problems(
        &baseline(),
        &measure_all(sources(), docs()),
        step.as_deref(),
        (crate::api::CONTRACT_VERSION, crate::api::CONTRACT_PATCH),
    ));
}

#[test]
fn ratchet_tables_are_pinned() {
    fail_on(ratchet_pin_problems(&baseline()));
}

#[test]
fn every_unlisted_ok_path_exists() {
    let root = manifest_root();
    for stem in UNLISTED_OK.iter().chain(
        MODULES
            .iter()
            .map(|(p, _)| p)
            .filter(|p| initial_of(&format!("{MODULE_LINES}{p}")).unwrap() > 0),
    ) {
        assert!(
            root.join(format!("{stem}.rs")).is_file() || root.join(stem).is_dir(),
            "{stem} is allowlisted or budgeted as existing but is gone"
        );
    }
}

#[test]
fn every_production_file_is_listed() {
    fail_on(unlisted_problems(sources()));
}

#[test]
fn inline_test_lines_count_only_braced_items() {
    let text = "fn a() {}\n\
                #[cfg(test)]\nmod tests;\n\
                #[cfg(test)]\nuse std::fmt;\n\
                #[cfg(test)]\nmod t {\n    fn f() { let s = r#\"}\"#; }\n}\n\
                struct S {\n    #[cfg(test)]\n    x: u8,\n    y: u8,\n}\n\
                #[cfg(all(test, unix))]\nfn g() {\n    let _ = '}';\n}\n\
                #[cfg(any(test, unix))]\nfn h() {\n}\n\
                fn tail() {}\n";
    // `mod t` spans 3 lines + attribute = 4 lines; `fn g` 3 + 1 = 4.
    assert_eq!(inline_test_lines(text), 4 + 4);
    assert_eq!(inline_test_lines("fn a() {}\n"), 0);
    assert_eq!(
        inline_test_lines("#[cfg(test)]\nmod tests;\n#[cfg(test)]\nconst X: u8 = 1;\n"),
        0
    );
}

#[test]
fn ratchet_pins_catch_edits() {
    let base = baseline();
    assert!(ratchet_pin_problems(&base).is_empty());
    let mut b = baseline();
    b.ledger[0].by += 1;
    assert_eq!(ratchet_pin_problems(&b).len(), 1);
    let mut b = baseline();
    b.trajectory[1].max += 1;
    assert!(
        ratchet_pin_problems(&b)
            .iter()
            .any(|m| m.contains("steps or trajectory changed"))
    );
    let mut b = baseline();
    b.steps.insert("shrink-S9".into(), "1.99.0".into());
    assert!(!ratchet_pin_problems(&b).is_empty());
    // A trajectory that does not fall strictly is refused even with a matching digest check.
    let mut b = baseline();
    let last = b.trajectory.len() - 1;
    b.trajectory[last].max = b.trajectory[last - 1].max;
    assert!(
        ratchet_pin_problems(&b)
            .iter()
            .any(|m| m.contains("does not fall strictly"))
    );
}

#[test]
fn a_new_unlisted_path_and_a_recreated_module_are_caught() {
    let src = source("src/brand_new.rs", "fn a() {}\n");
    assert_eq!(unlisted_problems(&[src]).len(), 1);
    assert!(unlisted_problems(&[source("src/api.rs", "fn a() {}\n")]).is_empty());
    // An existing module that fell to zero lines is under its budget again when recreated.
    let existing = MODULES
        .iter()
        .find(|(p, _)| initial_of(&format!("{MODULE_LINES}{p}")).unwrap() > 0)
        .unwrap();
    let name = format!("{MODULE_LINES}{}", existing.0);
    let mut base = shrink_fixture();
    let initial = initial_of(&name).unwrap() as i64;
    base.ledger
        .push(program_entry(&name, -initial, "shrink-S3"));
    let mut m = measure_all(&[], &[]);
    m.insert(
        name.clone(),
        Measured {
            value: existing.1 + 1,
            sites: vec![],
        },
    );
    assert_eq!(module_budget_problems(&m, &base).len(), 1);
    // And it stays falls-only after the rewrite rose again.
    base.ledger.push(program_entry(&name, 5, "shrink-S3"));
    assert_eq!(shrink_problems(&base).len(), 1);
}

#[test]
fn agreed_by_refs_look_like_ids_or_urls() {
    for ok in [
        "msg-000001",
        "https://example.org/x/1",
        "8c1f2a3b-0000",
        "api_send:42/7",
    ] {
        assert!(ref_looks_valid(ok), "{ok}");
    }
    for bad in ["", "ok", "see chat", "x y z abc"] {
        assert!(!ref_looks_valid(bad), "{bad}");
    }
}
