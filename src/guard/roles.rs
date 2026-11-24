//! Owned roles (design §3). Changing anything between the `guard-roles-begin`
//! and `guard-roles-end` lines changes [`ROLES_DIGEST`], which must be updated
//! together with `guard/baseline.json` (`roles_digest`) as a reviewed change.

/// How the role is held: counted (ratchet only) or enforced (no new site outside
/// its owners without an adjacent, reasoned marker).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Counted,
    Enforced,
}

/// What enforces the role once it is `Enforced`: clippy `disallowed-methods`
/// (`forbidden` holds method paths that must then be in `clippy.toml`) or the
/// guard's own regex scan (`forbidden` holds regexes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mech {
    Clippy,
    Regex,
}

#[derive(Debug)]
pub struct Role {
    /// Role name as written in markers: `guard-allow(<name>)`.
    pub name: &'static str,
    /// Existing owner paths (prefixes; a trailing `/` is a directory). Future
    /// owners appear only in counter `exclude` lists until their step lands.
    pub owners: &'static [&'static str],
    /// Only files under this prefix are subject to the role.
    pub scope: Option<&'static str>,
    /// Regexes (Regex roles) or clippy method paths (Clippy roles).
    pub forbidden: &'static [&'static str],
    /// Regexes whose matches are blanked before `forbidden` and the role's
    /// counters are applied (calls through the owner's public surface).
    pub permitted: &'static [&'static str],
    /// Counters that belong to this role.
    pub counters: &'static [&'static str],
    pub mech: Mech,
    pub mode: Mode,
    pub hint: &'static str,
}

/// sha256 (hex) of the bytes between the `guard-roles-begin` line and the
/// `guard-roles-end` line of this file.
pub const ROLES_DIGEST: &str = "c606308479800069ee3ff3b0ac43232f0889ebbc210ca81e5d1b75139ec1e273";

