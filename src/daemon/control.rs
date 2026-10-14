//! The daemon's local socket: the management view and `runo daemon …` on
//! this device ask it about pairing and clients. One JSON line each way;
//! only the daemon's own account can reach it (its directory is 0700).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{Daemon, log, socket_path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Local {
    /// `DaemonState`.
    State,
    Pairing {
        open: bool,
    },
    Revoke {
        client: String,
    },
}

#[derive(Serialize, Deserialize)]
struct Answer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default)]
    ok: Value,
}

pub fn listen(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("the daemon is already running ({})", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let answer = match serde_json::from_str::<Local>(&line) {
                    Ok(request) => match answer(&daemon, request) {
                        Ok(ok) => Answer { error: None, ok },
                        Err(err) => Answer {
                            error: Some(format!("{err:#}")),
                            ok: Value::Null,
                        },
                    },
                    Err(err) => Answer {
                        error: Some(format!("malformed request: {err}")),
                        ok: Value::Null,
                    },
                };
                let mut json = serde_json::to_vec(&answer).unwrap();
                json.push(b'\n');
                if write.write_all(&json).await.is_err() {
                    break;
                }
            }
        });
    }
}

fn answer(daemon: &Daemon, request: Local) -> Result<Value> {
    Ok(match request {
        Local::State => serde_json::to_value(daemon.state()?)?,
        Local::Pairing { open: true } => {
            log("pairing window opened on the device");
            serde_json::to_value(daemon.open_pairing()?)?
        }
        Local::Pairing { open: false } => serde_json::to_value(daemon.close_pairing())?,
        Local::Revoke { client } => serde_json::to_value(daemon.revoke(&client)?)?,
    })
}

/// Asks the daemon running on this device's state; an error says it is not
/// running.
pub async fn request<T: serde::de::DeserializeOwned>(state: &Path, request: Local) -> Result<T> {
    let path = socket_path(state);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("the daemon is not running ({})", path.display()))?;
    let (read, mut write) = stream.into_split();
    let mut json = serde_json::to_vec(&request)?;
    json.push(b'\n');
    write.write_all(&json).await?;
    let line = BufReader::new(read)
        .lines()
        .next_line()
        .await?
        .context("the daemon gave no answer")?;
    let answer: Answer = serde_json::from_str(&line)?;
    if let Some(error) = answer.error {
        bail!("{error}");
    }
    Ok(serde_json::from_value(answer.ok)?)
}
