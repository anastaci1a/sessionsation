//! Handoff and Space-move journals written by the 1.72.0 base (shrink-C3) load and re-save
//! byte for byte, so old records stay readable and 1.71.3 reads what this code writes.
use super::super::*;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/handoff-jobs");

/// Move, account switch, restart, terminate, relocation and pause journals in several phases.
#[test]
fn base_handoff_journals_resave_byte_for_byte() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut all: Vec<_> = fs::read_dir(FIXTURES)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            !p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("space-move")
        })
        .collect();
    all.sort();
    assert_eq!(all.len(), 12);
    let mut kinds = std::collections::BTreeSet::new();
    for path in &all {
        let bytes = fs::read(path).unwrap();
        let job: Job = serde_json::from_slice(&bytes).unwrap();
        kinds.insert(format!("{}:{}", job.kind, job.relocation.is_some()));
        save_job(&paths, &job).unwrap();
        let saved = fs::read(job_path(&paths, &job.id).unwrap()).unwrap();
        assert_eq!(saved, bytes, "{}", path.display());
        // The listing parses it back unchanged (settled phases; the rest are long past the
        // start grace, so only their phase and detail are inferred).
        let listed = jobs(&paths).unwrap().into_iter().next().unwrap();
        let mut original = job.clone();
        if original.pending() {
            assert_eq!(listed.phase, "failed", "{}", path.display());
            original.phase = listed.phase.clone();
            original.detail = listed.detail.clone();
        }
        let listed = serde_json::to_vec_pretty(&listed).unwrap();
        assert_eq!(listed, serde_json::to_vec_pretty(&original).unwrap());
        fs::remove_file(job_path(&paths, &job.id).unwrap()).unwrap();
    }
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        [
            "account:false",
            "directory:true",
            "pause:false",
            "restart:true",
            "terminate:false"
        ]
    );
}

/// A journal from before `kind` and most optional fields loads with the base defaults.
#[test]
fn a_legacy_handoff_journal_loads_with_the_base_defaults() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let legacy = json!({"id":"l1","source":{"pane":"%3","tmux_session":"s","pid":7,"cwd":"/w",
        "thread_id":"t","rollout":"/r.jsonl","home":"/h","binary":"codex","resume_options":[]},
        "target":"work","phase":"failed","detail":"The move worker stopped."});
    let job: Job = serde_json::from_value(legacy).unwrap();
    save_job(&paths, &job).unwrap();
    let expected = fs::read(Path::new(FIXTURES).join("legacy-move.json")).unwrap();
    assert_eq!(fs::read(job_path(&paths, "l1").unwrap()).unwrap(), expected);
}

fn queued(paths: &Paths, id: &str, kind: &str, created_at: i64) {
    let mut job: Job = serde_json::from_slice(
        &fs::read(Path::new(FIXTURES).join("account-switch-queued.json")).unwrap(),
    )
    .unwrap();
    job.id = id.into();
    job.kind = kind.into();
    job.created_at = created_at;
    save_job(paths, &job).unwrap();
}

/// One worker per job: a held `<id>.worker-lock` refuses a second worker and a recover, and
/// a free lock still never replays a job that already left `queued`.
#[tokio::test]
async fn a_handoff_job_has_one_worker_at_a_time() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    for kind in ["account", "terminate", "pause", "restart", "directory"] {
        let id = format!("one-{kind}");
        queued(&paths, &id, kind, daemon::now() - 60);
        let lock =
            crate::paths::lock_file(&jobs_dir(&paths).unwrap().join(format!("{id}.worker-lock")))
                .unwrap();
        lock.lock_exclusive().unwrap();
        let second = movement::worker(&paths, &id).await.unwrap_err();
        assert_eq!(second.to_string(), "this move already has a worker");
        let recover = movement::recover(&paths, &id).await.unwrap_err();
        assert_eq!(
            recover.to_string(),
            "the move worker is still reconciling this job; retry when it stops"
        );
        FileExt::unlock(&lock).unwrap();
        let mut started = load(&paths, &id);
        started.phase = "waiting_for_idle".into();
        save_job(&paths, &started).unwrap();
        let replay = movement::worker(&paths, &id).await.unwrap_err();
        assert!(
            replay
                .to_string()
                .starts_with("this job has already started"),
            "{replay}"
        );
        assert_eq!(load(&paths, &id).phase, "waiting_for_idle");
    }
}
fn load(paths: &Paths, id: &str) -> Job {
    serde_json::from_slice(&fs::read(job_path(paths, id).unwrap()).unwrap()).unwrap()
}

/// The dead-worker settle rule: an unfinished job whose worker is gone reads as failed only
/// after the 10 s start grace, and only then may recover take it; a young one stays as it is.
#[tokio::test]
async fn an_unfinished_job_settles_only_after_its_worker_died_and_the_grace() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    for kind in ["account", "terminate", "pause"] {
        let (young, old) = (format!("young-{kind}"), format!("old-{kind}"));
        queued(&paths, &young, kind, daemon::now());
        queued(&paths, &old, kind, daemon::now() - 11);
        let listed = jobs(&paths).unwrap();
        let phase = |id: &str| listed.iter().find(|j| j.id == id).unwrap().phase.clone();
        assert_eq!(phase(&young), "queued");
        assert_eq!(phase(&old), "failed");
        assert_eq!(
            load(&paths, &old).phase,
            "queued",
            "the inference is never written"
        );
        let refused = movement::recover(&paths, &young).await.unwrap_err();
        assert!(
            refused
                .to_string()
                .starts_with("only failed or interrupted"),
            "{refused}"
        );
    }
    // A settled pause recovers without touching any session.
    let pause = movement::recover(&paths, "old-pause").await.unwrap();
    assert_eq!(pause.phase, "recovered");
}

/// Relocation's recover takes no start grace: a young directory change whose worker is gone is
/// accepted at once (a young account switch is still refused).
#[tokio::test]
async fn a_young_relocation_with_a_dead_worker_is_recovered_without_the_grace() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut job: Job = serde_json::from_slice(
        &fs::read(Path::new(FIXTURES).join("relocation-copying.json")).unwrap(),
    )
    .unwrap();
    job.id = "young-directory".into();
    job.phase = "waiting_for_idle".into();
    job.created_at = daemon::now();
    job.source_archived = false;
    (job.source.inactive, job.source.pane) = (true, String::new());
    let r = job.relocation.as_mut().unwrap();
    (r.move_files, r.state) = (false, "prepared".into());
    save_job(&paths, &job).unwrap();
    let recovered = movement::recover(&paths, &job.id).await.unwrap();
    assert_eq!(recovered.phase, "recovered");
    assert_eq!(recovered.relocation.unwrap().state, "rolled_back");
    queued(&paths, "young-account", "account", daemon::now());
    let refused = movement::recover(&paths, "young-account")
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .starts_with("only failed or interrupted"),
        "{refused}"
    );
}