// guard-roles-begin
pub const ROLES: &[Role] = &[
    Role {
        name: "locks",
        owners: &[],
        scope: None,
        forbidden: &[
            "fs2::FileExt::lock_exclusive",
            "fs2::FileExt::try_lock_exclusive",
            "fs2::FileExt::lock_shared",
            "fs2::FileExt::try_lock_shared",
            "fs2::FileExt::unlock",
            "std::fs::File::lock",
            "std::fs::File::try_lock",
            "std::fs::File::lock_shared",
            "std::fs::File::try_lock_shared",
            "std::fs::File::unlock",
            "libc::flock",
        ],
        permitted: &[],
        counters: &["flock_sites", "fs2_imports", "locks_allow_markers"],
        mech: Mech::Clippy,
        mode: Mode::Counted,
        hint: "use sync::Lock::open(..)",
    },
    Role {
        name: "atomic-writes",
        owners: &["src/paths.rs"],
        scope: None,
        forbidden: &[
            "std::fs::rename",
            "tempfile::NamedTempFile::new",
            "tempfile::NamedTempFile::persist",
            "std::fs::write",
            "std::fs::File::create",
        ],
        permitted: &[],
        counters: &[
            "rename_sites",
            "fs_write_sites",
            "atomic_writes_allow_markers",
        ],
        mech: Mech::Clippy,
        mode: Mode::Counted,
        hint: "use paths::atomic_write(..) / atomic_write_json / atomic_write_mode / atomic_move_dir",
    },
    Role {
        name: "records",
        owners: &[],
        scope: None,
        forbidden: &[
            r"join\((crate::)?layout::(LAUNCHES|RESUMPTIONS|DELETIONS|ACCOUNT_DELETIONS|ACCOUNT_RENAMES|ACCOUNT_RESET_CREDITS|CLAUDE_SWITCHES|CLAUDE_IDLE_NOTIFY|SHARED_MIGRATIONS|AUTO_RECOVERIES|CONVERSATION_OPERATIONS|HANDOFF_JOBS|SPACE_MOVES|LIFECYCLE_ADMISSION|MESSAGE_RECOVERIES|COMPACTIONS|MODEL_CHANGES|SESSION_LAUNCHES|MOVE_BACKUPS|ROOT_MIGRATIONS|PROJECT_RELOCATION_LOCK|COMPACTIONS_LOCK|MODEL_CHANGES_LOCK)\)",
            r"pub phase: String",
            r"worker\.lock",
            r"journal_paths\(",
            r#""operations/"#,
        ],
        permitted: &[],
        counters: &[
            "save_fns",
            "phase_records",
            "worker_lock_sites",
            "operations_dir_sites",
            "file_cache_statics",
            "records_allow_markers",
        ],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use record::Record<T>",
    },
    Role {
        name: "caches",
        owners: &["src/file_cache.rs"],
        scope: None,
        forbidden: &[r"OnceLock<Mutex<"],
        permitted: &[],
        counters: &["json_parse_sites", "caches_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use file_cache::FileCache<T>",
    },
    Role {
        name: "spawn",
        owners: &[],
        scope: None,
        forbidden: &["std::process::Command::new", "tokio::process::Command::new"],
        permitted: &[],
        counters: &["spawn_sites", "env_scrub_sites", "spawn_allow_markers"],
        mech: Mech::Clippy,
        mode: Mode::Counted,
        hint: "use proc::command(Bin::..)",
    },
    Role {
        name: "identity",
        owners: &["src/proc.rs"],
        scope: None,
        forbidden: &[
            r"/proc/\{?[a-z_]*\}?/stat\b",
            r"kernel/random/boot_id",
            r"pidfd_open|SYS_pidfd|pidfd_send_signal",
            r"fn (is_)?(alive|gone|is_gone)\w*\(",
        ],
        permitted: &[],
        counters: &[
            "proc_stat_readers",
            "pidfd_sites",
            "alive_fns",
            "boot_id_readers",
            "identity_allow_markers",
        ],
        mech: Mech::Regex,
        mode: Mode::Enforced,
        hint: "use proc::identity (stat, alive, gone, same, Pidfd, signal_exact, boot_id)",
    },
    Role {
        name: "herdr",
        owners: &[],
        scope: None,
        forbidden: &[
            r#"Command::new\("herdr""#,
            r#"&\["pane""#,
            r#"&\["tab""#,
            r"HERDR_SOCKET_PATH",
        ],
        permitted: &[],
        counters: &["herdr_cli_callers", "herdr_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use host::herdr",
    },
    Role {
        name: "tmux",
        owners: &[],
        scope: None,
        forbidden: &[r#"Command::new\("tmux""#],
        permitted: &[],
        counters: &["tmux_raw", "tmux_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use host::tmux::tmux(socket, args)",
    },
    Role {
        name: "ws",
        owners: &["src/messaging/transport.rs"],
        scope: None,
        forbidden: &[r"tokio_tungstenite::client_async"],
        permitted: &[],
        counters: &["ws_connects", "ws_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use messaging::transport::connect / connect_with",
    },
    Role {
        name: "typing",
        owners: &[],
        scope: Some("src/messaging/claude_steer/"),
        forbidden: &[r"fn (deliver|observe_loop|recheck|restore|until|clear|withdraw)\("],
        permitted: &[],
        counters: &[
            "typing_dup_fns",
            "apply_session_input_fns",
            "typing_allow_markers",
        ],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use claude_steer::typing::Typist with a ScreenProfile",
    },
    Role {
        name: "waits",
        owners: &["src/timing.rs"],
        scope: None,
        forbidden: &[
            "std::thread::sleep",
            "tokio::time::sleep",
            "std::time::SystemTime::now",
        ],
        permitted: &[],
        counters: &[
            "raw_sleeps",
            "duration_literals",
            "now_sites",
            "waits_allow_markers",
        ],
        mech: Mech::Clippy,
        mode: Mode::Counted,
        hint: "use timing::{scaled, secs, budget, Deadline, poll_until, now, now_ms}",
    },
    Role {
        name: "codes",
        owners: &[],
        scope: None,
        forbidden: &[
            r#"(ensure!|bail!|anyhow!|Fault\(|context\().*"[A-Z][A-Z0-9]*(_[A-Z0-9]+)+[:"]"#,
        ],
        permitted: &[],
        counters: &["error_code_literals", "codes_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "declare the code in codes.rs and use codes::<NAME>",
    },
    Role {
        name: "generation",
        owners: &["src/generation.rs"],
        scope: None,
        forbidden: &["sha2::Digest::digest"],
        permitted: &[],
        counters: &[
            "sha256_digests",
            "stale_target_literals",
            "generation_allow_markers",
        ],
        mech: Mech::Clippy,
        mode: Mode::Counted,
        hint: "use generation::of",
    },
    Role {
        name: "ledger",
        owners: &["src/messaging/ledger.rs", "src/layout.rs"],
        scope: None,
        forbidden: &[r"receipts\.db|adapter\.lock|RECEIPTS_DB"],
        permitted: &[],
        counters: &["ledger_opens", "receipts_sites", "ledger_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Enforced,
        hint: "use messaging::ledger::open_read / open_write",
    },
    Role {
        name: "sqlite",
        owners: &["src/store.rs"],
        scope: None,
        forbidden: &[
            "rusqlite::Connection::open",
            "rusqlite::Connection::open_with_flags",
            "rusqlite::Connection::open_in_memory",
        ],
        permitted: &[],
        counters: &["sqlite_opens", "sqlite_allow_markers"],
        mech: Mech::Clippy,
        mode: Mode::Counted,
        hint: "use store::sqlite::open(path, Mode, busy)",
    },
    Role {
        name: "state-view",
        owners: &["src/daemon.rs", "src/store.rs"],
        scope: None,
        forbidden: &[r"load_state_snapshot\("],
        permitted: &[],
        counters: &["state_view_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use state_view::StateView",
    },
    Role {
        name: "provider-ops",
        owners: &["src/provider/"],
        scope: None,
        forbidden: &[
            r#"(==|!=) *"(codex|claude|gemini|antigravity)""#,
            r#""(codex|claude|gemini|antigravity)"( *\| *"[a-z]+")* *(if [^=]*)?=>"#,
            r"(==|!=) *(Some\()?(\w+::)*ProviderId::(Codex|Claude|Gemini|Antigravity)",
            r"ProviderId::(Codex|Claude|Gemini|Antigravity) *(==|!=)",
            r"(Some\()?(\w+::)*ProviderId::(Codex|Claude|Gemini|Antigravity)\)?( *\| *(Some\()?(\w+::)*ProviderId::\w+\)?)* *(if [^=]*)?=>",
            r"matches!\([^,]*, *(Some\()?(\w+::)*ProviderId::",
            r"if let (Some\()?(\w+::)*ProviderId::",
            r"\((Some\()?(\w+::)*ProviderId::(Codex|Claude|Gemini|Antigravity)\)?, *[^)]*\) *(if [^=]*)?=>",
            r#"(==|!=) *Some\("(codex|claude|gemini|antigravity)"\)"#,
            r#"Some\("(codex|claude|gemini|antigravity)"\) *(\| *Some\("[a-z]+"\) *)*(if .*)?=>"#,
            r#"strip_prefix\("(codex|claude|gemini|antigravity):"\)"#,
            r#""UNSUPPORTED_PROVIDER_OPERATION"#,
        ],
        permitted: &[],
        counters: &[
            "provider_string_compares",
            "provider_enum_dispatch",
            "unsupported_literals",
            "provider_ops_allow_markers",
        ],
        mech: Mech::Regex,
        mode: Mode::Enforced,
        hint: "use provider::ops(..)",
    },
    Role {
        name: "contract",
        owners: &[],
        scope: None,
        forbidden: &[r#"since_revision"? *: *[0-9]"#, r#""since_contract" *: *""#],
        permitted: &[],
        counters: &[
            "since_revision_literals",
            "since_contract_literals",
            "contract_allow_markers",
        ],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use contract::FEATURES",
    },
    Role {
        name: "settings",
        owners: &[],
        scope: Some("src/provider/claude"),
        forbidden: &[r"settings\.json"],
        permitted: &[],
        counters: &["settings_json_claude", "settings_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use provider::claude::settings::edit(home, f)",
    },
    Role {
        name: "control",
        owners: &["src/control.rs"],
        scope: Some("src/daemon.rs"),
        forbidden: &[r#""[a-z_]+(/[a-z_-]+)+" *=>"#],
        permitted: &[],
        counters: &["daemon_methods", "control_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Counted,
        hint: "use control::Method::parse",
    },
    Role {
        // Claude-OAuth specifics only (credential file, claudeAiOauth key, Anthropic API
        // host, /api/oauth paths). Generic token words (access_token, refresh_token,
        // Authorization, curl) are shared with Codex/Gemini token code (SM8) and stay out.
        name: "oauth",
        owners: &[
            "src/provider/claude/oauth.rs",
            "src/provider/claude/oauth/",
            "src/reset_credit/claude.rs",
        ],
        scope: None,
        forbidden: &[r#"\.credentials\.json|claudeAiOauth|api\.anthropic\.com|/api/oauth"#],
        permitted: &[r"\bclaude::oauth::\w+"],
        counters: &["oauth_sites", "oauth_allow_markers"],
        mech: Mech::Regex,
        mode: Mode::Enforced,
        hint: "use provider::claude::oauth::{access_token, api, usage, organization_uuid, stale}",
    },
];
// guard-roles-end
