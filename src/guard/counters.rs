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
    /// Lines of every `#[cfg(test)]` / `#[cfg(all(test, ..))]` item that ends in a
    /// `}` (attribute line to closing line): inline test mods, fns, impls. Items
    /// ending in `;` or `,` (`mod tests;`, `use`, a field) are not counted.
    InlineTestLines,
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
    r"|\((Some\()?(\w+::)*ProviderId::(Codex|Claude|Gemini|Antigravity)\)?, *[^)]*\) *(if [^=]*)?=>",
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
        name: "inline_test_lines",
        regex: "",
        exclude: &[],
        scope: None,
        files: &[],
        role: None,
        initial: 19352,
        kind: Kind::InlineTestLines,
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
        concat!(
            r#"(==|!=) *"(codex|claude|gemini|antigravity)"|"(codex|claude|gemini|antigravity)" *(\| *"[a-z]+" *)*(if .*)?=>"#,
            r#"|(==|!=) *Some\("(codex|claude|gemini|antigravity)"\)"#,
            r#"|Some\("(codex|claude|gemini|antigravity)"\) *(\| *Some\("[a-z]+"\) *)*(if .*)?=>"#,
            r#"|strip_prefix\("(codex|claude|gemini|antigravity):"\)"#,
        ),
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
        regex: r"fn (deliver|observe_loop|recheck|restore|until|clear|withdraw|salvage|type_checked|left)\s*[<(]",
        exclude: &["src/messaging/claude_steer/typing.rs"],
        scope: Some("src/messaging/claude_steer/"),
        files: &[],
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
        r#"\.credentials\.json|claudeAiOauth|api\.anthropic\.com|/api/oauth"#,
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

/// Prefix of a ledger entry that moves a hard cap: `cap:<counter>`.
pub const CAP: &str = "cap:";

/// Enforced hard caps of the documentation counters (step D1, operator directive
/// 2026-11-23): the guard fails when a measured value exceeds its cap. These are
/// the D1 sizes plus about 5% headroom; the total cap is the D1 total plus 5%,
/// 39% below the 512857 bytes measured before D1. A cap is never edited here: it
/// moves only by a `cap:<counter>` ledger entry with a reason and a real
/// `considered` (what was cut instead), and `guard/baseline.json` "caps" must
/// equal `initial + Σ by`. A doc file absent here has cap 0, so a new file needs a
/// cap entry as well as its `doc_bytes:` entry.
pub const CAP_INITIAL: &[(&str, usize)] = &[
    ("doc_bytes:AGENTS.md", 110),
    ("doc_bytes:README.md", 2600),
    ("doc_bytes:agents/README.md", 10400),
    ("doc_bytes:docs/README.md", 940),
    ("doc_bytes:docs/architecture.md", 8000),
    ("doc_bytes:docs/changelog.md", 14000),
    ("doc_bytes:docs/cli.md", 20000),
    ("doc_bytes:docs/contract.md", 233400),
    ("doc_bytes:docs/operations.md", 24200),
    ("doc_bytes_total", 313300),
    ("doc_duplicate_paragraphs", 0),
    ("src_lines", 103236),
    ("inline_test_lines", 0),
];

/// Prefix of the per-module line counters: `module_lines:<path>` (SHRINK design
/// §6.1). A path is a module stem: it covers `<path>.rs` and every file under
/// `<path>/`, test files and the guard excluded like `src_lines`.
pub const MODULE_LINES: &str = "module_lines:";

/// Ledger index of the falls-only marker entry (`program: "shrink"`, step
/// `shrink-S0`): entries after it are under the falls-only rule (design §6.2).
pub const SHRINK_MARKER_INDEX: usize = 154;

