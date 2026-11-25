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
    crate::provider::ops::require_bare(crate::provider::ops::supervised(&s.provider))?;
    let runtime = readiness::recorded(paths, &s)?
        .as_ref()
        .map(Attachment::generation);
    let file = paths
        .root
        .join(crate::layout::OBSERVATIONS)
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
            let removed = crate::accounts::removed_session(paths, &s.provider, &s.account)?;
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
                .join(crate::layout::OBSERVATIONS)
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
mod tests;
