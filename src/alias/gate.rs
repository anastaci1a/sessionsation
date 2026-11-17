//! Source gate for risk R3 (a missed path-comparison site makes a running
//! session look stopped after the root alias is activated).
//!
//! Outside test code, every line that
//! - calls `fs::canonicalize(`, or
//! - compares (`==` / `!=`) an operand named `home`, `cwd`, `directory`, `root`
//!   (or `*_home`, `*_cwd`, `*_directory`, `*_root`)
//!
//! must either use the alias-aware helpers ([`super::canonical`],
//! [`super::same_path`], [`super::same_str`], [`super::logical`],
//! `layout::same_home`, `layout::resolve_path`) or carry a `raw-path: <reason>`
//! comment on that line, within the five lines above it or on the next line
//! (rustfmt moves trailing comments). A new raw site fails this test with its
//! file and line.
use std::path::{Path, PathBuf};

const MARKER: &str = "raw-path:";
const SAFE: &[&str] = &[
    "alias::canonical(",
    "alias::same_path(",
    "alias::same_str(",
    "alias::logical(",
    "alias::starts_with(",
    "same_home(",
    "resolve_path(",
];
const NAMES: &[&str] = &["home", "cwd", "directory", "root"];

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

fn pathish(segment: &str) -> bool {
    let s = segment.trim_start_matches(['*', '&']);
    NAMES
        .iter()
        .any(|n| s == *n || s.ends_with(&format!("_{n}")))
}

/// The name an operand ends with: `a.b.home` -> `home`, `f(x)` -> `f`, `Some(home)` -> `home`.
fn left_name(code: &str) -> Option<String> {
    let s = code.trim_end();
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    if s.ends_with(')') {
        // A call: the name of the called function or method.
        let mut depth = 0;
        let mut i = bytes.len();
        while i > 0 {
            i -= 1;
            match bytes[i] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        end = i;
    }
    let token: String = s[..end]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '*' | '&' | ':'))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let last = token.rsplit(['.', ':']).next()?.to_owned();
    (!last.is_empty()).then_some(last)
}
fn right_name(code: &str) -> Option<String> {
    let s = code.trim_start();
    let token: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '*' | '&' | ':'))
        .collect();
    let rest = &s[token.len()..];
    if token == "Some" && rest.starts_with('(') {
        return right_name(&rest[1..]);
    }
    if rest.starts_with('(') {
        // A call: judged by its name (`mode(home)` is not a path).
        return token.rsplit(['.', ':']).next().map(str::to_owned);
    }
    let last = token.rsplit(['.', ':']).next()?.to_owned();
    (!last.is_empty()).then_some(last)
}

fn violations_in(path: &Path, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = vec![];
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or_default();
            if next.starts_with("mod ") && next.ends_with('{') {
                break; // the in-file test module ends the production code
            }
        }
        if trimmed.starts_with("//") {
            continue;
        }
        // rustfmt may move a trailing comment into the next line or split the
        // statement, so a reason within the statement's neighbourhood counts.
        let marked = lines[i.saturating_sub(5)..(i + 2).min(lines.len())]
            .iter()
            .any(|l| l.contains(MARKER));
        if marked || SAFE.iter().any(|s| line.contains(s)) {
            continue;
        }
        let code = match line.find("//") {
            Some(at) if !line[..at].ends_with(':') => &line[..at],
            _ => line,
        };
        if code.contains("canonicalize(") {
            out.push(format!(
                "{}:{}: raw canonicalize: {}",
                path.display(),
                i + 1,
                trimmed
            ));
            continue;
        }
        for op in ["==", "!="] {
            let mut from = 0;
            while let Some(at) = code[from..].find(op) {
                let at = from + at;
                from = at + 2;
                let before = &code[..at];
                if before.ends_with(['=', '<', '>', '!']) || code[at + 2..].starts_with('=') {
                    continue;
                }
                let l = left_name(before);
                let r = right_name(&code[at + 2..]);
                if l.as_deref().is_some_and(pathish) || r.as_deref().is_some_and(pathish) {
                    out.push(format!(
                        "{}:{}: raw path comparison: {}",
                        path.display(),
                        i + 1,
                        trimmed
                    ));
                    break;
                }
            }
        }
    }
    out
}

#[test]
fn operand_names() {
    assert_eq!(left_name("if s.home ").as_deref(), Some("home"));
    assert_eq!(
        left_name("x && credentials_stamp(home) ").as_deref(),
        Some("credentials_stamp")
    );
    assert_eq!(right_name(" Some(home)").as_deref(), Some("home"));
    assert_eq!(
        right_name(" auth_mode::Mode::ApiKey").as_deref(),
        Some("ApiKey")
    );
    assert_eq!(right_name(" paths.root {").as_deref(), Some("root"));
    let bad = "fn f() {\n    if a.home == b {}\n    let x = fs::canonicalize(p);\n}\n";
    assert_eq!(violations_in(Path::new("x.rs"), bad).len(), 2);
    let ok = "fn f() {\n    // raw-path: literal\n    if a.home == b {}\n    let x = crate::alias::canonical(p); // fine\n}\n#[cfg(test)]\nmod tests {\n    fn t() { assert!(a.cwd == b); }\n}\n";
    assert!(violations_in(Path::new("x.rs"), ok).is_empty());
}

#[test]
fn no_raw_path_comparisons() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![];
    rust_files(&src, &mut files);
    files.sort();
    let mut all = vec![];
    for f in files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        // Test-only files (`tests.rs`, `*/tests/*.rs`) and this gate itself.
        if name == "tests.rs"
            || f.components().any(|c| c.as_os_str() == "tests")
            || f.ends_with("alias/gate.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        all.extend(violations_in(f.strip_prefix(&src).unwrap_or(&f), &text));
    }
    assert!(
        all.is_empty(),
        "path comparisons must go through crate::alias (or carry a `raw-path:` reason):\n{}",
        all.join("\n")
    );
}
