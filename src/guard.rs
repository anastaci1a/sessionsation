//! Enforcement harness for owned roles (centralization design §4, step S0).
//!
//! Test-only (`#[cfg(test)] mod guard;` in `lib.rs`). It scans production
//! sources under `src/` (test files, inline `#[cfg(test)]` items, comment lines
//! and this module itself excluded), counts every ratchet counter of
//! [`counters::COUNTERS`] plus the documentation counters, and compares them with
//! `guard/baseline.json`, whose signed ledger must balance against the pinned
//! `initial` values. Roles in [`roles::ROLES`] that are `Enforced` reject any
//! site outside their owners without an adjacent `// guard-allow(<role>): <why>`.
//! Method-call roles are enforced by clippy `disallowed-methods` (`clippy.toml`)
//! once they flip to `Enforced`.
//!
//! Run `cargo test --lib guard::measure -- --ignored --nocapture` to print the
//! current value of every counter.

mod counters;
mod roles;

use counters::{COUNTERS, Counter, DOC_BYTES, DOC_INITIAL, Kind};
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

/// The text with every `#[cfg(test)]` / `#[cfg(all(test, ..))]` item
/// (attribute included) replaced by spaces.
fn strip_tests(text: &str) -> String {
    let sk = skeleton(text);
    let n = sk.len();
    let mut out = text.as_bytes().to_vec();
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
            let end = item_end(&sk, k + 1);
            blank(&mut out, i, end);
            i = end;
        } else {
            i = k + 1;
        }
    }
    String::from_utf8(out).expect("guard: blanking keeps UTF-8")
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

/// Every counter by name, including one `doc_bytes:<path>` per doc file.
fn measure_all(sources: &[Source], docs: &[Doc]) -> BTreeMap<String, Measured> {
    let mut out: BTreeMap<String, Measured> = COUNTERS
        .iter()
        .map(|c| (c.name.to_owned(), measure_one(c, sources, docs)))
        .collect();
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
    COUNTERS.iter().find(|c| c.name == name).map(|c| c.initial)
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
    ledger: Vec<Entry>,
    enforced: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineCounter {
    value: i64,
}

#[derive(Debug, Deserialize)]
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
        if !base.counters.contains_key(&e.counter) {
            out.push(format!("{at} names a counter missing from \"counters\""));
        }
        if e.by == 0 {
            out.push(format!("{at} has by: 0"));
        }
        if e.reason.trim().is_empty() {
            out.push(format!("{at} has no reason"));
        }
        if e.step.trim().is_empty() {
            out.push(format!("{at} has no step"));
        }
        let considered = e.considered.trim();
        if e.by > 0 && !size_counter(&e.counter) && (considered.is_empty() || considered == "n/a") {
            out.push(format!(
                "{at} raises the counter (by: {:+}) without \"considered\" (what was tried instead)",
                e.by
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
        || COUNTERS
            .iter()
            .any(|c| c.name == name && matches!(c.kind, Kind::Lines | Kind::DocBytesTotal))
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
    for (p, files) in duplicate_paragraphs(docs).iter().take(10) {
        println!("dup [{}] {}", files.join(", "), first_chars(p, 60));
    }
    if let Ok(base) = load_baseline(&manifest_root()) {
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
    ] {
        assert!(rx.is_match(hit), "should match: {hit}");
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
