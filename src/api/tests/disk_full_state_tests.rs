use super::super::*;

fn enospc() -> anyhow::Error {
    anyhow::Error::new(std::io::Error::from_raw_os_error(libc::ENOSPC)).context("refresh state")
}

#[test]
fn read_only_launch_commands_do_not_refresh_lifecycle_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    for command in [
        Command::Launch {
            command: crate::launch::Command::List,
        },
        Command::Launch {
            command: crate::launch::Command::Validate,
        },
        Command::Launch {
            command: crate::launch::Command::Status {
                request_id: "r".into(),
            },
        },
        Command::ResumeLaunch {
            command: crate::resume::Command::List,
        },
        Command::ResumeLaunch {
            command: crate::resume::Command::Validate,
        },
        Command::ResumeLaunch {
            command: crate::resume::Command::Status {
                request_id: "r".into(),
            },
        },
    ] {
        assert!(lifecycle_subject(&paths, &command).is_none());
    }
    let start = Command::Launch {
        command: crate::launch::Command::Start,
    };
    assert_eq!(lifecycle_subject(&paths, &start), Some(None));
    let resume_start = Command::ResumeLaunch {
        command: crate::resume::Command::Start,
    };
    assert_eq!(lifecycle_subject(&paths, &resume_start), Some(None));
}

#[test]
fn a_full_disk_serves_the_cached_snapshot_with_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    // No snapshot yet: the error stays.
    assert!(serve_snapshot_when_disk_full(&paths, ArchiveFilter::Active, enospc()).is_err());
    let observed_at = daemon::now() - 100;
    let snapshot = json!({"observed_at":observed_at,"accounts":[],"sessions":[{"id":"s1"}]});
    write_snapshot(&paths, ArchiveFilter::Active, &snapshot);
    let served = serve_snapshot_when_disk_full(&paths, ArchiveFilter::Active, enospc()).unwrap();
    assert_eq!(served["sessions"][0]["id"], "s1");
    assert_eq!(served["snapshot"]["cached"], true);
    assert_eq!(served["snapshot"]["fresh"], false);
    assert_eq!(served["snapshot"]["refresh_failed"], "DISK_FULL");
    assert_eq!(
        served["warnings"][0]["code"],
        "STATE_REFRESH_FAILED_DISK_FULL"
    );
    assert_eq!(served["observed_at"], observed_at);
    // Another failure is never masked by an old snapshot.
    let other =
        serve_snapshot_when_disk_full(&paths, ArchiveFilter::Active, anyhow::anyhow!("BOOM"));
    assert!(other.unwrap_err().to_string().contains("BOOM"));
    // Another filter has its own snapshot.
    assert!(serve_snapshot_when_disk_full(&paths, ArchiveFilter::All, enospc()).is_err());
}
