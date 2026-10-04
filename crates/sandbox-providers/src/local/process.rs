//! A remote group leader stays alive until the host acknowledges termination.
//! This pins its PID while cleanup signals the group, including grandchildren.
use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncRead;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

/// One process's combined stdout and stderr. Past this the session fails
/// rather than buffering without bound. The 64 KiB above 64 MiB leaves a
/// caller reading up to 64 MiB room to detect that it overflowed.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024 + 64 * 1024;
static NEXT_PROCESS: AtomicU64 = AtomicU64::new(0);
type Exit = Result<Option<i32>, String>;

struct DockerSession {
    stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    exit: watch::Receiver<Option<Exit>>,
    cancel: CancellationToken,
}
impl Drop for DockerSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
#[async_trait]
impl Session for DockerSession {
    async fn write_stdin(&self, bytes: &[u8]) -> Result<(), SandboxError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| SandboxError::Request("stdin is closed".into()))?;
        stdin
            .write_all(bytes)
            .await
            .map_err(|_| SandboxError::Request("stdin write failed".into()))?;
        stdin
            .flush()
            .await
            .map_err(|_| SandboxError::Request("stdin flush failed".into()))
    }
    async fn close_stdin(&self) -> Result<(), SandboxError> {
        *self.stdin.lock().await = None;
        Ok(())
    }
    async fn wait(&self) -> Result<Option<i32>, SandboxError> {
        let mut exit = self.exit.clone();
        loop {
            if let Some(result) = exit.borrow_and_update().clone() {
                return result.map_err(SandboxError::Terminated);
            }
            exit.changed()
                .await
                .map_err(|_| SandboxError::Terminated("process supervisor stopped".into()))?;
        }
    }
    async fn kill(&self) -> Result<(), SandboxError> {
        self.cancel.cancel();
        self.wait().await.map(|_| ())
    }
}

