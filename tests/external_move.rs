//! Synthetic servers and isolated tmux only: these tests never touch real accounts.
#[path = "external_move/autostart.rs"]
mod autostart;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sessionsation::{
    config::Config,
    daemon,
    model::{Account, State, Usage, Window},
    paths::Paths,
    store::{self, Store},
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;
const ID: &str = "11111111-1111-4111-8111-111111111111";
fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
fn command(root: &Path, socket: &str, args: &[&str]) -> Output {
    Command::new(binary())
        .env("TMUX", format!("{socket},1,0"))
        .arg("--data-dir")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}
fn value(root: &Path, socket: &str, args: &[&str]) -> Value {
    let o = command(root, socket, args);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}
fn tmux(socket: &str, args: &[&str]) -> String {
    let o = Command::new("tmux")
        .args(["-S", socket])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().into()
}
#[derive(Default)]
struct Server {
    limits: Option<Value>,
    threads: BTreeMap<String, Value>,
    started_profiles: BTreeMap<String, Value>,
    calls: Vec<(String, Value)>,
    background: bool,
    fail_resume: bool,
    resume_system_error: bool,
    ignore_full_access: bool,
    fail_name_set: bool,
    fail_resume_cwd: Option<PathBuf>,
    fail_unarchive: bool,
    fail_archive: bool,
    queue_after_reads: Option<usize>,
    queue_reads: usize,
    hold_busy: bool,
    reject_continuation: bool,
    lose_continuation_reply: bool,
    history_mismatch: bool,
    history_timings: bool,
    history_content_mismatch: bool,
    fail_delete: bool,
    repeat_history_cursor: bool,
    delay_loaded: bool,
    delayed_loaded: usize,
}
fn find_rollout(home: &Path, id: &str) -> Option<PathBuf> {
    fn walk(p: &Path, id: &str) -> Option<PathBuf> {
        for e in fs::read_dir(p).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(p) = walk(&p, id) {
                    return Some(p);
                }
            } else if p.file_name()?.to_string_lossy().contains(id) {
                return Some(p);
            }
        }
        None
    }
    walk(&home.join("sessions"), id).or_else(|| walk(&home.join("archived_sessions"), id))
}
fn history(home: &Path, cwd: &Path, id: &str) -> PathBuf {
    let p = home.join(format!("sessions/2026/10/09/rollout-{id}.jsonl"));
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p,format!("{}\n{}\n{}\n",json!({"type":"session_meta","payload":{"id":id,"cwd":cwd,"source":"cli"}}),json!({"type":"turn_context","payload":{"approval_policy":"on-request","sandbox_policy":{"type":"workspace-write"}}}),json!({"type":"event_msg","payload":{"type":"task_complete"}}))).unwrap();
    p
}
fn thread_value(path: &Path, id: &str, status: &str) -> Value {
    let text = fs::read_to_string(path).unwrap();
    let m: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    json!({"id":id,"path":path,"cwd":m["payload"]["cwd"],"status":{"type":status},"historyMode":m["payload"]["history_mode"].as_str().unwrap_or("legacy"),"turns":[{"id":"turn1","status":if status=="active"{"inProgress"}else{"completed"}}]})
}
async fn server(home: PathBuf, state: Arc<Mutex<Server>>) {
    let path = home.join("app-server-control/app-server-control.sock");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(path).unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let home = home.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(raw))) = ws.next().await {
                let v: Value = serde_json::from_str(&raw).unwrap();
                if v.get("id").is_none() {
                    continue;
                }
                let m = v["method"].as_str().unwrap();
                let delayed = {
                    let mut s = state.lock().unwrap();
                    let delayed = m == "thread/loaded/list" && s.delay_loaded;
                    if delayed {
                        s.delayed_loaded += 1;
                    }
                    delayed
                };
                if delayed {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                let p = &v["params"];
                let id = p["threadId"].as_str().unwrap_or("");
                let response = {
                    let mut s = state.lock().unwrap();
                    s.calls.push((m.into(), p.clone()));
                    let result = match m {
                        "initialize" => json!({"userAgent":"fixture"}),
                        "account/read" => json!({"account":{"type":"chatgpt"}}),
                        "account/rateLimits/read" => {
                            s.limits.clone().unwrap_or_else(||json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":daemon::now()+18000},"secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":daemon::now()+604800}}}))
                        }
                        "thread/loaded/list" => {
                            json!({"data":s.threads.iter().filter(|(_,t)|t["status"]["type"]!="notLoaded").map(|(id,_)|id.clone()).collect::<Vec<_>>(),"nextCursor":null})
                        }
                        "thread/read" => {
                            if !s.threads.contains_key(id) {
                                if let Some(path) = find_rollout(&home, id) {
                                    s.threads
                                        .insert(id.into(), thread_value(&path, id, "notLoaded"));
                                }
                            }
                            json!({"thread":s.threads.get(id)})
                        }
                        "thread/start" => {
                            let id = uuid::Uuid::new_v4().to_string();
                            let path = history(&home, Path::new(p["cwd"].as_str().unwrap()), &id);
                            let t = thread_value(&path, &id, "idle");
                            // Native Codex defers the rollout until a durable operation.
                            fs::remove_file(&path).unwrap();
                            s.started_profiles
                                .insert(id.clone(), p["permissions"].clone());
                            s.threads.insert(id, t.clone());
                            json!({"thread":t})
                        }
                        "thread/name/set" if s.fail_name_set => json!({"fixture_error":true}),
                        "thread/name/set" => {
                            let t = s.threads.get_mut(id).unwrap();
                            history(&home, Path::new(t["cwd"].as_str().unwrap()), id);
                            t["name"] = p["name"].clone();
                            json!({})
                        }
                        "thread/turns/list" | "thread/items/list" => {
                            if m=="thread/turns/list" && p["sortDirection"]=="desc" {
                                json!({"data":s.threads.get(id).and_then(|t|t["turns"].as_array()).and_then(|a|a.last()).cloned().into_iter().collect::<Vec<_>>(),"nextCursor":null})
                            } else {
                            let count = if m == "thread/turns/list" {
                                70
                            } else if s.history_mismatch {
                                269
                            } else {
                                270
                            };
                            let start = p["cursor"]
                                .as_str()
                                .and_then(|s| s.parse::<usize>().ok())
                                .unwrap_or(0);
                            let end =
                                (start + p["limit"].as_u64().unwrap_or(64) as usize).min(count);
                            let mut data: Vec<_> = (start..end).map(|n| if m == "thread/turns/list" { json!({"id":format!("turn-{n}"),"status":"completed","items":[],"itemsView":"notLoaded"}) } else { json!({"turnId":format!("turn-{}", n / 4),"item":{"type":"agentMessage","id":format!("item-{n}"),"text":format!("fixture history {n}")}}) }).collect();
                            if m == "thread/items/list" {
                                for entry in &mut data {
                                    entry["startedAtMs"] = if s.history_timings {
                                        json!(1000)
                                    } else {
                                        Value::Null
                                    };
                                    entry["completedAtMs"] = if s.history_timings {
                                        json!(2000)
                                    } else {
                                        Value::Null
                                    };
                                    if s.history_content_mismatch {
                                        entry["item"]["text"] = json!("changed history");
                                    }
                                }
                            }
                            json!({"data":data,"nextCursor": if s.repeat_history_cursor { Some("0".into()) } else if end < count { Some(end.to_string()) } else { None }})
                            }
                        }
                        "thread/resume" => {
                            let stale_path = s.threads.get(id).is_some_and(|t| {
                                t["historyMode"] == "paginated"
                                    && p["path"].is_string()
                                    && t["path"] != p["path"]
                            });
                            if s.fail_resume
                                || find_rollout(&home, id).is_none()
                                || stale_path
                                || s.fail_resume_cwd
                                    .as_ref()
                                    .is_some_and(|cwd| p["cwd"].as_str() == cwd.to_str())
                            {
                                json!({"fixture_error":true})
                            } else {
                                let path = find_rollout(&home, id).unwrap();
                                let mut t = thread_value(&path, id, "idle");
                                if s.resume_system_error {
                                    t["status"]["type"] = json!("systemError");
                                    t["turns"] = json!([{"id":"failed-turn","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}]);
                                }
                                if p["cwd"].is_string() {
                                    t["cwd"] = p["cwd"].clone();
                                }
                                s.threads.insert(id.into(), t.clone());
                                let full = p["permissions"] == ":danger-full-access"
                                    && !s.ignore_full_access;
                                let mode = if full {
                                    "dangerFullAccess"
                                } else {
                                    match p["sandbox"].as_str() {
                                        Some("danger-full-access") => "dangerFullAccess",
                                        Some("read-only") => "readOnly",
                                        _ => "workspaceWrite",
                                    }
                                };
                                // Codex retains the creation profile when resuming a
                                // still-loaded thread, even if resume requests another.
                                let profile =
                                    s.started_profiles.get(id).cloned().unwrap_or_else(|| {
                                        if full {
                                            json!(":danger-full-access")
                                        } else {
                                            Value::Null
                                        }
                                    });
                                json!({"thread":t,"approvalPolicy":p["approvalPolicy"],"sandbox":{"type":mode},
                                    "activePermissionProfile":if profile.is_string() { json!({"id":profile}) } else { Value::Null }})
                            }
                        }
                        "turn/start" if s.reject_continuation => json!({"fixture_error":true}),
                        "turn/start" => {
                            let t = s.threads.get_mut(id).unwrap();
                            t["status"]["type"] = json!("active");
                            t["turns"] = json!([{"id":"continued-turn","status":"inProgress","itemsView":"summary","items":[{"type":"userMessage","clientId":p["clientUserMessageId"]}]}]);
                            json!({"turn":{"id":"continued-turn","status":"inProgress"}})
                        }
                        "turn/steer" if s.hold_busy => json!({}),
                        "turn/steer" => {
                            let t = s.threads.get_mut(id).unwrap();
                            t["status"]["type"] = json!("idle");
                            if let Some(turns)=t["turns"].as_array_mut() {for turn in turns {turn["status"]=json!("completed");}}
                            let path = t["path"].as_str().unwrap();
                            writeln!(fs::OpenOptions::new().append(true).open(path).unwrap(),"{}",json!({"type":"event_msg","payload":{"type":"task_complete","message":p["input"]}})).unwrap();
                            json!({})
                        }
                        "thread/unarchive" if s.fail_unarchive => json!({"fixture_error":true}),
                        "thread/archive" if s.fail_archive => json!({"fixture_error":true}),
                        "thread/archive" | "thread/unarchive" => {
                            let t = s.threads.get_mut(id).unwrap();
                            let old = PathBuf::from(t["path"].as_str().unwrap());
                            let path = home
                                .join(if m == "thread/archive" {
                                    "archived_sessions"
                                } else {
                                    "sessions"
                                })
                                .join(old.file_name().unwrap());
                            fs::create_dir_all(path.parent().unwrap()).unwrap();
                            fs::rename(old, &path).unwrap();
                            t["path"] = json!(path);
                            t["status"]["type"] = json!("notLoaded");
                            json!({})
                        }
                        "thread/backgroundTerminals/list" => {
                            json!({"data":if s.background{vec![json!({"processId":"background"})]}else{vec![]},"nextCursor":null})
                        }
                        "thread/backgroundTerminals/terminate" => {
                            s.background = false;
                            json!({})
                        }
                        "thread/goal/get" => json!({"goal":null}),
                        "thread/delete" if s.fail_delete => json!({"fixture_error":true}),
                        "thread/delete" => {
                            if let Some(thread) = s.threads.remove(id) {
                                fs::remove_file(thread["path"].as_str().unwrap()).unwrap();
                            }
                            json!({})
                        }
                        "thread/queue/list" => {
                            s.queue_reads += 1;
                            json!({"data":if s.queue_after_reads.is_some_and(|n| s.queue_reads >= n) {
                                vec![json!({"id":"pending-input"})]
                            } else {vec![]},"nextCursor":null})
                        }
                        _ => json!({}),
                    };
                    if result["fixture_error"] == true
                        || (m == "thread/read" && result["thread"].is_null())
                    {
                        json!({"id":v["id"],"error":{"code":-1,"message":"fixture rejection"}})
                    } else {
                        json!({"id":v["id"],"result":result})
                    }
                };
                if m == "turn/start" && state.lock().unwrap().lose_continuation_reply {
                    break;
                }
                if ws
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}
struct Fixture {
    tmp: tempfile::TempDir,
    paths: Paths,
    socket: String,
    a: Arc<Mutex<Server>>,
    b: Arc<Mutex<Server>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = command(&self.paths.root, &self.socket, &["daemon", "stop"]);
        let _ = Command::new("tmux")
            .args(["-S", &self.socket, "kill-server"])
            .output();
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn automatic_job(&self) -> Value {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let jobs = self.invoke(&["moves"]);
            if let Some(job) = jobs.as_array().unwrap().first() {
                return job.clone();
            }
            assert!(Instant::now() < until, "automatic operation did not start");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let socket = tmp.path().join("tmux").to_str().unwrap().into();
        let c = tmp.path().join("ui.c");
        fs::write(&c,r#"#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <glob.h>
int main(int argc,char**argv){char path[4096];if(argc>1 && strcmp(argv[1],"app-server")==0){snprintf(path,sizeof(path),"%s/backend-restart",getenv("CODEX_HOME"));FILE*f=fopen(path,"w");fclose(f);return 0;}snprintf(path,sizeof(path),"%s/launch-args",getenv("CODEX_HOME"));FILE*f=fopen(path,"w");int remote=0,permissions=0;for(int i=1;i<argc;i++){fprintf(f,"%s\n",argv[i]);if(strcmp(argv[i],"--remote")==0)remote=1;if(strcmp(argv[i],"--sandbox")==0||strcmp(argv[i],"--ask-for-approval")==0||strcmp(argv[i],"--dangerously-bypass-approvals-and-sandbox")==0||strcmp(argv[i],"--approve-for-me")==0||strcmp(argv[i],"--full-auto")==0||strncmp(argv[i],"approval_policy=",16)==0||strncmp(argv[i],"sandbox_mode=",13)==0)permissions=1;}fclose(f);if(remote&&permissions){fprintf(stderr,"Permission overrides are not supported when resuming a remote task.\n");return 42;}if(argc>2&&strcmp(argv[1],"resume")==0&&strlen(argv[2])==36){glob_t g={0};snprintf(path,sizeof(path),"%s/sessions/**/rollout-*%s*.jsonl",getenv("CODEX_HOME"),argv[2]);int found=glob(path,0,NULL,&g)==0;globfree(&g);snprintf(path,sizeof(path),"%s/sessions/*/*/*/rollout-*%s*.jsonl",getenv("CODEX_HOME"),argv[2]);found|=glob(path,0,NULL,&g)==0;globfree(&g);snprintf(path,sizeof(path),"%s/sessions/rollout-*%s*.jsonl",getenv("CODEX_HOME"),argv[2]);found|=glob(path,0,NULL,&g)==0;globfree(&g);if(!found){fprintf(stderr,"no rollout found for thread id %s\n",argv[2]);return 43;}}for(;;)pause();}"#).unwrap();
        let bin = tmp.path().join("codex");
        assert!(
            Command::new("cc")
                .arg(&c)
                .arg("-o")
                .arg(&bin)
                .status()
                .unwrap()
                .success()
        );
        store::save_config(
            &paths,
            &Config {
                codex_binary: bin.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut state = State::default();
        for name in ["personal", "work"] {
            let home = paths.account(name).unwrap();
            fs::create_dir_all(&home).unwrap();
            fs::write(
                home.join("auth.json"),
                json!({"tokens":{"account_id":format!("fixture-{name}")}}).to_string(),
            )
            .unwrap();
            let mut account = Account::new(name.into(), String::new());
            account.usage = Usage {
                short: Some(Window {
                    used_percent: 10.,
                    duration_minutes: 300,
                    resets_at: daemon::now() + 18000,
                }),
                weekly: Some(Window {
                    used_percent: 20.,
                    duration_minutes: 10080,
                    resets_at: daemon::now() + 604800,
                }),
                observed_at: daemon::now(),
                ..Default::default()
            };
            state.accounts.insert(name.into(), account);
        }
        {
            let mut store = Store::open(&paths).unwrap();
            store.save(&state, daemon::now(), "fixture", "").unwrap();
        }
        let a = Arc::new(Mutex::new(Server::default()));
        let b = Arc::new(Mutex::new(Server::default()));
        let tasks = vec![
            tokio::spawn(server(paths.account("personal").unwrap(), a.clone())),
            tokio::spawn(server(paths.account("work").unwrap(), b.clone())),
        ];
        tokio::time::sleep(Duration::from_millis(50)).await;
        let f = Self {
            tmp,
            paths,
            socket,
            a,
            b,
            tasks,
        };
        let o = command(&f.paths.root, &f.socket, &["daemon", "start"]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        f
    }
    fn invoke(&self, args: &[&str]) -> Value {
        value(&self.paths.root, &self.socket, args)
    }
    fn live(&self, id: &str, busy: bool) -> String {
        self.live_in(id, busy, self.tmp.path())
    }
    fn live_in(&self, id: &str, busy: bool, cwd: &Path) -> String {
        let home = self.paths.account("personal").unwrap();
        let path = history(&home, cwd, id);
        self.a.lock().unwrap().threads.insert(
            id.into(),
            thread_value(&path, id, if busy { "active" } else { "idle" }),
        );
        tmux(
            &self.socket,
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-s",
                &format!("codex-fixture-{id}"),
                "-c",
                cwd.to_str().unwrap(),
                &format!(
                    "exec env CODEX_HOME={} {} resume --sandbox workspace-write --ask-for-approval on-request {}",
                    home.display(),
                    self.tmp.path().join("codex").display(),
                    id
                ),
            ],
        )
    }
    async fn wait(&self, id: &str, phase: &str) -> Value {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let jobs = self.invoke(&["moves"]);
            let j = jobs
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["id"] == id)
                .unwrap()
                .clone();
            if j["phase"] == phase
                && !(phase == "complete"
                    && j["resume_after_move"] == true
                    && matches!(j["continuation_delivery"].as_str(), Some("" | "sending")))
            {
                return j;
            }
            assert!(Instant::now() < until, "expected {phase}, got {j}");
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn experimental_rotation_moves_busy_work_with_explicit_consent() {
    let f = Fixture::new().await;
    f.live(ID, true);
    f.invoke(&[
        "thresholds",
        "personal",
        "--multiplier",
        "1",
        "--short",
        "5",
        "--weekly",
        "90",
    ]);
    f.invoke(&["thresholds", "work", "--multiplier", "1"]);
    let rejected = command(
        &f.paths.root,
        &f.socket,
        &[
            "auto-switch",
            "--thread",
            ID,
            "--accounts",
            "personal,work",
            "--enable",
        ],
    );
    assert!(!rejected.status.success());
    assert!(f.invoke(&["moves"]).as_array().unwrap().is_empty());
    let enabled = f.invoke(&[
        "auto-switch",
        "--thread",
        ID,
        "--accounts",
        "personal,work",
        "--enable",
        "--accept-experimental",
    ]);
    assert_eq!(enabled["active"], true);
    let job = f.automatic_job().await;
    assert_eq!(job["kind"], "account");
    assert_eq!(job["target"], "work");
    assert_eq!(job["pause_approved"], true);
    let done = f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert_eq!(done["continuation_delivery"], "accepted");
    let disabled = f.invoke(&["auto-switch", "--thread", ID, "--disable"]);
    assert_eq!(disabled["active"], false);
    assert_eq!(
        f.a.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "turn/steer")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn experimental_exhaustion_pauses_once_and_keeps_terminal_open() {
    let f = Fixture::new().await;
    let pane = f.live(ID, true);
    let pid = tmux(
        &f.socket,
        &["display-message", "-p", "-t", &pane, "#{pane_pid}"],
    );
    for account in ["personal", "work"] {
        f.invoke(&[
            "thresholds",
            account,
            "--multiplier",
            "1",
            "--short",
            "5",
            "--weekly",
            "90",
        ]);
    }
    f.invoke(&[
        "auto-switch",
        "--thread",
        ID,
        "--accounts",
        "personal,work",
        "--enable",
        "--accept-experimental",
    ]);
    let job = f.automatic_job().await;
    assert_eq!(job["kind"], "pause");
    let done = f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert_eq!(done["resume_after_move"], false);
    assert_eq!(
        pid,
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
        )
    );
    assert_eq!(f.invoke(&["auto-switch", "--thread", ID])["active"], false);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(f.invoke(&["moves"]).as_array().unwrap().len(), 1);
    assert!(
        !f.b.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "thread/resume" || m == "turn/start")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn experimental_disable_prevents_threshold_actions() {
    let f = Fixture::new().await;
    f.live(ID, true);
    for account in ["personal", "work"] {
        f.invoke(&["thresholds", account, "--multiplier", "1"]);
    }
    f.invoke(&[
        "auto-switch",
        "--thread",
        ID,
        "--accounts",
        "personal,work",
        "--enable",
        "--accept-experimental",
    ]);
    f.invoke(&["auto-switch", "--thread", ID, "--disable"]);
    f.invoke(&["thresholds", "personal", "--short", "5", "--weekly", "5"]);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(f.invoke(&["moves"]).as_array().unwrap().is_empty());
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/steer")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn experimental_uncertain_continuation_suspends_without_replaying() {
    let f = Fixture::new().await;
    f.live(ID, true);
    f.b.lock().unwrap().reject_continuation = true;
    f.invoke(&[
        "thresholds",
        "personal",
        "--multiplier",
        "1",
        "--short",
        "5",
        "--weekly",
        "90",
    ]);
    f.invoke(&["thresholds", "work", "--multiplier", "1"]);
    f.invoke(&[
        "auto-switch",
        "--thread",
        ID,
        "--accounts",
        "personal,work",
        "--enable",
        "--accept-experimental",
    ]);
    let job = f.automatic_job().await;
    f.wait(job["id"].as_str().unwrap(), "complete").await;
    let until = Instant::now() + Duration::from_secs(12);
    loop {
        let status = f.invoke(&["auto-switch", "--thread", ID]);
        if status["active"] == false {
            assert!(status["policy"]["error"].is_string());
            break;
        }
        assert!(
            Instant::now() < until,
            "automation should suspend after uncertain continuation"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(f.invoke(&["moves"]).as_array().unwrap().len(), 1);
    assert_eq!(
        f.b.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "turn/start")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_move_requires_consent_preserves_permissions_and_can_move_again() {
    let f = Fixture::new().await;
    let pane = f.live(ID, true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.a.lock().unwrap().background = true;
    let list = f.invoke(&["sessions"]);
    assert_eq!(list["schema_version"], 1);
    assert_eq!(list["sessions"][0]["activity"], "busy");
    assert_eq!(list["sessions"][0]["selectable"], true);
    assert_eq!(list["sessions"][0]["display_account"], "personal");
    let j = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    let id = j["id"].as_str().unwrap();
    let again = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    assert_eq!(again["id"], id);
    let prompt = f.wait(id, "awaiting_pause_confirmation").await;
    assert!(
        prompt["detail"]
            .as_str()
            .unwrap()
            .contains("automatically ask Codex to resume")
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/steer")
    );
    assert_eq!(
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_dead}"]
        ),
        "0"
    );
    let seq = prompt["confirmation_seq"].to_string();
    f.invoke(&["move-response", id, "yes", "--sequence", &seq]);
    let next = f.wait(id, "awaiting_background_approval").await;
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &["move-response", id, "yes", "--sequence", &seq]
        )
        .status
        .success()
    );
    f.invoke(&[
        "move-response",
        id,
        "yes",
        "--sequence",
        &next["confirmation_seq"].to_string(),
    ]);
    let done = f.wait(id, "complete").await;
    assert_eq!(done["continuation_delivery"], "accepted");
    {
        let target = f.b.lock().unwrap();
        let starts: Vec<_> = target
            .calls
            .iter()
            .filter(|(m, _)| m == "turn/start")
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].1["threadId"], ID);
        assert!(
            starts[0].1["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Pick up where you left off")
        );
        assert_eq!(target.threads[ID]["status"]["type"], "active");
    }
    assert!(
        !command(&f.paths.root, &f.socket, &["handoff-worker", "--job", id])
            .status
            .success()
    );
    assert_eq!(
        f.a.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "turn/steer")
            .count(),
        1
    );
    assert_eq!(
        f.b.lock()
            .unwrap()
            .calls
            .iter()
            .find(|(m, _)| m == "thread/resume")
            .unwrap()
            .1["sandbox"],
        "workspace-write"
    );
    let list = f.invoke(&["sessions", "--all"]);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(list["sessions"][0]["account"], "work");
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &["move", "--pane", &pane, "--to", "work"]
        )
        .status
        .success()
    );
    // Allow archive history lookup for the return move: original server knows notLoaded.
    f.b.lock().unwrap().threads.get_mut(ID).unwrap()["status"]["type"] = json!("idle");
    let j = f.invoke(&["move", "--pane", &pane, "--to", "personal"]);
    let idle_move = f.wait(j["id"].as_str().unwrap(), "complete").await;
    assert_eq!(idle_move["resume_after_move"], false);
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "turn/start")
    );
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["account"],
        "personal"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_failure_never_replays_or_rolls_back_a_completed_move() {
    for mode in ["rejected", "lost_reply", "failed_move"] {
        let f = Fixture::new().await;
        let pane = f.live(ID, true);
        {
            let mut target = f.b.lock().unwrap();
            target.reject_continuation = mode == "rejected";
            target.lose_continuation_reply = mode == "lost_reply";
            target.fail_resume = mode == "failed_move";
        }
        let job = f.invoke(&["move", "--pane", &pane, "--to", "work", "--pause"]);
        let id = job["id"].as_str().unwrap();
        let done = f
            .wait(
                id,
                if mode == "failed_move" {
                    "recovered"
                } else {
                    "complete"
                },
            )
            .await;
        if mode != "failed_move" {
            assert_eq!(done["continuation_delivery"], "unconfirmed");
            assert!(
                done["detail"]
                    .as_str()
                    .unwrap()
                    .contains("will not be retried automatically")
            );
            assert_eq!(f.invoke(&["sessions"])["sessions"][0]["account"], "work");
            assert!(
                !command(&f.paths.root, &f.socket, &["recover", id])
                    .status
                    .success()
            );
        } else {
            assert_eq!(
                f.invoke(&["sessions"])["sessions"][0]["account"],
                "personal"
            );
        }
        assert!(
            !command(&f.paths.root, &f.socket, &["handoff-worker", "--job", id])
                .status
                .success()
        );
        let target = f.b.lock().unwrap();
        assert_eq!(
            target
                .calls
                .iter()
                .filter(|(m, _)| m == "turn/start")
                .count(),
            usize::from(mode != "failed_move")
        );
        if mode == "lost_reply" {
            assert_eq!(target.threads[ID]["status"]["type"], "active");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_and_decline_leave_busy_source_running() {
    let f = Fixture::new().await;
    let pane = f.live(ID, true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    for decline in [false, true] {
        let j = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
        let id = j["id"].as_str().unwrap();
        f.wait(id, "awaiting_pause_confirmation").await;
        if decline {
            f.invoke(&["move-response", id, "no"]);
        } else {
            f.invoke(&["cancel-move", id]);
        }
        f.wait(id, "cancelled").await;
    }
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/steer")
    );
    assert_eq!(
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_dead}"]
        ),
        "0"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inactive_moves_start_a_discoverable_pane() {
    let f = Fixture::new().await;
    history(&f.paths.account("personal").unwrap(), f.tmp.path(), ID);
    let j = f.invoke(&["move", "--thread", ID, "--to", "work"]);
    let done = f.wait(j["id"].as_str().unwrap(), "complete").await;
    assert!(done["source"]["pane"].as_str().unwrap().starts_with('%'));
    let list = f.invoke(&["sessions", "--all"]);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(list["sessions"][0]["account"], "work");
}

fn paginated(f: &Fixture, id: &str) {
    let path = find_rollout(&f.paths.account("personal").unwrap(), id).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    let (first, rest) = text.split_once('\n').unwrap();
    let mut meta: Value = serde_json::from_str(first).unwrap();
    meta["payload"]["history_mode"] = json!("paginated");
    fs::write(&path, format!("{meta}\n{rest}")).unwrap();
    f.a.lock().unwrap().threads.get_mut(id).unwrap()["historyMode"] = json!("paginated");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paginated_history_is_verified_across_pages_and_can_move_back() {
    let f = Fixture::new().await;
    f.b.lock().unwrap().history_timings = true;
    let pane = f.live(ID, false);
    paginated(&f, ID);
    let original =
        fs::read(find_rollout(&f.paths.account("personal").unwrap(), ID).unwrap()).unwrap();
    let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    let done = f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert_eq!(done["history_checks"][0]["turns"], 70);
    assert_eq!(done["history_checks"][0]["items"], 270);
    assert_eq!(
        original,
        fs::read(find_rollout(&f.paths.account("work").unwrap(), ID).unwrap()).unwrap()
    );
    assert!(
        !serde_json::to_string(&done)
            .unwrap()
            .contains("fixture history")
    );
    let back = f.invoke(&["move", "--pane", &pane, "--to", "personal"]);
    f.wait(back["id"].as_str().unwrap(), "complete").await;
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["account"],
        "personal"
    );
    for server in [&f.a, &f.b] {
        assert!(
            !server
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "turn/start")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn equal_history_counts_with_changed_content_restore_source() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    paginated(&f, ID);
    f.b.lock().unwrap().history_content_mismatch = true;
    let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    let recovered = f.wait(job["id"].as_str().unwrap(), "recovered").await;
    let detail = recovered["detail"].as_str().unwrap();
    assert!(detail.contains("expected 70 turns / 270 items; found 70 / 270"));
    assert!(detail.contains("content or order differs"));
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["account"],
        "personal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_history_restores_source_and_retry_checks_again() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    paginated(&f, ID);
    f.b.lock().unwrap().history_mismatch = true;
    let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    let recovered = f.wait(job["id"].as_str().unwrap(), "recovered").await;
    assert!(
        recovered["detail"]
            .as_str()
            .unwrap()
            .contains("did not match")
    );
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["account"],
        "personal"
    );
    f.b.lock().unwrap().history_mismatch = false;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let retried = f.invoke(&["retry", job["id"].as_str().unwrap()]);
    assert_ne!(retried["id"], job["id"]);
    assert_eq!(retried["pause_approved"], false);
    assert_eq!(retried["risks_approved"], false);
    f.wait(retried["id"].as_str().unwrap(), "complete").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paginated_child_history_is_verified_and_left_unloaded() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    paginated(&f, ID);
    let child_id = "22222222-2222-4222-8222-222222222222";
    let path = history(
        &f.paths.account("personal").unwrap(),
        f.tmp.path(),
        child_id,
    );
    let text = fs::read_to_string(&path).unwrap();
    let (first, rest) = text.split_once('\n').unwrap();
    let mut meta: Value = serde_json::from_str(first).unwrap();
    meta["payload"]["source"] = json!({"subagent":{"parent_thread_id":ID}});
    meta["payload"]["history_mode"] = json!("paginated");
    fs::write(&path, format!("{meta}\n{rest}")).unwrap();
    f.a.lock()
        .unwrap()
        .threads
        .insert(child_id.into(), thread_value(&path, child_id, "notLoaded"));
    let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    let id = job["id"].as_str().unwrap();
    let prompt = f.wait(id, "awaiting_risk_confirmation").await;
    f.invoke(&[
        "move-response",
        id,
        "yes",
        "--sequence",
        &prompt["confirmation_seq"].to_string(),
    ]);
    let done = f.wait(id, "complete").await;
    assert_eq!(done["history_checks"].as_array().unwrap().len(), 2);
    assert_eq!(done["prepared_threads"], json!([child_id]));
    assert_eq!(
        f.b.lock().unwrap().threads[child_id]["status"]["type"],
        "notLoaded"
    );
    assert!(find_rollout(&f.paths.account("work").unwrap(), child_id).is_some());
    assert!(
        !f.b.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/start")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broken_history_pagination_leaves_original_process_untouched() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    paginated(&f, ID);
    f.a.lock().unwrap().repeat_history_cursor = true;
    let pid = tmux(
        &f.socket,
        &["display-message", "-p", "-t", &pane, "#{pane_pid}"],
    );
    let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    let failed = f.wait(job["id"].as_str().unwrap(), "failed").await;
    assert!(failed["detail"].as_str().unwrap().contains("pagination"));
    assert_eq!(
        pid,
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
        )
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "thread/archive")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_order_is_persistent_and_does_not_follow_background_writes() {
    let f = Fixture::new().await;
    let first = f.live(ID, false);
    let other = "22222222-2222-4222-8222-222222222222";
    let second = f.live(other, false);
    f.invoke(&["open", "--session", &format!("pane:{first}")]);
    f.invoke(&["open", "--session", &format!("pane:{second}")]);
    let rows = f.invoke(&["sessions"]);
    assert_eq!(rows["sessions"][0]["thread_id"], other);
    assert!(
        rows["sessions"][0]["last_accessed"].as_i64().unwrap()
            > rows["sessions"][1]["last_accessed"].as_i64().unwrap()
    );
    let path = find_rollout(&f.paths.account("personal").unwrap(), ID).unwrap();
    writeln!(
        fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{{\"type\":\"usage\"}}"
    )
    .unwrap();
    assert_eq!(f.invoke(&["sessions"])["sessions"][0]["thread_id"], other);
    f.invoke(&["open", "--session", &format!("pane:{first}")]);
    assert_eq!(f.invoke(&["sessions"])["sessions"][0]["thread_id"], ID);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_and_archive_recovery_use_original_external_home_without_a_turn() {
    let mut f = Fixture::new().await;
    let home = f.tmp.path().join("default-home");
    let path = history(&home, f.tmp.path(), ID);
    let native = Arc::new(Mutex::new(Server::default()));
    let mut thread = thread_value(&path, ID, "notLoaded");
    thread["historyMode"] = json!("paginated");
    native.lock().unwrap().threads.insert(ID.into(), thread);
    f.tasks
        .push(tokio::spawn(server(home.clone(), native.clone())));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let invoke = |args: &[&str]| {
        let o = Command::new(binary())
            .env("CODEX_HOME", &home)
            .env("TMUX", format!("{},1,0", f.socket))
            .arg("--data-dir")
            .arg(&f.paths.root)
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice::<Value>(&o.stdout).unwrap()
    };
    let saved = invoke(&["sessions", "--all"]);
    assert_eq!(saved["sessions"][0]["thread_id"], ID);
    assert_eq!(saved["sessions"][0]["account"], "external");
    let opened = invoke(&[
        "open",
        "--session",
        &format!("thread:{ID}"),
        "--name",
        "restored",
    ]);
    let pane = opened["session"]["pane"].as_str().unwrap();
    let again = invoke(&["open", "--session", &format!("pane:{pane}")]);
    assert_eq!(again, opened);
    assert_eq!(
        invoke(&["open", "--session", &format!("thread:{ID}")]),
        opened
    );
    let list = invoke(&["sessions", "--all"]);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(list["sessions"][0]["activity"], "idle");
    assert_eq!(list["sessions"][0]["home"], home.to_str().unwrap());
    // Dispose only the inert test TUI, then archive via the synthetic server.
    assert_eq!(opened["session"]["tmux_session"], "codex-restored");
    assert_eq!(list["sessions"][0]["name"], "codex-restored");
    tmux(&f.socket, &["kill-session", "-t", "=codex-restored"]);
    let mut control = sessionsation::control::Control::connect(&home)
        .await
        .unwrap();
    control
        .request("thread/archive", json!({"threadId":ID}))
        .await
        .unwrap();
    assert_eq!(invoke(&["sessions", "--all"])["sessions"], json!([]));
    let archives = invoke(&["archives"]);
    let key = archives["sessions"][0]["id"].as_str().unwrap();
    assert!(key.starts_with("archive:"));
    assert_eq!(archives["sessions"][0]["archived"], true);
    invoke(&["open", "--session", key, "--name", "recovered"]);
    assert_eq!(invoke(&["archives"])["sessions"], json!([]));
    let calls = &native.lock().unwrap().calls;
    assert!(calls.iter().any(|(m, _)| m == "thread/unarchive"));
    assert!(
        calls
            .iter()
            .filter(|(m, _)| m == "thread/resume")
            .all(|(_, p)| p.get("path").is_none())
    );
    assert!(!calls.iter().any(|(m, _)| matches!(
        m.as_str(),
        "turn/start" | "turn/steer" | "thread/queue/start"
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn main_menu_launch_and_saved_start_use_the_real_switcher_contract() {
    let Ok(menu) = std::env::var("SWITCHER_MENU_REPO") else {
        return;
    };
    let f = Fixture::new().await;
    let bin = f.tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(binary(), bin.join("codex-switcher")).unwrap();
    let script = r#"
source "$MENU_REPO/init.bash"
ez_codex_attach() { printf '%s\n' "$1" > "$RESULT_PATH"; }
ez_codex_choose_account() { printf personal; }
ez_codex_field() { if [[ $1 == 'Session name' ]]; then printf menu-project; else printf '%s' "$PROJECT_PATH"; fi; }
if [[ $MODE == new ]]; then ez_codex_new
else
  ez_codex_inventory || exit 21
  row=$(jq -c --arg id "$THREAD_ID" '.sessions[] | select(.thread_id == $id)' <<< "$ez_codex_snapshot")
  [[ -n $row ]] || exit 22
  ez_codex_start "$row"
fi
"#;
    let menu_run = |mode: &str, thread: &str| {
        let out = Command::new("bash")
            .args(["-c", script])
            .env("MENU_REPO", &menu)
            .env("HOME", f.tmp.path())
            .env("XDG_STATE_HOME", f.tmp.path().join("menu-state"))
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("TMUX", format!("{},1,0", f.socket))
            .env("CODEX_SWITCHER_HOME", &f.paths.root)
            .env("CODEX_HOME", f.tmp.path().join("unused-default"))
            .env("MODE", mode)
            .env("THREAD_ID", thread)
            .env("PROJECT_PATH", f.tmp.path())
            .env("RESULT_PATH", f.tmp.path().join("attached"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    menu_run("new", "");
    let mut first = json!({});
    for _ in 0..50 {
        let list = f.invoke(&["sessions"]);
        if let Some(row) = list["sessions"].as_array().unwrap().first() {
            first = row.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let thread = first["thread_id"]
        .as_str()
        .expect("main menu created a managed Codex session");
    let job = f.invoke(&["terminate", "--pane", first["pane"].as_str().unwrap()]);
    f.wait(job["id"].as_str().unwrap(), "complete").await;
    menu_run("saved", thread);
    let list = f.invoke(&["sessions", "--all"]);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(list["sessions"][0]["thread_id"], thread);
    assert_eq!(list["sessions"][0]["account"], "personal");
    assert_eq!(list["sessions"][0]["activity"], "idle");
    assert_eq!(
        fs::read_to_string(f.tmp.path().join("attached"))
            .unwrap()
            .trim(),
        "=codex-menu-project"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_launches_share_account_server_and_register_exact_threads() {
    let f = Fixture::new().await;
    f.invoke(&["config", "set", "short_ceiling", "10"]);
    assert_eq!(
        f.invoke(&["status", "--json"])["accounts"][0]["eligible"],
        false
    );
    let legacy_lease = sessionsation::paths::lock_file(
        &f.paths.account("personal").unwrap().join(".switcher-lease"),
    )
    .unwrap();
    fs2::FileExt::lock_exclusive(&legacy_lease).unwrap();
    for (index, options) in [
        "--sandbox read-only --ask-for-approval on-request",
        "--dangerously-bypass-approvals-and-sandbox",
    ]
    .iter()
    .enumerate()
    {
        tmux(
            &f.socket,
            &[
                "new-session",
                "-d",
                "-s",
                &format!("codex-new-{index}"),
                "-c",
                f.tmp.path().to_str().unwrap(),
                &format!(
                    "exec {} --data-dir {} run --account personal -- --model fixture-model {options}",
                    binary(),
                    f.paths.root.display()
                ),
            ],
        );
    }
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let list = f.invoke(&["sessions"]);
        if list["sessions"].as_array().unwrap().len() == 2 {
            assert_ne!(
                list["sessions"][0]["thread_id"],
                list["sessions"][1]["thread_id"]
            );
            break;
        }
        assert!(Instant::now() < until, "{list}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        f.a.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "thread/start")
            .count(),
        2
    );
    let server = f.a.lock().unwrap();
    for (index, name) in ["new-0", "new-1"].iter().enumerate() {
        let (_, named) = server
            .calls
            .iter()
            .find(|(method, params)| method == "thread/name/set" && params["name"] == *name)
            .unwrap();
        let id = named["threadId"].as_str().unwrap();
        assert!(find_rollout(&f.paths.account("personal").unwrap(), id).is_some());
        let resumed = server
            .calls
            .iter()
            .find(|(method, params)| method == "thread/resume" && params["threadId"] == id)
            .unwrap();
        assert_eq!(resumed.1["model"], "fixture-model");
        assert_eq!(
            resumed.1["approvalPolicy"],
            if index == 0 { "on-request" } else { "never" }
        );
        if index == 0 {
            assert_eq!(resumed.1["sandbox"], "read-only");
        } else {
            assert_eq!(resumed.1["permissions"], ":danger-full-access");
            assert!(resumed.1.get("sandbox").is_none());
        }
        assert_eq!(resumed.1["excludeTurns"], true);
        assert!(resumed.1.get("historyMode").is_none());
    }
    assert!(
        !server
            .calls
            .iter()
            .any(|(method, _)| method == "turn/start")
    );
    for (approval, sandbox) in [("on-request", "read-only"), ("never", "danger-full-access")] {
        assert!(
            server
                .calls
                .iter()
                .any(|(method, params)| method == "thread/start"
                    && params["approvalPolicy"] == approval
                    && if sandbox == "danger-full-access" {
                        params["permissions"] == ":danger-full-access"
                            && params.get("sandbox").is_none()
                    } else {
                        params["sandbox"] == sandbox && params.get("permissions").is_none()
                    })
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_launch_does_not_attach_when_saving_or_resume_verification_fails() {
    for fail_name in [true, false] {
        let f = Fixture::new().await;
        {
            let mut server = f.a.lock().unwrap();
            server.fail_name_set = fail_name;
            server.fail_resume = !fail_name;
        }
        let error_log = f.tmp.path().join("launch-error.log");
        let pane = tmux(
            &f.socket,
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-x",
                "160",
                "-y",
                "30",
                &format!(
                    "tmux set-option -p remain-on-exit on; exec {} --data-dir {} run --account personal 2>{}",
                    binary(),
                    f.paths.root.display(),
                    error_log.display()
                ),
            ],
        );
        let until = Instant::now() + Duration::from_secs(8);
        while tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_dead}"],
        ) != "1"
        {
            assert!(
                Instant::now() < until,
                "launch should exit before attaching"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let error = fs::read_to_string(&error_log).unwrap();
        assert!(
            error.contains(if fail_name {
                "could not save the new conversation"
            } else {
                "new conversation was saved but Codex could not reopen it"
            }),
            "{error}"
        );
        assert!(
            !f.paths
                .account("personal")
                .unwrap()
                .join("launch-args")
                .exists()
        );
        assert!(
            !f.paths
                .root
                .join("managed-panes")
                .join(format!("{}.json", pane.trim_start_matches('%')))
                .exists()
        );
        assert_eq!(
            f.a.lock()
                .unwrap()
                .calls
                .iter()
                .filter(|(m, _)| m == "thread/start")
                .count(),
            1
        );
        assert!(
            !f.a.lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "turn/start")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_keeps_queued_input_then_accepts_a_terminal_error_after_it_is_cleared() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    paginated(&f, ID);
    {
        let mut server = f.b.lock().unwrap();
        server.history_mismatch = true;
        server.resume_system_error = true;
        server.queue_after_reads = Some(1);
    }
    let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    let id = job["id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let jobs = f.invoke(&["moves"]);
        let current = jobs
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["id"] == id)
            .unwrap();
        if current["detail"]
            .as_str()
            .unwrap()
            .contains("Automatic recovery could not finish")
        {
            assert!(current["detail"].as_str().unwrap().contains("queued input"));
            assert_eq!(current["target_loaded"], true);
            break;
        }
        assert!(Instant::now() < until, "{current}");
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    {
        let mut server = f.b.lock().unwrap();
        assert!(
            !server
                .calls
                .iter()
                .any(|(method, _)| method == "thread/archive")
        );
        server.queue_after_reads = None;
    }
    loop {
        let recovered = command(&f.paths.root, &f.socket, &["recover", id]);
        if recovered.status.success() {
            break;
        }
        assert!(
            Instant::now() < until,
            "{}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["account"],
        "personal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_stop_rechecks_queued_input_and_recovery_handles_rejected_archive() {
    for late_queue in [true, false] {
        let f = Fixture::new().await;
        let pane = f.live(ID, false);
        let pid = tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_pid}"],
        );
        {
            let mut server = f.a.lock().unwrap();
            // The two wait_idle passes see an empty queue; input arrives only
            // at the final stop boundary, after history preparation.
            server.queue_after_reads = late_queue.then_some(3);
            server.fail_archive = !late_queue;
        }
        let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
        let done = f
            .wait(
                job["id"].as_str().unwrap(),
                if late_queue { "failed" } else { "recovered" },
            )
            .await;
        if late_queue {
            assert!(done["detail"].as_str().unwrap().contains("queued input"));
        }
        assert_eq!(
            tmux(
                &f.socket,
                &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
            ),
            pid
        );
        let server = f.a.lock().unwrap();
        assert!(
            !server
                .calls
                .iter()
                .any(|(method, _)| method == "thread/unarchive")
        );
        if late_queue {
            assert!(
                !server
                    .calls
                    .iter()
                    .any(|(method, _)| method == "thread/archive")
            );
        }
        assert!(
            !f.paths
                .account("work")
                .unwrap()
                .join("launch-args")
                .exists()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_resume_preserves_saved_location_and_permission_mode() {
    for approval in ["untrusted", "never"] {
        let f = Fixture::new().await;
        let home = f.paths.account("personal").unwrap();
        let old_cwd = f.tmp.path().join("old-project");
        let new_cwd = f.tmp.path().join("new-project");
        fs::create_dir(&old_cwd).unwrap();
        fs::create_dir(&new_cwd).unwrap();
        let rollout = history(&home, &old_cwd, ID);
        let sandbox = if approval == "never" {
            "danger-full-access"
        } else {
            "workspace-write"
        };
        writeln!(fs::OpenOptions::new().append(true).open(&rollout).unwrap(), "{}",
            json!({"type":"turn_context","payload":{"approval_policy":approval,"sandbox_policy":{"type":sandbox}}})).unwrap();
        fs::create_dir_all(f.paths.root.join("thread-locations")).unwrap();
        fs::write(
            f.paths
                .root
                .join("thread-locations")
                .join(format!("{ID}.json")),
            serde_json::to_vec(&json!({"cwd":new_cwd})).unwrap(),
        )
        .unwrap();
        tmux(
            &f.socket,
            &[
                "new-session",
                "-d",
                "-s",
                "codex-resume",
                "-c",
                f.tmp.path().to_str().unwrap(),
                &format!(
                    "exec {} --data-dir {} run --account personal --thread {ID}",
                    binary(),
                    f.paths.root.display()
                ),
            ],
        );
        let until = Instant::now() + Duration::from_secs(10);
        while !home.join("launch-args").exists() {
            assert!(Instant::now() < until, "resume did not open");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let server = f.a.lock().unwrap();
        let params = &server
            .calls
            .iter()
            .find(|(method, _)| method == "thread/resume")
            .unwrap()
            .1;
        assert_eq!(params["cwd"], json!(new_cwd));
        assert_eq!(params["approvalPolicy"], approval);
        if approval == "never" {
            assert_eq!(params["permissions"], ":danger-full-access");
        } else {
            assert_eq!(params["sandbox"], "workspace-write");
        }
        assert!(
            !server
                .calls
                .iter()
                .any(|(method, _)| method == "turn/start")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_destination_restores_original_history_and_pane() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    f.b.lock().unwrap().fail_resume = true;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let j = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    f.wait(j["id"].as_str().unwrap(), "recovered").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["account"],
        "personal"
    );
    assert!(find_rollout(&f.paths.account("personal").unwrap(), ID).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_failed_live_turn_can_move_without_sending_an_unusable_pause() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    let rollout = find_rollout(&f.paths.account("personal").unwrap(), ID).unwrap();
    writeln!(
        fs::OpenOptions::new().append(true).open(&rollout).unwrap(),
        "{}",
        json!({"type":"event_msg","payload":{"type":"task_started"}})
    )
    .unwrap();
    {
        let mut server = f.a.lock().unwrap();
        let thread = server.threads.get_mut(ID).unwrap();
        thread["status"]["type"] = json!("systemError");
        thread["turns"] = json!([{"id":"turn1","status":"failed",
            "error":{"message":"Usage limit reached","codexErrorInfo":"usageLimitExceeded"}}]);
    }
    let list = f.invoke(&["sessions"]);
    assert_eq!(list["sessions"][0]["activity"], "idle");
    assert!(
        list["sessions"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("last turn failed")
    );
    let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "turn/steer")
    );
    assert_eq!(f.invoke(&["sessions"])["sessions"][0]["account"], "work");
    let target = f.b.lock().unwrap();
    let submissions: Vec<_> = target
        .calls
        .iter()
        .filter(|(method, _)| method == "turn/start")
        .collect();
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].1["clientUserMessageId"], job["id"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_continuation_distinguishes_failure_from_completed_interrupted_and_user_stop() {
    for (status, stopped, resume) in [
        ("failed", false, true),
        ("failed", true, false),
        ("completed", false, false),
        ("interrupted", false, false),
    ] {
        let f = Fixture::new().await;
        let pane = f.live(ID, false);
        f.a.lock().unwrap().threads.get_mut(ID).unwrap()["turns"] =
            json!([{"id":"last-turn","status":status,"itemsView":"summary","items":[]}]);
        if stopped {
            fs::create_dir_all(f.paths.root.join("conversations")).unwrap();
            fs::write(
                f.paths
                    .root
                    .join("conversations")
                    .join(format!("{ID}.json")),
                serde_json::to_vec(&json!({"stop_intent":"explicit-user-stop"})).unwrap(),
            )
            .unwrap();
        }
        let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
        let done = f.wait(job["id"].as_str().unwrap(), "complete").await;
        assert_eq!(
            done["resume_after_move"], resume,
            "{status}, stopped={stopped}"
        );
        assert_eq!(
            f.b.lock()
                .unwrap()
                .calls
                .iter()
                .filter(|(method, _)| method == "turn/start")
                .count(),
            usize::from(resume)
        );
        if resume {
            assert_eq!(done["continuation_turn_id"], "continued-turn");
            assert_eq!(done["continuation_delivery"], "accepted");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_access_moves_restore_the_named_profile_and_reject_downgrades() {
    for mismatch in [false, true] {
        let f = Fixture::new().await;
        let pane = f.live(ID, false);
        let rollout = find_rollout(&f.paths.account("personal").unwrap(), ID).unwrap();
        writeln!(fs::OpenOptions::new().append(true).open(&rollout).unwrap(), "{}",
            json!({"type":"turn_context","payload":{"approval_policy":"never","sandbox_policy":{"type":"danger-full-access"},"permission_profile":{"type":"disabled"}}})).unwrap();
        f.b.lock().unwrap().ignore_full_access = mismatch;
        let job = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
        let done = f
            .wait(
                job["id"].as_str().unwrap(),
                if mismatch { "recovered" } else { "complete" },
            )
            .await;
        {
            let target = f.b.lock().unwrap();
            let params = &target
                .calls
                .iter()
                .find(|(m, _)| m == "thread/resume")
                .unwrap()
                .1;
            assert_eq!(params["permissions"], ":danger-full-access");
            assert_eq!(params["approvalPolicy"], "never");
            assert!(params.get("sandbox").is_none());
            assert!(!target.calls.iter().any(|(m, _)| m == "turn/start"));
            if mismatch {
                assert!(
                    done["detail"]
                        .as_str()
                        .unwrap()
                        .contains("did not restore the requested permissions")
                );
                assert!(
                    !f.paths
                        .account("work")
                        .unwrap()
                        .join("launch-args")
                        .exists()
                );
            }
        }
        if !mismatch {
            let destination = f.tmp.path().join("new-working-dir");
            fs::create_dir(&destination).unwrap();
            let relocate = f.invoke(&[
                "relocate",
                "--pane",
                &pane,
                "--to",
                destination.to_str().unwrap(),
            ]);
            f.wait(relocate["id"].as_str().unwrap(), "complete").await;
            let target = f.b.lock().unwrap();
            let params = &target
                .calls
                .iter()
                .rev()
                .find(|(m, _)| m == "thread/resume")
                .unwrap()
                .1;
            assert_eq!(params["permissions"], ":danger-full-access");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_direct_session_moves_without_a_pause_prompt() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    f.a.lock().unwrap().threads.clear();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let j = f.invoke(&["move", "--pane", &pane, "--to", "work"]);
    f.wait(j["id"].as_str().unwrap(), "complete").await;
    assert_eq!(f.invoke(&["sessions"])["sessions"][0]["account"], "work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relocation_moves_folder_preserves_history_and_rejects_collisions() {
    let f = Fixture::new().await;
    let project = f.tmp.path().join("project before");
    let target = f.tmp.path().join("project after");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("notes"), "unsaved project changes").unwrap();
    let pane = f.live_in(ID, false, &project);
    f.a.lock().unwrap().threads.get_mut(ID).unwrap()["historyMode"] = json!("paginated");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let plan = f.invoke(&[
        "relocate",
        "--pane",
        &pane,
        "--to",
        target.to_str().unwrap(),
        "--move-files",
        "--dry-run",
    ]);
    assert_eq!(plan["relocation"]["from"], project.to_str().unwrap());
    assert!(project.exists() && !target.exists());
    fs::create_dir(&target).unwrap();
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &[
                "relocate",
                "--pane",
                &pane,
                "--to",
                target.to_str().unwrap(),
                "--move-files"
            ]
        )
        .status
        .success()
    );
    fs::remove_dir(&target).unwrap();
    let j = f.invoke(&[
        "relocate",
        "--pane",
        &pane,
        "--to",
        target.to_str().unwrap(),
        "--move-files",
    ]);
    let done = f.wait(j["id"].as_str().unwrap(), "complete").await;
    assert_eq!(done["relocation"]["state"], "complete");
    assert!(!project.exists());
    assert_eq!(
        fs::read_to_string(target.join("notes")).unwrap(),
        "unsaved project changes"
    );
    let list = f.invoke(&["sessions"]);
    assert_eq!(list["sessions"][0]["cwd"], target.to_str().unwrap());
    assert_eq!(list["sessions"][0]["thread_id"], ID);
    assert_eq!(list["sessions"][0]["account"], "personal");
    assert!(
        f.a.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "thread/resume")
            .all(|(_, p)| p.get("path").is_none())
    );
    // Redefining the root changes only the session, not the project files.
    fs::create_dir(&project).unwrap();
    let j = f.invoke(&[
        "relocate",
        "--pane",
        &pane,
        "--to",
        project.to_str().unwrap(),
    ]);
    f.wait(j["id"].as_str().unwrap(), "complete").await;
    assert!(target.join("notes").exists());
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["cwd"],
        project.to_str().unwrap()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relocation_waits_for_other_live_sessions_and_can_be_cancelled() {
    let f = Fixture::new().await;
    let project = f.tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    let pane = f.live_in(ID, false, &project);
    f.live_in("22222222-2222-4222-8222-222222222222", false, &project);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let target = f.tmp.path().join("destination");
    let j = f.invoke(&[
        "relocate",
        "--pane",
        &pane,
        "--to",
        target.to_str().unwrap(),
        "--move-files",
    ]);
    let id = j["id"].as_str().unwrap();
    f.wait(id, "waiting_for_project").await;
    assert!(project.exists() && !target.exists());
    f.invoke(&["cancel-move", id]);
    f.wait(id, "cancelled").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_relocation_restart_rolls_back_folder_before_recovery() {
    let f = Fixture::new().await;
    let project = f.tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("file"), "retained").unwrap();
    let pane = f.live_in(ID, false, &project);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let target = f.tmp.path().join("destination");
    f.a.lock().unwrap().fail_resume_cwd = Some(target.clone());
    let j = f.invoke(&[
        "relocate",
        "--pane",
        &pane,
        "--to",
        target.to_str().unwrap(),
        "--move-files",
    ]);
    f.wait(j["id"].as_str().unwrap(), "recovered").await;
    assert!(project.join("file").exists() && !target.exists());
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["cwd"],
        project.to_str().unwrap()
    );
    let server = f.a.lock().unwrap();
    assert!(
        server
            .calls
            .iter()
            .any(|(method, params)| method == "thread/resume"
                && params["cwd"] == project.to_str().unwrap()
                && params["approvalPolicy"] == "on-request"
                && params["sandbox"] == "workspace-write")
    );
    assert_eq!(server.threads[ID]["status"]["type"], "idle");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_filesystem_move_preserves_symlinks_and_retains_source_backup() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let f = Fixture::new().await;
    let project = f.tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    let destination = tempfile::tempdir_in("/dev/shm").unwrap();
    assert_ne!(
        fs::metadata(&project).unwrap().dev(),
        fs::metadata(destination.path()).unwrap().dev(),
        "test needs distinct filesystems"
    );
    fs::write(project.join("file"), "retained").unwrap();
    symlink("file", project.join("link")).unwrap();
    let pane = f.live_in(ID, false, &project);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let target = destination.path().join("new parent/nested/destination");
    f.invoke(&[
        "relocate",
        "--pane",
        &pane,
        "--to",
        target.to_str().unwrap(),
        "--move-files",
        "--dry-run",
    ]);
    assert!(
        !target.parent().unwrap().exists(),
        "preview must not create directories"
    );
    let j = f.invoke(&[
        "relocate",
        "--pane",
        &pane,
        "--to",
        target.to_str().unwrap(),
        "--move-files",
    ]);
    let done = f.wait(j["id"].as_str().unwrap(), "complete").await;
    assert!(!project.exists());
    assert_eq!(
        fs::read_link(target.join("link")).unwrap(),
        PathBuf::from("file")
    );
    assert_eq!(fs::read_to_string(target.join("file")).unwrap(), "retained");
    assert!(
        Path::new(done["relocation"]["retained_source"].as_str().unwrap())
            .join("file")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saved_relocation_records_the_new_directory_without_rewriting_history() {
    let f = Fixture::new().await;
    let project = f.tmp.path().join("old");
    let target = f.tmp.path().join("new");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&target).unwrap();
    let history_path = history(&f.paths.account("personal").unwrap(), &project, ID);
    let original = fs::read(&history_path).unwrap();
    let job = f.invoke(&["relocate", "--thread", ID, "--to", target.to_str().unwrap()]);
    let done = f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert_eq!(done["source"]["tmux_session"], "codex-old");
    assert_eq!(fs::read(&history_path).unwrap(), original);
    tmux(
        &f.socket,
        &["kill-pane", "-t", done["source"]["pane"].as_str().unwrap()],
    );
    let list = f.invoke(&["sessions", "--all"]);
    assert_eq!(list["sessions"][0]["lifecycle"], "inactive");
    assert_eq!(list["sessions"][0]["cwd"], target.to_str().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_delete_requires_fresh_confirmation_and_keeps_project_files() {
    let f = Fixture::new().await;
    let project = f.tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("important"), "user work").unwrap();
    let a = history(&f.paths.account("personal").unwrap(), &project, ID);
    let b = history(&f.paths.account("work").unwrap(), &project, ID);
    let input_cache = f.paths.account("personal").unwrap().join("history.jsonl");
    fs::write(
        &input_cache,
        format!(
            "{}\n{}\n",
            json!({"session_id":ID,"text":"deleted prompt"}),
            json!({"session_id":"other","text":"kept prompt"})
        ),
    )
    .unwrap();
    let unrelated = history(
        &f.paths.account("work").unwrap(),
        &project,
        "22222222-2222-4222-8222-222222222222",
    );
    let child_id = "33333333-3333-4333-8333-333333333333";
    let child = history(&f.paths.account("personal").unwrap(), &project, child_id);
    let content = fs::read_to_string(&child).unwrap();
    let mut lines: Vec<Value> = content
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["payload"]["source"] = json!({"subAgent":{"thread_spawn":{"parent_thread_id":ID}}});
    fs::write(
        &child,
        lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let backup = f.paths.root.join("move-backups/old/backup.jsonl");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();
    fs::copy(&a, &backup).unwrap();
    let key = format!("thread:{ID}");
    let preview = f.invoke(&["delete-session", "--session", &key]);
    assert_eq!(preview["copies"].as_array().unwrap().len(), 4);
    assert_eq!(preview["thread_ids"].as_array().unwrap().len(), 2);
    assert!(a.exists() && b.exists());
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &[
                "delete-session",
                "--session",
                &key,
                "--confirm",
                "wrong",
                "--revision",
                preview["revision"].as_str().unwrap()
            ]
        )
        .status
        .success()
    );
    // A new history record invalidates a previously displayed confirmation.
    writeln!(
        fs::OpenOptions::new().append(true).open(&a).unwrap(),
        "{}",
        json!({"type":"event_msg","payload":{"type":"task_complete"}})
    )
    .unwrap();
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &[
                "delete-session",
                "--session",
                &key,
                "--confirm",
                ID,
                "--revision",
                preview["revision"].as_str().unwrap()
            ]
        )
        .status
        .success()
    );
    let preview = f.invoke(&["delete-session", "--session", &key]);
    let result = f.invoke(&[
        "delete-session",
        "--session",
        &key,
        "--confirm",
        ID,
        "--revision",
        preview["revision"].as_str().unwrap(),
    ]);
    assert_eq!(result["deleted"], true);
    assert!(!a.exists() && !b.exists() && !backup.exists() && !child.exists());
    let deleted: Vec<_> =
        f.a.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "thread/delete")
            .map(|(_, p)| p["threadId"].as_str().unwrap().to_owned())
            .collect();
    assert_eq!(deleted, vec![child_id.to_string(), ID.to_string()]);
    assert!(unrelated.exists());
    assert_eq!(
        fs::read_to_string(&input_cache).unwrap(),
        format!("{}\n", json!({"session_id":"other","text":"kept prompt"}))
    );
    assert_eq!(
        fs::read_to_string(project.join("important")).unwrap(),
        "user work"
    );
    assert!(
        !f.invoke(&["sessions", "--all"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["thread_id"] == ID)
    );
    assert!(
        !command(&f.paths.root, &f.socket, &["open", "--session", &key])
            .status
            .success()
    );
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &["run", "--account", "personal", "--thread", ID]
        )
        .status
        .success()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_rejects_live_and_loaded_copies_and_retries_partial_deletion() {
    let f = Fixture::new().await;
    let live = f.live(ID, false);
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &["delete-session", "--session", &format!("pane:{live}")]
        )
        .status
        .success()
    );
    let end = f.invoke(&["terminate", "--pane", &live]);
    f.wait(end["id"].as_str().unwrap(), "complete").await;
    let a = find_rollout(&f.paths.account("personal").unwrap(), ID).unwrap();
    let b = history(&f.paths.account("work").unwrap(), f.tmp.path(), ID);
    let key = format!("thread:{ID}");
    let plan = f.invoke(&["delete-session", "--session", &key]);
    f.b.lock()
        .unwrap()
        .threads
        .insert(ID.into(), thread_value(&b, ID, "active"));
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &[
                "delete-session",
                "--session",
                &key,
                "--confirm",
                ID,
                "--revision",
                plan["revision"].as_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert!(a.exists() && b.exists());
    f.b.lock().unwrap().threads.get_mut(ID).unwrap()["status"]["type"] = json!("notLoaded");
    f.b.lock().unwrap().fail_delete = true;
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &[
                "delete-session",
                "--session",
                &key,
                "--confirm",
                ID,
                "--revision",
                plan["revision"].as_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert!(!a.exists() && b.exists());
    assert!(
        !command(&f.paths.root, &f.socket, &["open", "--session", &key])
            .status
            .success()
    );
    f.b.lock().unwrap().fail_delete = false;
    let retry = f.invoke(&["delete-session", "--session", &key]);
    f.invoke(&[
        "delete-session",
        "--session",
        &key,
        "--confirm",
        ID,
        "--revision",
        retry["revision"].as_str().unwrap(),
    ]);
    assert!(!b.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_reclaims_ended_names_and_plain_tmux_has_no_codex_actions() {
    let f = Fixture::new().await;
    let live = f.live(ID, false);
    let dead = tmux(
        &f.socket,
        &[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-s",
            "codex-cli-main-menu",
            "tmux set-option -p remain-on-exit on; exit",
        ],
    );
    let until = Instant::now() + Duration::from_secs(5);
    while tmux(
        &f.socket,
        &["display-message", "-p", "-t", &dead, "#{pane_dead}"],
    ) != "1"
    {
        assert!(Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let renamed = f.invoke(&[
        "rename",
        "--session",
        &format!("pane:{live}"),
        "--name",
        "cli-main-menu",
    ]);
    assert_eq!(renamed["name"], "codex-cli-main-menu");
    assert_eq!(renamed["tmux_session"], "codex-cli-main-menu");
    let again = f.invoke(&[
        "rename",
        "--session",
        &format!("pane:{live}"),
        "--name",
        "codex-cli-main-menu",
    ]);
    assert_eq!(again["tmux_session"], "codex-cli-main-menu");
    let plain = tmux(
        &f.socket,
        &[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-s",
            "codex-shell",
            "sleep 120",
        ],
    );
    let list = f.invoke(&["sessions", "--include-tmux"]);
    let rows = list["sessions"].as_array().unwrap();
    let retained = rows.iter().find(|s| s["pane"] == dead).unwrap();
    assert_eq!(retained["kind"], "tmux");
    assert_eq!(retained["tmux_dead"], true);
    assert!(
        retained["name"]
            .as_str()
            .unwrap()
            .starts_with("ended-cli-main-menu-")
    );
    assert_eq!(
        rows.iter().find(|s| s["pane"] == plain).unwrap()["kind"],
        "tmux"
    );
    assert_eq!(
        f.invoke(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.invoke(&["open", "--session", &format!("tmux:{plain}")])["session"]["pane"],
        plain
    );
    assert!(
        !command(
            &f.paths.root,
            &f.socket,
            &["move", "--pane", &plain, "--to", "work"]
        )
        .status
        .success()
    );
    let blocked = command(
        &f.paths.root,
        &f.socket,
        &[
            "rename",
            "--session",
            &format!("pane:{live}"),
            "--name",
            "shell",
        ],
    );
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("another running tmux session"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn names_survive_rename_move_and_reopen_without_history_suffixes() {
    let f = Fixture::new().await;
    history(&f.paths.account("personal").unwrap(), f.tmp.path(), ID);
    let key = format!("thread:{ID}");
    f.invoke(&["rename", "--session", &key, "--name", "studio"]);
    let opened = f.invoke(&["open", "--session", &key]);
    assert_eq!(opened["session"]["tmux_session"], "codex-studio");
    let pane = opened["session"]["pane"].as_str().unwrap();
    f.invoke(&[
        "rename",
        "--session",
        &format!("pane:{pane}"),
        "--name",
        "renamed",
    ]);
    let moved = f.invoke(&["move", "--pane", pane, "--to", "work"]);
    f.wait(moved["id"].as_str().unwrap(), "complete").await;
    assert_eq!(
        f.invoke(&["sessions"])["sessions"][0]["name"],
        "codex-renamed"
    );
    let end = f.invoke(&["terminate", "--pane", pane]);
    f.wait(end["id"].as_str().unwrap(), "complete").await;
    let list = f.invoke(&["sessions", "--all"]);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(list["sessions"][0]["account"], "work");
    assert_eq!(list["sessions"][0]["name"], "codex-renamed");
    let reopened = f.invoke(&["open", "--session", &key]);
    assert_eq!(reopened["session"]["tmux_session"], "codex-renamed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_replaces_the_terminal_and_refreshes_an_unused_account_backend() {
    let f = Fixture::new().await;
    let pane = f.live(ID, true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let old_pid = tmux(
        &f.socket,
        &["display-message", "-p", "-t", &pane, "#{pane_pid}"],
    );
    let job = f.invoke(&["restart", "--pane", &pane, "--pause"]);
    assert_eq!(job["kind"], "restart");
    let done = f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert!(done["detail"].as_str().unwrap().contains("Restarted Codex"));
    assert!(
        f.paths
            .account("personal")
            .unwrap()
            .join("backend-restart")
            .exists()
    );
    assert_ne!(
        old_pid,
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
        )
    );
    let list = f.invoke(&["sessions"]);
    assert_eq!(list["sessions"][0]["thread_id"], ID);
    assert_eq!(list["sessions"][0]["account"], "personal");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarting_one_terminal_does_not_restart_a_backend_used_by_another() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    let other = f.live("22222222-2222-4222-8222-222222222222", true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let other_pid = tmux(
        &f.socket,
        &["display-message", "-p", "-t", &other, "#{pane_pid}"],
    );
    let job = f.invoke(&["restart", "--pane", &pane]);
    let done = f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert!(
        done["detail"]
            .as_str()
            .unwrap()
            .contains("other sessions use it")
    );
    assert!(
        !f.paths
            .account("personal")
            .unwrap()
            .join("backend-restart")
            .exists()
    );
    assert_eq!(
        other_pid,
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &other, "#{pane_pid}"]
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminate_preserves_resumable_history_and_other_sessions_shared_and_direct() {
    for shared in [true, false] {
        let f = Fixture::new().await;
        let pane = f.live(ID, false);
        let other_id = "22222222-2222-4222-8222-222222222222";
        let other = f.live(other_id, true);
        let other_pid = tmux(
            &f.socket,
            &["display-message", "-p", "-t", &other, "#{pane_pid}"],
        );
        let home = f.paths.account("personal").unwrap();
        let before = fs::read(find_rollout(&home, ID).unwrap()).unwrap();
        if shared {
            // Ending in place needs no portable export or eligible destination.
            f.a.lock().unwrap().threads.get_mut(ID).unwrap()["historyMode"] = json!("paginated");
        } else {
            f.a.lock().unwrap().threads.remove(ID);
        }
        let j = f.invoke(&["terminate", "--pane", &pane]);
        assert_eq!(j["kind"], "terminate");
        f.wait(j["id"].as_str().unwrap(), "complete").await;
        assert_eq!(fs::read(find_rollout(&home, ID).unwrap()).unwrap(), before);
        let sessions = f.invoke(&["sessions", "--all"]);
        let saved = sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["thread_id"] == ID)
            .unwrap();
        assert_eq!(saved["lifecycle"], "inactive");
        assert_eq!(saved["account"], "personal");
        assert_eq!(
            tmux(
                &f.socket,
                &["display-message", "-p", "-t", &other, "#{pane_pid}"]
            ),
            other_pid
        );
        assert!(
            !command(&f.paths.root, &f.socket, &["terminate", "--pane", &pane])
                .status
                .success()
        );
        assert!(
            !command(&f.paths.root, &f.socket, &["terminate", "--thread", ID])
                .status
                .success()
        );
        assert!(
            !f.a.lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "turn/steer" || m == "thread/resume")
        );
        let resumed = tmux(
            &f.socket,
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                &format!(
                    "exec {} --data-dir {} run --account personal --thread {}",
                    binary(),
                    f.paths.root.display(),
                    ID
                ),
            ],
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if f.invoke(&["sessions"])["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["pane"] == resumed && s["thread_id"] == ID)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "saved conversation failed to resume"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminate_busy_session_requires_pause_and_separate_background_consent() {
    let f = Fixture::new().await;
    let pane = f.live(ID, true);
    f.a.lock().unwrap().background = true;
    let j = f.invoke(&["terminate", "--pane", &pane]);
    let id = j["id"].as_str().unwrap();
    let prompt = f.wait(id, "awaiting_pause_confirmation").await;
    assert_eq!(f.invoke(&["terminate", "--pane", &pane])["id"], id);
    assert!(
        !command(&f.paths.root, &f.socket, &["restart", "--pane", &pane])
            .status
            .success()
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/steer")
    );
    f.invoke(&[
        "move-response",
        id,
        "yes",
        "--sequence",
        &prompt["confirmation_seq"].to_string(),
    ]);
    f.wait(id, "awaiting_background_approval").await;
    f.invoke(&["move-response", id, "no"]);
    f.wait(id, "cancelled").await;
    assert!(f.a.lock().unwrap().background);
    assert_eq!(f.invoke(&["sessions"])["sessions"][0]["pane"], pane);
    let j = f.invoke(&["terminate", "--pane", &pane, "--pause"]);
    let id = j["id"].as_str().unwrap();
    f.wait(id, "awaiting_background_approval").await;
    f.invoke(&["move-response", id, "yes"]);
    f.wait(id, "complete").await;
    assert!(!f.a.lock().unwrap().background);
    assert_eq!(f.invoke(&["sessions"])["sessions"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_work_waits_for_positive_idle_cleans_background_and_keeps_terminal() {
    let f = Fixture::new().await;
    let pane = f.live(ID, true);
    let pid = tmux(
        &f.socket,
        &["display-message", "-p", "-t", &pane, "#{pane_pid}"],
    );
    f.a.lock().unwrap().hold_busy = true;
    let j = f.invoke(&["pause-session", "--pane", &pane, "--pause"]);
    let id = j["id"].as_str().unwrap();
    f.wait(id, "waiting_for_idle").await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        f.invoke(&["moves"])
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["id"] == id && j["phase"] != "complete")
    );
    assert!(
        !command(&f.paths.root, &f.socket, &["terminate", "--pane", &pane])
            .status
            .success()
    );
    f.invoke(&["cancel-move", id]);
    f.wait(id, "cancelled").await;
    {
        let mut server = f.a.lock().unwrap();
        server.hold_busy = false;
        server.background = true;
    }
    let j = f.invoke(&["pause-session", "--pane", &pane]);
    let id = j["id"].as_str().unwrap();
    assert_eq!(j["kind"], "pause");
    f.wait(id, "awaiting_pause_confirmation").await;
    f.invoke(&["move-response", id, "yes"]);
    f.wait(id, "awaiting_background_approval").await;
    f.invoke(&["move-response", id, "yes"]);
    f.wait(id, "complete").await;
    assert_eq!(
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
        ),
        pid
    );
    assert_eq!(f.invoke(&["sessions"])["sessions"][0]["activity"], "idle");
    let server = f.a.lock().unwrap();
    assert!(!server.background);
    assert!(
        !server
            .calls
            .iter()
            .any(|(m, _)| m == "thread/archive" || m == "thread/resume")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn termination_recovery_restores_resume_visibility_without_relaunching() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    f.a.lock().unwrap().fail_unarchive = true;
    let j = f.invoke(&["terminate", "--pane", &pane]);
    let id = j["id"].as_str().unwrap();
    f.wait(id, "failed").await;
    assert_eq!(f.invoke(&["sessions"])["sessions"], json!([]));
    f.a.lock().unwrap().fail_unarchive = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = command(&f.paths.root, &f.socket, &["recover", id]);
        if output.status.success() {
            break;
        }
        // The worker may still hold the recovery lock, or its own recovery just completed.
        if f.invoke(&["moves"])
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["id"] == id && j["phase"] == "complete")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(find_rollout(&f.paths.account("personal").unwrap(), ID).is_some());
    assert_eq!(f.invoke(&["sessions"])["sessions"], json!([]));
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "thread/resume")
    );
    assert!(
        f.invoke(&["sessions", "--all"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["thread_id"] == ID && s["inactive"] == true)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_group_activity_before_access_and_keep_names_after_external_exit() {
    let f = Fixture::new().await;
    let ids = [
        ID,
        "22222222-2222-4222-8222-222222222222",
        "33333333-3333-4333-8333-333333333333",
        "44444444-4444-4444-8444-444444444444",
        "55555555-5555-4555-8555-555555555555",
    ];
    let busy_old = f.live(ids[0], true);
    f.live(ids[1], true);
    f.live(ids[2], false);
    for id in &ids[3..] {
        history(&f.paths.account("personal").unwrap(), f.tmp.path(), id);
    }
    tmux(
        &f.socket,
        &["rename-session", "-t", &busy_old, "codex-my-custom-name"],
    );
    let access = json!({ids[0]:100, ids[1]:200, ids[2]:300, ids[3]:400, ids[4]:500});
    fs::write(
        f.paths.root.join("session-access.json"),
        serde_json::to_vec(&access).unwrap(),
    )
    .unwrap();
    let listed = f.invoke(&["sessions", "--all"]);
    let order = |value: &Value| {
        value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["thread_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&listed), [ids[1], ids[0], ids[2], ids[4], ids[3]]);
    assert_eq!(listed["sessions"][1]["name"], "codex-my-custom-name");
    // Discovery alone must preserve the label: no Open/Rename/move through switcher.
    tmux(&f.socket, &["kill-pane", "-t", &busy_old]);
    f.a.lock().unwrap().threads.get_mut(ids[0]).unwrap()["status"]["type"] = json!("notLoaded");
    let saved = f.invoke(&["sessions", "--all"]);
    assert_eq!(order(&saved), [ids[1], ids[2], ids[4], ids[3], ids[0]]);
    assert_eq!(saved["sessions"][4]["name"], "codex-my-custom-name");
    assert_eq!(saved["sessions"][4]["last_accessed"], 100);
    f.a.lock().unwrap().threads.get_mut(ids[1]).unwrap()["status"]["type"] = json!("idle");
    assert_eq!(
        order(&f.invoke(&["sessions", "--all"])),
        [ids[2], ids[1], ids[4], ids[3], ids[0]]
    );
    let opened = f.invoke(&["open", "--session", &format!("thread:{}", ids[0])]);
    assert_eq!(opened["session"]["tmux_session"], "codex-my-custom-name");
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/start")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn focused_cli_views_expose_existing_account_session_and_history_data() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    let details = f.invoke(&["session", "--session", &format!("thread:{ID}")]);
    assert_eq!(details["schema_version"], 1);
    assert_eq!(details["session"]["pane"], pane);
    assert_eq!(details["session"]["account"], "personal");
    assert_eq!(details["account_details"]["account"]["name"], "personal");
    let account = f.invoke(&["account", "show", "personal"]);
    assert!(account["account"]["usage_fresh"].is_boolean());
    assert!(account["remaining_percent"].is_object());
    let history = f.invoke(&["account", "history", "personal", "--range", "weekly"]);
    assert_eq!(history["history"]["account"], "personal");
    assert_eq!(history["range"]["step"], 21600);
    assert_eq!(history["history"]["bins"].as_array().unwrap().len(), 28);
    let custom = f.invoke(&[
        "account", "history", "personal", "--start", "0", "--end", "3600", "--step", "1800",
    ]);
    assert_eq!(custom["history"]["bins"].as_array().unwrap().len(), 2);
    for args in [
        vec!["account", "show", "missing"],
        vec!["session", "--session", "a display name"],
        vec![
            "session",
            "--session",
            "thread:00000000-0000-4000-8000-000000000000",
        ],
        vec![
            "account", "history", "personal", "--start", "0", "--end", "3600", "--step", "1",
        ],
    ] {
        assert!(!command(&f.paths.root, &f.socket, &args).status.success());
    }
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| matches!(m.as_str(), "turn/start" | "turn/steer"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_live_session_skips_blocked_unrelated_saved_history() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    let dir = f.paths.account("work").unwrap().join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let fifo =
        std::ffi::CString::new(dir.join("unrelated.jsonl").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let started = Instant::now();
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new(binary())
            .env("TMUX", format!("{},1,0", f.socket))
            .arg("--data-dir")
            .arg(&f.paths.root)
            .args(["open", "--session", &format!("pane:{pane}")])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("live open scanned blocked saved history")
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["pane"], pane);
    eprintln!(
        "live open with blocked saved history: {:?}",
        started.elapsed()
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| matches!(m.as_str(), "turn/start" | "thread/resume"))
    );
}
