//! Ratchet counters (design §5). `initial` is the guard's own value on the S0
//! base commit (2610888) and never changes: every later change of a counter is a
//! signed entry in the `guard/baseline.json` ledger, and the guard requires
//! `value == initial + Σ by` per counter.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Non-comment lines matching `regex`.
    Sites,
    /// Raw line count of the files (`files`, or every scanned source).
    Lines,
    /// Distinct matches of `regex` (with `:` and `"` removed).
    Distinct,
    /// Lines matching `regex`, comment lines included (allow markers).
    Markers,
    /// Sum of the `doc_bytes:<path>` counters.
    DocBytesTotal,
    /// Normalized paragraphs (>= 160 chars) shared by several doc files; a
    /// paragraph in N files counts N - 1.
    DocDuplicateParagraphs,
}

#[derive(Debug)]
pub struct Counter {
    pub name: &'static str,
    /// For Sites/Distinct/Markers; Lines uses `files`.
    pub regex: &'static str,
    /// Path prefixes dropped from the count (today's helpers and future owners;
    /// `*` matches one path segment).
    pub exclude: &'static [&'static str],
    /// Only files under this prefix.
    pub scope: Option<&'static str>,
    /// Exact files the counter applies to; empty = all sources.
    pub files: &'static [&'static str],
    pub role: Option<&'static str>,
    /// Guard value at S0 landing (2610888).
    pub initial: usize,
    pub kind: Kind,
}

const fn sites(
    name: &'static str,
    regex: &'static str,
    exclude: &'static [&'static str],
    role: &'static str,
    initial: usize,
) -> Counter {
    Counter {
        name,
        regex,
        exclude,
        scope: None,
        files: &[],
        role: Some(role),
        initial,
        kind: Kind::Sites,
    }
}

const fn markers(
    name: &'static str,
    regex: &'static str,
    role: &'static str,
    initial: usize,
) -> Counter {
    Counter {
        name,
        regex,
        exclude: &[],
        scope: None,
        files: &[],
        role: Some(role),
        initial,
        kind: Kind::Markers,
    }
}

const PROVIDER_ENUM_DISPATCH: &str = concat!(
    r"(==|!=) *(Some\()?(\w+::)*ProviderId::(Codex|Claude|Gemini|Antigravity)",
    r"|ProviderId::(Codex|Claude|Gemini|Antigravity) *(==|!=)",
    r"|(Some\()?(\w+::)*ProviderId::(Codex|Claude|Gemini|Antigravity)\)?( *\| *(Some\()?(\w+::)*ProviderId::\w+\)?)* *(if [^=]*)?=>",
    r"|matches!\([^,]*, *(Some\()?(\w+::)*ProviderId::",
    r"|if let (Some\()?(\w+::)*ProviderId::",
);

