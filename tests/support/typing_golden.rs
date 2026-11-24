//! Golden HERDR command sequences of the terminal typing engine (step S8). The fake HERDR
//! scripts log every call a `*-terminal-worker` process makes to
//! `$FAKE_HERDR_DIR/worker-calls` with a digest of each call's output. Each scenario's slice
//! of that log is written one call per line as `argv => oN`, where `oN` numbers the distinct
//! outputs in order of first appearance; a call repeated with the same argv and the same
//! output (a poll that saw nothing new) is written once. With the receipt's outcome and
//! journal steps after each scenario, the text is compared
//! with `tests/fixtures/typing-golden/<provider>.txt`. Set `TYPING_GOLDEN_RECORD=1` to
//! write the file instead (only on a reviewed change of typing behaviour).
#![allow(dead_code)]
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub struct Golden {
    log: PathBuf,
    from: usize,
    out: String,
    outputs: Vec<String>,
}

fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

impl Golden {
    pub fn new(herdr: &Path) -> Self {
        Self {
            log: herdr.join("worker-calls"),
            from: 0,
            out: String::new(),
            outputs: vec![],
        }
    }
    /// Start a scenario: later calls belong to it.
    pub fn begin(&mut self, name: &str) {
        self.settle();
        self.from = lines(&self.log).len();
        self.out.push_str(&format!("## {name}\n"));
    }
    /// Wait until the worker log has been quiet for a moment (the worker has exited).
    fn settle(&self) {
        let mut n = lines(&self.log).len();
        let mut quiet = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(20);
        while quiet.elapsed() < Duration::from_millis(600) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            let now = lines(&self.log).len();
            if now != n {
                n = now;
                quiet = Instant::now();
            }
        }
    }
    /// End a scenario with its receipt: the normalized calls and the outcome.
    pub fn end(&mut self, receipt: &Value) {
        self.settle();
        let mut last: Option<Value> = None;
        for l in lines(&self.log).into_iter().skip(self.from) {
            let call: Value = serde_json::from_str(&l).unwrap();
            if last.as_ref() == Some(&call) {
                continue;
            }
            let digest = call["out"].as_str().unwrap().to_owned();
            let n = match self.outputs.iter().position(|d| *d == digest) {
                Some(i) => i + 1,
                None => {
                    self.outputs.push(digest);
                    self.outputs.len()
                }
            };
            self.out.push_str(&format!("{} => o{n}\n", call["args"]));
            last = Some(call);
        }
        let t = &receipt["terminal_input"];
        let outcome = serde_json::json!({
            "state": receipt["state"],
            "reason": receipt["reason"],
            "written": receipt["facts"]["written"],
            "steps": t["steps"],
            "view_switched": t["view_switched"],
            "escaped_prefix": t["escaped_prefix"],
            "draft": t["draft"],
            "correlation": receipt["correlation"]["record_type"],
            "gemini_input": receipt.get("gemini_input").map(|g| serde_json::json!({
                "busy_at_submit": g["busy_at_submit"], "expected": g["expected"],
                "screen_evidence": g["screen_evidence"], "outcome": g["outcome"],
            })),
            "antigravity_input": receipt.get("antigravity_input").map(|a| serde_json::json!({
                "busy_at_submit": a["busy_at_submit"], "key": a["key"], "expected": a["expected"],
                "screen_evidence": a["screen_evidence"], "outcome": a["outcome"],
            })),
        });
        self.out.push_str(&format!("=> {outcome}\n"));
    }
    /// A scenario refused at admission: no worker, no calls.
    pub fn refused(&mut self, error: &str) {
        let code = error
            .split(|c: char| !(c.is_ascii_uppercase() || c == '_'))
            .find(|w| w.len() > 3 && w.contains('_'))
            .unwrap_or("");
        self.out.push_str(&format!("=> refused {code}\n"));
    }
    pub fn check(&self, provider: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/typing-golden")
            .join(format!("{provider}.txt"));
        if std::env::var_os("TYPING_GOLDEN_RECORD").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &self.out).unwrap();
            return;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{} missing; record it on unchanged main", path.display()));
        if want != self.out {
            let (w, g): (Vec<&str>, Vec<&str>) =
                (want.lines().collect(), self.out.lines().collect());
            let at = w
                .iter()
                .zip(&g)
                .position(|(a, b)| a != b)
                .unwrap_or(w.len().min(g.len()));
            panic!(
                "typing golden {provider} differs at line {}:\n  want: {}\n  got:  {}\nfull output:\n{}",
                at + 1,
                w.get(at).unwrap_or(&"<end>"),
                g.get(at).unwrap_or(&"<end>"),
                self.out
            );
        }
    }
}