/// The `src_lines` target budget of each module of the design's §3.1 module map
/// (single-module rows; grouped rows such as `store`/`model`/`usage_history` or
/// `timing`..`lib` have no counter). A module absent here has no counter.
pub const MODULES: &[(&str, usize)] = &[
    ("src/main", 600),
    ("src/api", 1500),
    ("src/contract", 200),
    ("src/codes", 150),
    ("src/daemon", 700),
    ("src/control", 200),
    ("src/rpc", 300),
    ("src/accounts", 1100),
    ("src/reset_credit", 500),
    ("src/catalog", 1200),
    ("src/state", 700),
    ("src/activity", 300),
    ("src/record", 400),
    ("src/launch", 1300),
    ("src/pane_reuse", 250),
    ("src/auto_start", 600),
    ("src/archive", 800),
    ("src/host", 900),
    ("src/messaging/send", 600),
    ("src/messaging/ledger", 400),
    ("src/messaging/codex_input", 600),
    ("src/messaging/transport", 300),
    ("src/messaging/typing", 900),
    ("src/messaging/peer", 500),
    ("src/questions", 600),
    ("src/messaging/model_change", 500),
    ("src/messaging/compaction", 400),
    ("src/retention", 300),
    ("src/provider/mod", 400),
    ("src/provider/caps", 150),
    ("src/provider/codex", 900),
    ("src/provider/claude", 2000),
    ("src/provider/antigravity", 800),
    ("src/provider/events", 500),
    ("src/provider/observe", 350),
    ("src/provider/usage", 600),
    ("src/session_model", 300),
    ("src/paths", 350),
    ("src/alias", 300),
    ("src/shared_state", 200),
    ("src/proc", 300),
    ("src/file_cache", 150),
    ("src/config", 200),
    ("src/disk_space", 100),
];

/// `initial` of each `module_lines:<path>` counter at the S0 landing of the
/// shrink program. A module absent here has initial 0 (a module not created yet).
pub const MODULE_INITIAL: &[(&str, usize)] = &[
    ("src/accounts", 654),
    ("src/alias", 706),
    ("src/api", 5663),
    ("src/catalog", 3912),
    ("src/config", 276),
    ("src/control", 442),
    ("src/daemon", 1503),
    ("src/disk_space", 177),
    ("src/file_cache", 207),
    ("src/launch", 1661),
    ("src/main", 2188),
    ("src/messaging/compaction", 1565),
    ("src/messaging/ledger", 824),
    ("src/messaging/model_change", 1882),
    ("src/messaging/transport", 586),
    ("src/pane_reuse", 546),
    ("src/paths", 511),
    ("src/proc", 199),
    ("src/provider/antigravity", 2188),
    ("src/provider/claude", 9405),
    ("src/provider/codex", 298),
    ("src/provider/events", 1004),
    ("src/provider/mod", 358),
    ("src/questions", 1109),
    ("src/reset_credit", 1160),
    ("src/retention", 401),
    ("src/rpc", 299),
    ("src/session_model", 524),
    ("src/shared_state", 1431),
];

/// sha256 of `ledger[..=SHRINK_MARKER_INDEX]` (see `guard::ledger_prefix_digest`): the entries
/// before the falls-only marker are never edited.
pub const LEDGER_PREFIX_DIGEST: &str =
    "ed08070acb6d556ff4c0dad1e0851a9569760526b92059d2a1360de04dd0c0ff";
/// sha256 of baseline `steps` and `trajectory` (see `guard::tables_digest`): loosening a row is
/// a reviewed change of this file.
pub const TABLES_DIGEST: &str = "87174a895f5a52f50635990275007df70b363f3a08853781279e137893b03c97";
/// Existing modules (file stems; `<stem>.rs` and `<stem>/`) that have no budget row. A production
/// file in neither `MODULES` nor here fails `every_production_file_is_listed`.
pub const UNLISTED_OK: &[&str] = &[
    "src/account_deletion",
    "src/account_rename",
    "src/auto_recover",
    "src/background",
    "src/caller_origin",
    "src/claude_idle",
    "src/claude_switch",
    "src/external",
    "src/generation",
    "src/identity",
    "src/inline",
    "src/layout",
    "src/lib",
    "src/lifecycle",
    "src/live_activity",
    "src/login",
    "src/logout",
    "src/messaging/batch",
    "src/messaging/checkpoints",
    "src/messaging/claude_steer",
    "src/messaging/delivery",
    "src/messaging/headless",
    "src/messaging/input_health",
    "src/messaging/mod",
    "src/messaging/modes",
    "src/messaging/observe",
    "src/messaging/public",
    "src/messaging/readiness",
    "src/messaging/recovery",
    "src/messaging/retention",
    "src/model",
    "src/native_activity",
    "src/plan",
    "src/provider/activity",
    "src/provider/compact",
    "src/provider/conversation",
    "src/provider/dispatch",
    "src/provider/gemini",
    "src/provider/hooks",
    "src/provider/native",
    "src/provider/ops",
    "src/provider/send",
    "src/provider/session_meters",
    "src/provider/stop_shape",
    "src/provider/usage_cycle",
    "src/provider/usage_meter",
    "src/resume",
    "src/self_executable",
    "src/session_launch",
    "src/store",
    "src/sync",
    "src/terminal",
    "src/timing",
    "src/turn_notify",
    "src/usage_history",
];