pub const COUNTERS: &[Counter] = &[
    Counter {
        name: "src_lines",
        regex: "",
        exclude: &[],
        scope: None,
        files: &[],
        role: None,
        initial: 93238,
        kind: Kind::Lines,
    },
    Counter {
        name: "api_rs_lines",
        regex: "",
        exclude: &[],
        scope: None,
        files: &["src/api.rs"],
        role: None,
        initial: 5132,
        kind: Kind::Lines,
    },
    // locks
    sites(
        "flock_sites",
        r"(try_)?lock_(exclusive|shared)\(|\bflock\(|File::(try_)?lock\b",
        &["src/paths.rs", "src/sync.rs"],
        "locks",
        83,
    ),
    sites(
        "fs2_imports",
        r"fs2::|libc::flock",
        &["src/paths.rs", "src/sync.rs"],
        "locks",
        29,
    ),
    // atomic writes
    sites(
        "rename_sites",
        r"fs::rename\(|NamedTempFile|\.persist\(",
        &["src/paths.rs"],
        "atomic-writes",
        26,
    ),
    sites(
        "fs_write_sites",
        r"\bfs::write\(|File::create\(",
        &["src/paths.rs"],
        "atomic-writes",
        5,
    ),
    // caches
    sites(
        "json_parse_sites",
        r"serde_json::from_(slice|str|reader)\(",
        &["src/file_cache.rs", "src/record.rs"],
        "caches",
        131,
    ),
    // spawn
    sites(
        "spawn_sites",
        r"Command::new\(|process::Command\b|Command as [A-Za-z_]",
        &[
            "src/background.rs",
            "src/terminal.rs",
            "src/proc.rs",
            "src/host/",
        ],
        "spawn",
        62,
    ),
    sites(
        "env_scrub_sites",
        r"env_remove\(|env -u ",
        &["src/proc.rs"],
        "spawn",
        48,
    ),
    // identity
    sites(
        "proc_stat_readers",
        r"/proc/\{?[a-z_]*\}?/stat\b",
        &["src/external.rs", "src/proc.rs"],
        "identity",
        2,
    ),
    sites(
        "pidfd_sites",
        r"pidfd_open|SYS_pidfd|PidFd|pidfd_send|fn [a-z_]*pidfd",
        &["src/proc.rs"],
        "identity",
        6,
    ),
    sites(
        "alive_fns",
        r"fn (is_)?(alive|gone|is_gone|process_alive|still_running|exited)[a-z_]*\(",
        &["src/proc.rs"],
        "identity",
        8,
    ),
    sites(
        "boot_id_readers",
        r"kernel/random/boot_id",
        &["src/proc.rs"],
        "identity",
        3,
    ),
    // herdr / tmux / ws
    sites(
        "herdr_cli_callers",
        r#"terminal::(call|call_at|raw_at|state|state_at)\(|launch::host_call\(|Command::new\("herdr""#,
        &["src/terminal.rs", "src/pane_reuse.rs", "src/host/"],
        "herdr",
        41,
    ),
    sites(
        "tmux_raw",
        r#"Command::new\("tmux""#,
        &["src/terminal.rs", "src/host/"],
        "tmux",
        5,
    ),
    sites(
        "ws_connects",
        r"tokio_tungstenite::client_async",
        &["src/messaging/transport.rs"],
        "ws",
        3,
    ),
    // waits / clock
    sites(
        "raw_sleeps",
        r"(thread|time)::sleep\(|(^|[^a-z_:.])sleep\(|use std::thread::sleep",
        &["src/timing.rs"],
        "waits",
        98,
    ),
    sites(
        "duration_literals",
        r"Duration::from_(secs|millis)\([0-9]",
        &["src/timing.rs"],
        "waits",
        182,
    ),
    sites(
        "now_sites",
        r"UNIX_EPOCH|fn now(_ms)?\(",
        &["src/timing.rs"],
        "waits",
        26,
    ),
    // provider ops
    sites(
        "provider_string_compares",
        r#"(==|!=) *"(codex|claude|gemini|antigravity)"|"(codex|claude|gemini|antigravity)" *(\| *"[a-z]+" *)*(if .*)?=>"#,
        &["src/provider/"],
        "provider-ops",
        75,
    ),
    sites(
        "provider_enum_dispatch",
        PROVIDER_ENUM_DISPATCH,
        &["src/provider/"],
        "provider-ops",
        159,
    ),
    sites(
        "unsupported_literals",
        r#""UNSUPPORTED_PROVIDER_OPERATION"#,
        &["src/provider/"],
        "provider-ops",
        58,
    ),
    // contract
    sites(
        "since_revision_literals",
        r#"since_revision"? *: *[0-9]"#,
        &["src/contract.rs"],
        "contract",
        23,
    ),
    sites(
        "since_contract_literals",
        r#""since_contract" *: *""#,
        &["src/contract.rs"],
        "contract",
        33,
    ),
    // ledger / sqlite
    sites(
        "ledger_opens",
        r"Ledger::open(_wait|_waiting)?\(",
        &["src/messaging/ledger.rs"],
        "ledger",
        23,
    ),
    sites(
        "receipts_sites",
        r"receipts\.db|adapter\.lock|RECEIPTS_DB",
        &[
            "src/messaging/mod.rs",
            "src/messaging/ledger.rs",
            "src/layout.rs",
        ],
        "ledger",
        2,
    ),
    sites(
        "sqlite_opens",
        r"Connection::open",
        &[
            "src/store.rs",
            "src/usage_history.rs",
            "src/messaging/mod.rs",
            "src/messaging/ledger.rs",
        ],
        "sqlite",
        10,
    ),
    // records
    sites(
        "phase_records",
        r"^\s*pub phase: String",
        &["src/record.rs"],
        "records",
        12,
    ),
    sites(
        "save_fns",
        r"^(pub(\(crate\))? )?fn save[a-z_]*\(paths",
        &["src/record.rs"],
        "records",
        28,
    ),
    sites(
        "worker_lock_sites",
        r"worker\.lock",
        &["src/paths.rs", "src/background.rs", "src/record.rs"],
        "records",
        21,
    ),
    sites(
        "operations_dir_sites",
        r"join\((crate::)?layout::(LAUNCHES|RESUMPTIONS|DELETIONS|ACCOUNT_DELETIONS|ACCOUNT_RENAMES|ACCOUNT_RESET_CREDITS|CLAUDE_SWITCHES|CLAUDE_IDLE_NOTIFY|SHARED_MIGRATIONS|AUTO_RECOVERIES|CONVERSATION_OPERATIONS|HANDOFF_JOBS|SPACE_MOVES|LIFECYCLE_ADMISSION|MESSAGE_RECOVERIES|COMPACTIONS|MODEL_CHANGES|SESSION_LAUNCHES|MOVE_BACKUPS|ROOT_MIGRATIONS|PROJECT_RELOCATION_LOCK|COMPACTIONS_LOCK|MODEL_CHANGES_LOCK)\)",
        &["src/layout.rs", "src/paths.rs", "src/record.rs"],
        "records",
        43,
    ),
    sites(
        "file_cache_statics",
        r"static [A-Z_]+: crate::file_cache::FileCache",
        &["src/record.rs"],
        "records",
        8,
    ),
    // generation
    sites(
        "sha256_digests",
        r"Sha256::digest\(",
        &["src/generation.rs"],
        "generation",
        31,
    ),
    sites(
        "stale_target_literals",
        r#""STALE_TARGET"#,
        &["src/generation.rs"],
        "generation",
        68,
    ),
    // codes
    Counter {
        name: "error_code_literals",
        regex: r#""[A-Z][A-Z0-9]*(_[A-Z0-9]+)+[:"]"#,
        exclude: &["src/codes.rs"],
        scope: None,
        files: &[],
        role: Some("codes"),
        initial: 673,
        kind: Kind::Distinct,
    },
    // typing
    Counter {
        name: "typing_dup_fns",
        regex: r"fn (deliver|observe_loop|recheck|restore|until|clear|withdraw)\(",
        exclude: &["src/messaging/claude_steer/typing.rs"],
        scope: None,
        files: &[
            "src/messaging/claude_steer/terminal.rs",
            "src/messaging/claude_steer/gemini.rs",
            "src/messaging/claude_steer/antigravity.rs",
        ],
        role: Some("typing"),
        initial: 20,
        kind: Kind::Sites,
    },
    sites(
        "apply_session_input_fns",
        r"fn apply_session_input",
        &["src/messaging/claude_steer/typing.rs"],
        "typing",
        3,
    ),
    // settings
    Counter {
        name: "settings_json_claude",
        regex: r"settings\.json",
        exclude: &["src/provider/claude/settings.rs"],
        scope: Some("src/provider/claude"),
        files: &[],
        role: Some("settings"),
        initial: 6,
        kind: Kind::Sites,
    },
    // control
    Counter {
        name: "daemon_methods",
        regex: r#""[a-z_]+(/[a-z_-]+)+" *=>"#,
        exclude: &[],
        scope: None,
        files: &["src/daemon.rs"],
        role: Some("control"),
        initial: 23,
        kind: Kind::Sites,
    },
    // oauth
    sites(
        "oauth_sites",
        r#"credentials\.json|CREDENTIALS_FILE|"curl"|api\.anthropic\.com|/api/oauth|access_token|refresh_token|Authorization"#,
        &["src/provider/claude/oauth", "src/reset_credit/claude.rs"],
        "oauth",
        1,
    ),
    // allow markers, one per role
    markers(
        "locks_allow_markers",
        r#"guard-allow\(locks\)|allow\(clippy::disallowed_methods, reason = "locks"#,
        "locks",
        0,
    ),
    markers(
        "atomic_writes_allow_markers",
        r#"guard-allow\(atomic-writes\)|allow\(clippy::disallowed_methods, reason = "atomic-writes"#,
        "atomic-writes",
        0,
    ),
    markers(
        "records_allow_markers",
        r#"guard-allow\(records\)|allow\(clippy::disallowed_methods, reason = "records"#,
        "records",
        0,
    ),
    markers(
        "caches_allow_markers",
        r#"guard-allow\(caches\)|allow\(clippy::disallowed_methods, reason = "caches"#,
        "caches",
        0,
    ),
    markers(
        "spawn_allow_markers",
        r#"guard-allow\(spawn\)|allow\(clippy::disallowed_methods, reason = "spawn"#,
        "spawn",
        0,
    ),
    markers(
        "identity_allow_markers",
        r#"guard-allow\(identity\)|allow\(clippy::disallowed_methods, reason = "identity"#,
        "identity",
        0,
    ),
    markers(
        "herdr_allow_markers",
        r#"guard-allow\(herdr\)|allow\(clippy::disallowed_methods, reason = "herdr"#,
        "herdr",
        0,
    ),
    markers(
        "tmux_allow_markers",
        r#"guard-allow\(tmux\)|allow\(clippy::disallowed_methods, reason = "tmux"#,
        "tmux",
        0,
    ),
    markers(
        "ws_allow_markers",
        r#"guard-allow\(ws\)|allow\(clippy::disallowed_methods, reason = "ws"#,
        "ws",
        0,
    ),
    markers(
        "typing_allow_markers",
        r#"guard-allow\(typing\)|allow\(clippy::disallowed_methods, reason = "typing"#,
        "typing",
        0,
    ),
    markers(
        "waits_allow_markers",
        r#"guard-allow\(waits\)|allow\(clippy::disallowed_methods, reason = "waits"#,
        "waits",
        0,
    ),
    markers(
        "codes_allow_markers",
        r#"guard-allow\(codes\)|allow\(clippy::disallowed_methods, reason = "codes"#,
        "codes",
        0,
    ),
    markers(
        "generation_allow_markers",
        r#"guard-allow\(generation\)|allow\(clippy::disallowed_methods, reason = "generation"#,
        "generation",
        0,
    ),
    markers(
        "ledger_allow_markers",
        r#"guard-allow\(ledger\)|allow\(clippy::disallowed_methods, reason = "ledger"#,
        "ledger",
        0,
    ),
    markers(
        "sqlite_allow_markers",
        r#"guard-allow\(sqlite\)|allow\(clippy::disallowed_methods, reason = "sqlite"#,
        "sqlite",
        0,
    ),
    markers(
        "state_view_allow_markers",
        r#"guard-allow\(state-view\)|allow\(clippy::disallowed_methods, reason = "state-view"#,
        "state-view",
        0,
    ),
    markers(
        "provider_ops_allow_markers",
        r#"guard-allow\(provider-ops\)|allow\(clippy::disallowed_methods, reason = "provider-ops"#,
        "provider-ops",
        0,
    ),
    markers(
        "contract_allow_markers",
        r#"guard-allow\(contract\)|allow\(clippy::disallowed_methods, reason = "contract"#,
        "contract",
        0,
    ),
    markers(
        "settings_allow_markers",
        r#"guard-allow\(settings\)|allow\(clippy::disallowed_methods, reason = "settings"#,
        "settings",
        0,
    ),
    markers(
        "control_allow_markers",
        r#"guard-allow\(control\)|allow\(clippy::disallowed_methods, reason = "control"#,
        "control",
        0,
    ),
    markers(
        "oauth_allow_markers",
        r#"guard-allow\(oauth\)|allow\(clippy::disallowed_methods, reason = "oauth"#,
        "oauth",
        0,
    ),
    // documentation (operator directive 2026-11-23)
    Counter {
        name: "doc_bytes_total",
        regex: "",
        exclude: &[],
        scope: None,
        files: &[],
        role: None,
        initial: 477891,
        kind: Kind::DocBytesTotal,
    },
    Counter {
        name: "doc_duplicate_paragraphs",
        regex: "",
        exclude: &[],
        scope: None,
        files: &[],
        role: None,
        initial: 0,
        kind: Kind::DocDuplicateParagraphs,
    },
];

/// Prefix of the per-file documentation counters: `doc_bytes:<path>`.
pub const DOC_BYTES: &str = "doc_bytes:";

/// `initial` of each `doc_bytes:<path>` counter at S0. A doc file absent here
/// has initial 0, so a new file needs a ledger entry adding its size.
pub const DOC_INITIAL: &[(&str, usize)] = &[
    ("AGENTS.md", 103),
    ("README.md", 5500),
    ("agents/README.md", 6819),
    ("docs/README.md", 711),
    ("docs/architecture.md", 10016),
    ("docs/cli.md", 110969),
    ("docs/contract.md", 314589),
    ("docs/operations.md", 29184),
];
