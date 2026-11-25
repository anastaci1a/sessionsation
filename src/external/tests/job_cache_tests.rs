use super::super::*;

fn job_json(id: &str, phase: &str, created_at: i64, pad: &str) -> Value {
    json!({"id":id,"kind":"move","target":"work","phase":phase,"detail":pad,"created_at":created_at,
        "source":{"pane":"%1","tmux_session":"s","pid":0,"cwd":"/w","thread_id":"t","rollout":"/r.jsonl",
            "home":"/h","binary":"codex","resume_options":[]}})
}
fn put(paths: &Paths, v: &Value) {
    let id = v["id"].as_str().unwrap();
    atomic_write(
        &job_path(paths, id).unwrap(),
        &serde_json::to_vec(v).unwrap(),
    )
    .unwrap();
}
/// What a list without any cache returns: every readable job, newest first, with the
/// worker-liveness inference applied to the unfinished ones.
fn fresh(paths: &Paths) -> Vec<(String, String)> {
    let mut all: Vec<Job> = fs::read_dir(jobs_dir(paths).unwrap())
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "json")
                .then(|| serde_json::from_slice::<Job>(&fs::read(p).unwrap()).ok())?
        })
        .collect();
    all.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    all.into_iter()
        .map(|mut j| {
            if !matches!(
                j.phase.as_str(),
                "complete" | "failed" | "cancelled" | "recovered"
            ) && daemon::now() - j.created_at > 10
                && !handoff_worker_alive(&j.id)
            {
                j.phase = "failed".into();
            }
            (j.id, j.phase)
        })
        .collect()
}
fn cached(paths: &Paths) -> Vec<(String, String)> {
    jobs(paths)
        .unwrap()
        .into_iter()
        .map(|j| (j.id, j.phase))
        .collect()
}

#[test]
fn cached_jobs_list_like_fresh_reads_and_the_worker_inference_is_never_cached() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    crate::file_cache::trust_fresh_stamps(true);
    let now = daemon::now();
    put(&paths, &job_json("done", "complete", now - 500, "x"));
    put(
        &paths,
        &job_json("old-running", "running_on_target", now - 500, "x"),
    );
    put(
        &paths,
        &job_json("young-running", "running_on_target", now, "x"),
    );
    for _ in 0..2 {
        assert_eq!(cached(&paths), fresh(&paths));
    }
    // An unfinished job whose worker is gone reads as failed, on every call, while the
    // record itself still says it is running.
    let phases: std::collections::BTreeMap<_, _> = cached(&paths).into_iter().collect();
    assert_eq!(phases["old-running"], "failed");
    assert_eq!(phases["young-running"], "running_on_target");
    assert_eq!(phases["done"], "complete");
    // Rewritten, removed, corrupted.
    put(
        &paths,
        &job_json("done", "failed", now - 500, "a longer detail"),
    );
    assert_eq!(cached(&paths), fresh(&paths));
    fs::remove_file(job_path(&paths, "young-running").unwrap()).unwrap();
    assert_eq!(cached(&paths), fresh(&paths));
    fs::write(job_path(&paths, "done").unwrap(), b"{").unwrap();
    assert_eq!(cached(&paths), fresh(&paths));
    assert_eq!(cached(&paths).len(), 1);
    crate::file_cache::trust_fresh_stamps(false);
}