impl DockerSandbox {
    pub(super) async fn execute_bounded(
        &self,
        request: ExecRequest,
    ) -> Result<ExecOutput, SandboxError> {
        let (session, mut output) = self
            .start_process(SpawnRequest {
                program: "sh".into(),
                args: vec!["-c".into(), request.command],
                cwd: request.cwd,
                env: request.env,
            })
            .await?;
        session.close_stdin().await?;
        let collect = async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            while let Some(chunk) = output.recv().await {
                if chunk.stderr {
                    stderr.extend(chunk.bytes);
                } else {
                    stdout.extend(chunk.bytes);
                }
            }
            let exit_code = session.wait().await?.unwrap_or(-1);
            Ok(ExecOutput {
                exit_code,
                stdout,
                stderr,
            })
        };
        if let Some(ms) = request.timeout_ms {
            match tokio::time::timeout(Duration::from_millis(ms), collect).await {
                Ok(result) => result,
                Err(_) => {
                    session.kill().await?;
                    Err(SandboxError::Timeout(
                        "Docker command exceeded its deadline".into(),
                    ))
                }
            }
        } else {
            collect.await
        }
        // Cancellation drops the session; its owned supervisor still kills the remote group.
    }

    pub(super) async fn start_process(
        &self,
        request: SpawnRequest,
    ) -> Result<(Arc<dyn Session>, Output), SandboxError> {
        let token = format!(
            "orca-{:x}-{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_PROCESS.fetch_add(1, Ordering::Relaxed)
        );
        let footer = format!("\n{token}-exit ").into_bytes();
        let script = format!(
            r#"
identity() {{ IFS= read -r record < /proc/$$/stat || exit 125; record=${{record##*) }}; set -- $record; shift 19; printf '{token}-start %s %s\n' "$$" "$1" >&2; }}
identity
IFS= read -r ack || exit 125
[ "$ack" = '{token}' ] || exit 125
exec 3<&0
"$@" <&3 &
child=$!
wait "$child"
code=$?
printf '\n{token}-exit %s\n' "$code" >&2
while :; do sleep 3600; done
"#
        );
        let mut args = self.exec_args(request.cwd.as_deref(), true);
        for (key, value) in request.env {
            args.splice(1..1, ["-e".into(), format!("{key}={value}")]);
        }
        args.extend([
            "setsid".into(),
            "--wait".into(),
            "sh".into(),
            "-c".into(),
            script,
            "orca-process".into(),
            request.program,
        ]);
        args.extend(request.args);
        let mut child = Command::new("docker")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| SandboxError::Request("Docker process could not start".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SandboxError::Request("Docker process has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SandboxError::Request("Docker process has no stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| SandboxError::Request("Docker process has no stderr".into()))?;
        let stdin_slot = Arc::new(Mutex::new(None));
        let cancel = CancellationToken::new();
        let (exit_tx, exit_rx) = watch::channel(None);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CHUNKS);
        let session = Arc::new(DockerSession {
            stdin: stdin_slot.clone(),
            exit: exit_rx,
            cancel: cancel.clone(),
        });
        let container = self.id.clone();
        let user = self.user.clone();
        tokio::spawn(async move {
            // No caller command starts until the host has captured remote identity.
            let handshake = tokio::select! {
                _ = cancel.cancelled() => Err("Docker process start cancelled".to_owned()),
                result = tokio::time::timeout(Duration::from_secs(10), read_identity(&mut stderr, &token)) => result.unwrap_or_else(|_| Err("Docker process handshake timed out".into())),
            };
            let (pid, start) = match handshake {
                Ok(identity) => identity,
                Err(error) => {
                    drop(stdin);
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let _ = ready_tx.send(Err(error.clone()));
                    let _ = exit_tx.send(Some(Err(error)));
                    return;
                }
            };
            let mut stdin = stdin;
            if stdin
                .write_all(format!("{token}\n").as_bytes())
                .await
                .is_err()
            {
                drop(stdin);
                let _ = terminate_group(&container, user.as_deref(), &pid, &start).await;
                let _ = child.start_kill();
                let _ = child.wait().await;
                let error = "Docker process acknowledgement failed".to_owned();
                let _ = ready_tx.send(Err(error.clone()));
                let _ = exit_tx.send(Some(Err(error)));
                return;
            }
            *stdin_slot.lock().await = Some(stdin);
            let _ = ready_tx.send(Ok(()));
            let total = Arc::new(AtomicUsize::new(0));
            let (completed_tx, mut completed_rx) = mpsc::channel(2);
            let mut stdout_task = tokio::spawn(pump(
                stdout,
                output_tx.clone(),
                false,
                None,
                total.clone(),
                completed_tx.clone(),
            ));
            let mut stderr_task = tokio::spawn(pump(
                stderr,
                output_tx,
                true,
                Some(footer),
                total,
                completed_tx,
            ));
            let result = tokio::select! {
                _ = cancel.cancelled() => Ok(None),
                result = completed_rx.recv() => result.unwrap_or_else(|| Err("Docker output transport stopped".into())).map(Some),
                _ = child.wait() => Err("Docker process transport exited before completion".into()),
            };
            let cleanup = terminate_group(&container, user.as_deref(), &pid, &start).await;
            let normal = matches!(&result, Ok(Some(_))) && cleanup.is_ok();
            let mut result = cleanup.and(result);
            if normal {
                // Do not kill the CLI before it drains remote bytes. Also wait
                // for the readers before accepting success: overflow may sit
                // behind the completion frame on the other output stream.
                tokio::select! {
                    _ = async {
                        let _ = child.wait().await;
                        let _ = (&mut stdout_task).await;
                        let _ = (&mut stderr_task).await;
                    } => {
                        while let Ok(event) = completed_rx.try_recv() {
                            if let Err(error) = event { result = Err(error); }
                        }
                    },
                    _ = cancel.cancelled() => {
                        let _ = child.start_kill(); let _ = child.wait().await;
                        stdout_task.abort(); stderr_task.abort();
                        result = Ok(None);
                    }
                }
            } else {
                let _ = child.start_kill();
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
            }
            let _ = exit_tx.send(Some(result));
            *stdin_slot.lock().await = None;
        });
        ready_rx
            .await
            .map_err(|_| SandboxError::Terminated("Docker process supervisor stopped".into()))?
            .map_err(SandboxError::Request)?;
        Ok((session, output_rx))
    }
}

async fn read_identity<R: AsyncRead + Unpin>(
    pipe: &mut R,
    token: &str,
) -> Result<(String, String), String> {
    let mut line = Vec::new();
    for _ in 0..256 {
        let byte = pipe
            .read_u8()
            .await
            .map_err(|_| "Docker process identity unavailable".to_owned())?;
        if byte == b'\n' {
            let text = std::str::from_utf8(&line)
                .map_err(|_| "Invalid Docker process identity".to_owned())?;
            let fields: Vec<_> = text.split_whitespace().collect();
            if fields.len() == 3
                && fields[0] == format!("{token}-start")
                && fields[1..]
                    .iter()
                    .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                && fields[1].parse::<u64>().unwrap_or(0) > 1
            {
                return Ok((fields[1].into(), fields[2].into()));
            }
            return Err(
                "Invalid Docker process identity; setsid and Linux procfs are required".into(),
            );
        }
        line.push(byte);
    }
    Err("Docker process identity exceeded its bound".into())
}

async fn terminate_group(
    container: &str,
    user: Option<&str>,
    pid: &str,
    start: &str,
) -> Result<(), String> {
    let script = format!(
        r#"
if [ -r /proc/{pid}/stat ]; then
 IFS= read -r record < /proc/{pid}/stat || exit 1
 record=${{record##*) }}; set -- $record; shift 19
 [ "$1" = '{start}' ] || exit 1
 kill -KILL -{pid} 2>/dev/null || exit 1
fi
count=0
while [ "$count" -lt 100 ]; do
 alive=0
 for file in /proc/[0-9]*/stat; do
  IFS= read -r record < "$file" 2>/dev/null || continue
  record=${{record##*) }}; set -- $record
  [ "$3" = '{pid}' ] && [ "$1" != Z ] && alive=1
 done
 [ "$alive" = 0 ] && exit 0
 count=$((count + 1)); sleep 0.05
done
exit 1
"#
    );
    let mut args = vec!["exec".into()];
    if let Some(user) = user {
        args.extend(["--user".into(), user.into()]);
    }
    args.extend([container.into(), "sh".into(), "-c".into(), script]);
    let result = tokio::time::timeout(Duration::from_secs(10), docker(&args)).await;
    match result {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        _ => Err("Remote Docker process termination was not confirmed".into()),
    }
}

async fn emit(
    tx: &mpsc::Sender<Chunk>,
    bytes: &[u8],
    stderr: bool,
    total: &AtomicUsize,
) -> Result<(), String> {
    let previous = total.fetch_add(bytes.len(), Ordering::Relaxed);
    let allowed = MAX_OUTPUT_BYTES.saturating_sub(previous).min(bytes.len());
    if allowed > 0 {
        tx.send(Chunk {
            stderr,
            bytes: bytes[..allowed].into(),
        })
        .await
        .map_err(|_| "Docker output receiver closed".to_owned())?;
    }
    if allowed < bytes.len() {
        Err("Docker process output exceeded 64 MiB limit".into())
    } else {
        Ok(())
    }
}
async fn pump<R: AsyncRead + Unpin>(
    mut pipe: R,
    tx: mpsc::Sender<Chunk>,
    stderr: bool,
    footer: Option<Vec<u8>>,
    total: Arc<AtomicUsize>,
    completed: mpsc::Sender<Result<i32, String>>,
) {
    let mut buffer = [0u8; 8192];
    let mut pending = Vec::new();
    loop {
        let size = match pipe.read(&mut buffer).await {
            Ok(size) => size,
            Err(_) => {
                let _ = completed
                    .send(Err("Docker output read failed".into()))
                    .await;
                return;
            }
        };
        if size == 0 {
            if !pending.is_empty() {
                if let Err(error) = emit(&tx, &pending, stderr, &total).await {
                    let _ = completed.send(Err(error)).await;
                }
            }
            return;
        }
        pending.extend_from_slice(&buffer[..size]);
        if let Some(marker) = &footer {
            if let Some(index) = pending
                .windows(marker.len())
                .position(|bytes| bytes == marker)
            {
                if let Err(error) = emit(&tx, &pending[..index], stderr, &total).await {
                    let _ = completed.send(Err(error)).await;
                    return;
                }
                let status = &pending[index + marker.len()..];
                if let Some(end) = status.iter().position(|byte| *byte == b'\n') {
                    let code = std::str::from_utf8(&status[..end])
                        .ok()
                        .and_then(|text| text.parse::<i32>().ok())
                        .filter(|code| (0..=255).contains(code));
                    let _ = completed
                        .send(code.ok_or_else(|| "Invalid Docker process completion".into()))
                        .await;
                    return;
                }
                if status.len() > 4 {
                    let _ = completed
                        .send(Err("Invalid Docker process completion".into()))
                        .await;
                    return;
                }
                pending.drain(..index);
                continue;
            }
        }
        // Retain only bytes that could actually begin a control frame.
        // Holding a fixed tail would withhold short interactive stderr frames
        // (including the Bun REPL completion sentinel) until the next call.
        let retain = footer.as_ref().map_or(0, |marker| {
            (1..=marker.len().min(pending.len()))
                .rev()
                .find(|length| pending.ends_with(&marker[..*length]))
                .unwrap_or(0)
        });
        let count = pending.len().saturating_sub(retain);
        if let Err(error) = emit(&tx, &pending[..count], stderr, &total).await {
            let _ = completed.send(Err(error)).await;
            return;
        }
        pending.drain(..count);
    }
}
