//! Bounded JSON-lines transport for a private stdio app-server worker.
//! No raw protocol data is logged: frames can contain credentials and private text.
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, broadcast, mpsc, oneshot},
};

const FRAME_LIMIT: usize = 16 * 1024 * 1024;
type Reply = oneshot::Sender<Result<Value, String>>;
#[derive(Clone)]
pub struct Rpc {
    input: mpsc::Sender<Option<Value>>,
    pending: Arc<Mutex<HashMap<u64, Reply>>>,
    sequence: Arc<AtomicU64>,
    pub events: broadcast::Sender<Value>,
    child: Option<Arc<Mutex<Child>>>,
    shared: Option<Arc<Mutex<crate::control::Control>>>,
    pub initialized: Value,
}
impl Rpc {
    pub async fn spawn(binary: &str, home: &Path, cwd: &Path) -> Result<Self> {
        crate::paths::private_dir(home)?;
        if crate::control::available(home) {
            let control = crate::control::Control::connect(home).await?;
            let (input, _) = mpsc::channel(1);
            let (events, _) = broadcast::channel(1);
            return Ok(Self {
                input,
                events,
                pending: Arc::new(Mutex::new(HashMap::new())),
                sequence: Arc::new(AtomicU64::new(1)),
                child: None,
                shared: Some(Arc::new(Mutex::new(control))),
                initialized: Value::Null,
            });
        }
        let mut cmd = Command::new(binary);
        cmd.args([
            "app-server",
            "--listen",
            "stdio://",
            "-c",
            "cli_auth_credentials_store=\"file\"",
        ])
        .env("CODEX_HOME", home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        // Owned workers have a separate process group; no interaction with the invoking agent.
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .context("could not start isolated Codex app-server")?;
        let mut stdin = child.stdin.take().context("missing worker input")?;
        let stdout = child.stdout.take().context("missing worker output")?;
        let (input, mut queue) = mpsc::channel::<Option<Value>>(128);
        let pending = Arc::new(Mutex::new(HashMap::<u64, Reply>::new()));
        let (events, _) = broadcast::channel(4096);
        let p = pending.clone();
        let e = events.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let line = match read_frame(&mut reader).await {
                    Ok(Some(v)) => v,
                    _ => break,
                };
                if line.get("method").is_none() {
                    if let Some(id) = line["id"].as_u64() {
                        if let Some(reply) = p.lock().await.remove(&id) {
                            let result = if line.get("error").is_some() {
                                Err(format!(
                                    "app-server rejected request (code {})",
                                    line["error"]["code"]
                                ))
                            } else {
                                Ok(line["result"].clone())
                            };
                            let _ = reply.send(result);
                            continue;
                        }
                    }
                }
                let _ = e.send(line);
            }
            for (_, reply) in p.lock().await.drain() {
                let _ = reply.send(Err(
                    "app-server disconnected; request outcome may be unknown".into(),
                ));
            }
            let _ = e.send(json!({"method":"switcher/disconnected"}));
        });
        tokio::spawn(async move {
            while let Some(Some(value)) = queue.recv().await {
                let Ok(mut bytes) = serde_json::to_vec(&value) else {
                    break;
                };
                bytes.push(b'\n');
                if stdin.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });
        let mut rpc = Self {
            input,
            pending,
            sequence: Arc::new(AtomicU64::new(1)),
            events,
            child: Some(Arc::new(Mutex::new(child))),
            shared: None,
            initialized: Value::Null,
        };
        match rpc.request("initialize", json!({"clientInfo":{"name":"codex_switcher","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await {
            Ok(value) => rpc.initialized = value,
            Err(err) => { let _ = rpc.stop_idle().await; return Err(err); }
        }
        rpc.send(json!({"method":"initialized"})).await?;
        Ok(rpc)
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if let Some(control) = &self.shared {
            return control.lock().await.request(method, params).await;
        }
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self
            .send(json!({"id":id,"method":method,"params":params}))
            .await
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            bail!("app-server input closed");
        }
        let result = tokio::time::timeout(Duration::from_secs(30), rx).await;
        self.pending.lock().await.remove(&id);
        match result {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(anyhow!(error)),
            _ => bail!("app-server request timed out or disconnected; outcome may be unknown"),
        }
    }
    pub async fn send(&self, frame: Value) -> Result<()> {
        self.input
            .send(Some(frame))
            .await
            .map_err(|_| anyhow!("app-server input closed"))
    }
    /// Call only on an idle metadata worker or after independently verified quiescence.
    /// Never escalate to SIGKILL on timeout.
    pub async fn stop_idle(&self) -> Result<()> {
        let Some(child) = &self.child else {
            return Ok(());
        };
        self.input.send(None).await.ok();
        let mut child = child.lock().await;
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("worker has not exited; ownership retained")??;
        Ok(())
    }
    pub async fn pid(&self) -> Option<u32> {
        match &self.child {
            Some(child) => child.lock().await.id(),
            None => None,
        }
    }
    pub async fn is_alive(&self) -> bool {
        match &self.child {
            Some(child) => child
                .lock()
                .await
                .try_wait()
                .is_ok_and(|status| status.is_none()),
            None => self
                .request("thread/loaded/list", json!({"limit":1}))
                .await
                .is_ok(),
        }
    }
}

pub async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Value>> {
    let mut frame = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            } else {
                bail!("truncated protocol frame")
            }
        }
        let end = buf.iter().position(|b| *b == b'\n');
        let count = end.map_or(buf.len(), |i| i + 1);
        if frame.len() + count > FRAME_LIMIT {
            bail!("protocol frame too large")
        }
        frame.extend_from_slice(&buf[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(
                serde_json::from_slice(&frame).context("invalid protocol frame")?,
            ));
        }
    }
}
