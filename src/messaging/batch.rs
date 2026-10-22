//! Bounded passive observations. Cache hits never renew native evidence timestamps.
use super::*;
use futures_util::{StreamExt, stream};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub api_version: u32,
    pub sessions: Vec<Target>,
    #[serde(default)]
    pub policy: Policy,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub session_id: String,
    pub expected_generation: String,
}
#[derive(Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    IfStale,
    Refresh,
    CachedOnly,
}
#[derive(Serialize, Deserialize)]
struct Cached {
    runtime_generation: Option<String>,
    value: Value,
}
async fn one(paths: &Paths, t: Target, policy: &Policy) -> anyhow::Result<Value> {
    let s = crate::catalog::resolve(paths, &t.session_id, &t.expected_generation)?;
    anyhow::ensure!(s.provider == "codex", "UNSUPPORTED_PROVIDER_OPERATION");
    let runtime = readiness::recorded(paths, &s)?
        .as_ref()
        .map(Attachment::generation);
    let file = paths
        .root
        .join("observations")
        .join(format!("{}.json", s.id));
    if *policy != Policy::Refresh
        && let Ok(bytes) = fs::read(&file)
        && let Ok(c) = serde_json::from_slice::<Cached>(&bytes)
        && c.runtime_generation == runtime
        && c.value["generation"] == s.generation()
        && c.value["explicit_intent"]["id"]
            == json!(external::messaging_stop_intent(paths, &s.conversation_id)?)
    {
        let now = crate::daemon::now();
        let fresh = c.value["observed_at"]
            .as_i64()
            .zip(c.value["valid_until"].as_i64())
            .is_some_and(|(at, until)| at <= now && now < until);
        if fresh || *policy == Policy::CachedOnly {
            let mut v = c.value;
            v["cache_hit"] = json!(true);
            v["process_observed_at"] = json!(now);
            v["freshness"] = json!(if fresh { "fresh" } else { "stale" });
            v["observation"]["checked_at"] = json!(now);
            v["observation"]["status"] = json!(if v["observed_at"].is_null() {
                "unobserved"
            } else if fresh {
                "fresh"
            } else {
                "stale"
            });
            let stop = external::messaging_stop_intent(paths, &s.conversation_id)?;
            let removed = crate::accounts::removed(paths, &s.account)?;
            let enabled = modes::queue::sending_enabled(paths)?;
            let pending = crate::lifecycle::pending(paths, &s.conversation_id, None)?.is_some();
            let reason = if pending {
                Some("session_operation_in_progress")
            } else if removed {
                Some("account_removed")
            } else if stop.is_some() {
                Some("explicit_resume_required")
            } else if !enabled {
                Some("native_sending_disabled")
            } else if !fresh {
                Some(if v["observed_at"].is_null() {
                    v["reason"].as_str().unwrap_or("observation_required")
                } else {
                    "observation_stale"
                })
            } else {
                None
            }
            .map(str::to_owned);
            let reason = reason.as_deref();
            if pending || removed || !enabled || !fresh {
                v["actions"]["resume"] =
                    json!({"available":false,"reason":reason.unwrap_or("observation_required")});
            }
            if !fresh {
                v["last_observed_state"] = v["state"].clone();
                v["runtime_state"] = json!(if v["observed_at"].is_null() {
                    "unobserved"
                } else {
                    "unknown"
                });
            }
            if let Some(reason) = reason {
                for action in ["submit", "queue"] {
                    v["actions"][action] = json!({"available":false,"reason":reason});
                }
            }
            v["explicit_intent"] =
                json!({"state":if stop.is_some(){"stop_requested"}else{"unknown"},"id":stop});
            return Ok(v);
        }
    }
    if *policy == Policy::CachedOnly {
        let mut v = readiness::project(
            &s,
            json!({"state":"unknown","reason":"observation_required","stop_intent_id":external::messaging_stop_intent(paths,&s.conversation_id)?}),
            false,
            crate::daemon::now(),
        );
        v["cache_hit"] = json!(false);
        return Ok(v);
    }
    let mut value = readiness::observe_public(paths, &s.id, &s.generation()).await?;
    value["cache_hit"] = json!(false);
    if value["observed_at"].is_number() {
        private_dir(file.parent().ok_or(Fault("local_io_error"))?)?;
        crate::paths::atomic_write(
            &file,
            &serde_json::to_vec(&Cached {
                runtime_generation: runtime,
                value: value.clone(),
            })?,
        )?;
    }
    Ok(value)
}
pub async fn observations(paths: &Paths, r: BatchRequest) -> anyhow::Result<Value> {
    anyhow::ensure!(
        r.api_version == crate::api::VERSION,
        "UNSUPPORTED_API_VERSION"
    );
    anyhow::ensure!(r.sessions.len() <= 64, "TOO_MANY_TARGETS");
    let mut ids = std::collections::BTreeSet::new();
    for t in &r.sessions {
        uuid(&t.session_id)?;
        anyhow::ensure!(ids.insert(&t.session_id), "DUPLICATE_TARGET");
    }
    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let policy = &r.policy;
    let mut ordered = r.sessions.into_iter().enumerate().collect::<Vec<_>>();
    ordered.sort_by_key(|(_, t)| {
        fs::read(
            paths
                .root
                .join("observations")
                .join(format!("{}.json", t.session_id)),
        )
        .ok()
        .and_then(|b| serde_json::from_slice::<Cached>(&b).ok())
        .and_then(|c| c.value["observed_at"].as_i64())
        .unwrap_or(0)
    });
    let mut values=stream::iter(ordered.into_iter().map(|(index,t)|async move {
        let id=t.session_id.clone();
        let result=tokio::time::timeout_at(deadline,one(paths,t,policy)).await;
        let value=match result {
            Ok(Ok(v))=>json!({"session_id":id,"ok":true,"data":v}),
            Ok(Err(e))=>json!({"session_id":id,"ok":false,"error":{"code":e.to_string(),"retryable":true}}),
            Err(_)=>json!({"session_id":id,"ok":false,"error":{"code":"OBSERVATION_TIMEOUT","retryable":true}}),
        };(index,value)
    })).buffer_unordered(8).collect::<Vec<_>>().await;
    values.sort_by_key(|(i, _)| *i);
    Ok(
        json!({"observed_at":crate::daemon::now(),"elapsed_ms":started.elapsed().as_millis(),
        "results":values.into_iter().map(|(_,v)|v).collect::<Vec<_>>(),"starts_process":false,"sends_input":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_server};
    #[tokio::test]
    async fn delayed_target_does_not_serialize_batch_or_renew_old_evidence() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let (_dir, paths, input, _) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        fs::remove_file(paths.root.join("managed-panes/1.json")).unwrap();
        let slow = uuid::Uuid::new_v4().to_string();
        let mut targets = Vec::new();
        for index in 0..16 {
            let id = if index == 0 {
                slow.clone()
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            fs::write(
                paths.root.join(format!("managed-panes/{index}.json")),
                json!({"pid":a.pid,"start_time":a.start,"home":a.home,"thread":id}).to_string(),
            )
            .unwrap();
            let s = crate::catalog::record_launch(
                &paths,
                "selected",
                &id,
                &format!("target-{index}"),
                Some(crate::catalog::Attachment {
                    host: "tmux".into(),
                    target: format!("%{index}"),
                    pid: a.pid,
                    process_start: a.start,
                }),
            )
            .unwrap();
            targets.push(Target {
                expected_generation: s.generation(),
                session_id: s.id,
            });
        }
        let listener = tokio::net::UnixListener::bind(a.endpoint()).unwrap();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let slow = slow.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let r: Value = serde_json::from_str(&text).unwrap();
                        let result = match r["method"].as_str().unwrap() {
                            "initialized" => continue,
                            "initialize" => json!({"userAgent":"codex_cli_rs/0.163.0 (Linux)"}),
                            "thread/read" => {
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    if r["params"]["threadId"] == slow {
                                        6000
                                    } else {
                                        150
                                    },
                                ))
                                .await;
                                json!({"thread":{"id":r["params"]["threadId"],"status":{"type":"idle"}}})
                            }
                            "thread/turns/list" => {
                                json!({"data":[{"id":"previous","status":"completed"}]})
                            }
                            other => panic!("Observation mutated provider: {other}"),
                        };
                        if ws
                            .send(Message::Text(
                                json!({"id":r["id"],"result":result}).to_string().into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        let out = observations(
            &paths,
            BatchRequest {
                api_version: 1,
                sessions: targets,
                policy: Policy::Refresh,
            },
        )
        .await
        .unwrap();
        task.abort();
        let rows = out["results"].as_array().unwrap();
        assert_eq!(rows.len(), 16);
        assert!(out["elapsed_ms"].as_u64().unwrap() < 6500);
        for row in &rows[1..] {
            assert_eq!(row["ok"], true);
            assert_eq!(row["data"]["state"], "idle");
            assert_eq!(row["data"]["freshness"], "fresh");
            assert!(row["data"]["observed_at"].is_number());
        }
        assert!(rows[0]["ok"] == false || rows[0]["data"]["observed_at"].is_null());
        eprintln!(
            "16-target passive batch with one delayed endpoint: {}ms; other 15 observed independently",
            out["elapsed_ms"]
        );
    }
    #[tokio::test]
    async fn cache_preserves_evidence_without_mutation_lock_and_reports_stale_unobserved_and_races()
    {
        let (_dir, paths, input, _) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let s = crate::catalog::record_launch(
            &paths,
            "selected",
            &a.id,
            "fixture",
            Some(crate::catalog::Attachment {
                host: "tmux".into(),
                target: "%1".into(),
                pid: a.pid,
                process_start: a.start,
            }),
        )
        .unwrap();
        let server = scripted_server(&a.endpoint(), move |req| {
            match req["method"].as_str().unwrap() {
                "thread/read" => Some(
                    json!({"thread":{"id":req["params"]["threadId"],"status":{"type":"idle"}}}),
                ),
                "thread/turns/list" => {
                    Some(json!({"data":[{"id":"previous","status":"completed"}]}))
                }
                method => panic!("passive call mutated provider: {method}"),
            }
        })
        .await;
        let target = || Target {
            session_id: s.id.clone(),
            expected_generation: s.generation(),
        };
        // HERDR uses the same passive path as tmux, without querying the host.
        let old = paths.root.join("managed-panes/1.json");
        let b: Value = serde_json::from_slice(&fs::read(&old).unwrap()).unwrap();
        fs::create_dir_all(paths.root.join("herdr-panes")).unwrap();
        fs::write(
            paths.root.join("herdr-panes/test.json"),
            json!({"target":"herdr/fixture/w1:p1","binding":b}).to_string(),
        )
        .unwrap();
        fs::remove_file(old).unwrap();
        let _guard = mutation_lock(&paths, &a.id).unwrap();
        let before = std::time::Instant::now();
        let fresh = one(&paths, target(), &Policy::Refresh).await.unwrap();
        let cold_ms = before.elapsed().as_millis();
        assert_eq!(fresh["state"], "idle");
        assert_eq!(fresh["cache_hit"], false);
        server.abort();
        let before = std::time::Instant::now();
        let cached = one(&paths, target(), &Policy::IfStale).await.unwrap();
        let warm_ms = before.elapsed().as_millis();
        assert_eq!(cached["cache_hit"], true);
        assert_eq!(cached["observed_at"], fresh["observed_at"]);
        assert_eq!(cached["valid_until"], fresh["valid_until"]);
        eprintln!(
            "isolated passive observation: cold={cold_ms}ms cached={warm_ms}ms; shared mutation lock held, native writes=0"
        );
        let file = paths
            .root
            .join("observations")
            .join(format!("{}.json", s.id));
        let mut c: Cached = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        c.value["observed_at"] = json!(1);
        c.value["valid_until"] = json!(6);
        fs::write(&file, serde_json::to_vec(&c).unwrap()).unwrap();
        let stale = one(&paths, target(), &Policy::CachedOnly).await.unwrap();
        assert_eq!(stale["observed_at"], 1);
        assert_eq!(stale["observation"]["status"], "stale");
        assert_eq!(stale["actions"]["submit"]["available"], false);
        c.value["observed_at"] = Value::Null;
        c.value["valid_until"] = Value::Null;
        c.value["reason"] = json!("native_thread_read_timeout");
        c.value["observation"]["failure"] = json!({"method":"thread/read","budget_ms":3000});
        fs::write(&file, serde_json::to_vec(&c).unwrap()).unwrap();
        let unobserved = one(&paths, target(), &Policy::CachedOnly).await.unwrap();
        assert_eq!(unobserved["observation"]["status"], "unobserved");
        assert_eq!(unobserved["runtime_state"], "unobserved");
        assert_eq!(
            unobserved["actions"]["submit"]["reason"],
            "native_thread_read_timeout"
        );
        assert_eq!(
            unobserved["actions"]["queue"]["reason"],
            "native_thread_read_timeout"
        );
        assert_eq!(
            unobserved["observation"]["failure"]["method"],
            "thread/read"
        );
        assert!(unobserved["observed_at"].is_null() && unobserved["valid_until"].is_null());
        fs::remove_file(&file).unwrap();
        let missing = one(&paths, target(), &Policy::CachedOnly).await.unwrap();
        assert!(missing["observed_at"].is_null());
        assert_eq!(missing["runtime_state"], "unobserved");
        crate::catalog::rename(&paths, &s.id, &s.generation(), "renamed").unwrap();
        assert!(
            one(&paths, target(), &Policy::CachedOnly)
                .await
                .unwrap_err()
                .to_string()
                .contains("STALE_TARGET")
        );
    }
    #[tokio::test]
    async fn batch_errors_are_per_target_and_duplicate_targets_are_rejected() {
        let (_dir, paths, _, _) = fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let r:BatchRequest=serde_json::from_value(json!({"api_version":1,"policy":"cached_only","sessions":[{"session_id":id,"expected_generation":"missing"}]})).unwrap();
        let out = observations(&paths, r).await.unwrap();
        assert_eq!(out["results"][0]["ok"], false);
        assert_eq!(out["results"][0]["error"]["code"], "SESSION_NOT_FOUND");
        let r:BatchRequest=serde_json::from_value(json!({"api_version":1,"sessions":[{"session_id":id,"expected_generation":"g"},{"session_id":id,"expected_generation":"g"}]})).unwrap();
        assert!(
            observations(&paths, r)
                .await
                .unwrap_err()
                .to_string()
                .contains("DUPLICATE_TARGET")
        );
    }
}
