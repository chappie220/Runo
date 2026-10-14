//! One management connection: TLS, then the welcome, then calls and
//! channel data until either side closes or the client's trust is taken
//! away.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use runo_link::frame::{self, CONTROL, Frame};
use runo_link::lan::is_lan;
use runo_link::proto::{self, Call, Down, Event, Request};
use runo_link::{device_id, hex, pair, tls};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use super::outbox::{Outbox, outbox};
use super::{Connection, Daemon, PendingPairing, log, pty};
use crate::i18n::Lang;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of a file may wait in the queue before reading more of it.
const FILE_QUEUE: usize = 4 << 20;

#[derive(Default)]
struct Pairing {
    device_nonce: Option<pair::Nonce>,
    commit: Option<Vec<u8>>,
    code: Option<String>,
}

/// A file arriving on a channel.
struct Put {
    file: std::fs::File,
    temp: PathBuf,
    path: PathBuf,
    size: u64,
    written: u64,
    mode: Option<u32>,
    error: Option<String>,
}

struct Conn {
    id: u64,
    peer: SocketAddr,
    key: [u8; 32],
    secret: [u8; 32],
    daemon: Arc<Daemon>,
    connection: Arc<Connection>,
    outbox: Outbox,
    pairing: Pairing,
    puts: HashMap<u32, Put>,
    /// A trusted client outside the LAN that has not logged in yet.
    login: bool,
}

pub async fn handle(
    daemon: Arc<Daemon>,
    id: u64,
    stream: TcpStream,
    peer: SocketAddr,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, daemon.tls.accept(stream))
        .await
        .context("TLS handshake: no answer")?
        .context("TLS handshake")?;
    let (key, secret) = {
        let (_, connection) = stream.get_ref();
        (
            tls::peer_key(connection).context("the client showed no key")?,
            tls::session_secret(connection)?,
        )
    };
    let client = daemon.trust.client(&key);
    let (read, mut write) = tokio::io::split(stream);
    let (outbox, mut outgoing) = outbox();
    let writer = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            if frame::write(&mut write, &frame).await.is_err() {
                break;
            }
        }
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
    });
    if client.is_none() && !from_lan(&daemon, peer.ip()) {
        log(&format!(
            "{peer}: refused an unpaired client from outside the LAN"
        ));
        outbox.event(Event::Closed {
            reason: "this client is not paired with the device, and pairing works only on the local network".into(),
        });
        drop(outbox);
        let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
        return Ok(());
    }
    outbox.event(Event::Welcome {
        device: daemon.id.clone(),
        name: daemon.name.clone(),
        version: proto::VERSION,
        trusted: client.is_some(),
        pairing: daemon.pairing().open,
        login: client.is_some() && !from_lan(&daemon, peer.ip()),
    });
    let (kill, mut killed) = tokio::sync::watch::channel(None::<String>);
    let connection = Arc::new(Connection {
        client: Mutex::new(client.as_ref().map(|c| c.id.clone())),
        kill,
    });
    daemon
        .connections
        .lock()
        .unwrap()
        .insert(id, connection.clone());
    match &client {
        Some(client) => {
            daemon.trust.seen(&client.id);
            log(&format!(
                "{peer}: client {} ({}) connected",
                client.id, client.name
            ));
        }
        None => log(&format!(
            "{peer}: unpaired client {} connected",
            device_id(&key)
        )),
    }
    let mut conn = Conn {
        id,
        peer,
        key,
        secret,
        daemon: daemon.clone(),
        connection,
        outbox,
        pairing: Pairing::default(),
        puts: HashMap::new(),
        login: client.is_some() && !from_lan(&daemon, peer.ip()),
    };
    let mut read = read;
    loop {
        tokio::select! {
            frame = frame::read(&mut read) => match frame {
                Ok(Some(frame)) => conn.frame(frame).await,
                Ok(None) => break,
                Err(err) => {
                    log(&format!("{peer}: {err}"));
                    break;
                }
            },
            _ = killed.changed() => {
                let reason = killed.borrow().clone();
                if let Some(reason) = reason {
                    conn.outbox.event(Event::Closed { reason });
                }
                break;
            }
        }
    }
    daemon.connections.lock().unwrap().remove(&id);
    daemon.pending.lock().unwrap().remove(&id);
    daemon.sessions.detach_connection(id);
    for (_, put) in conn.puts.drain() {
        let _ = std::fs::remove_file(&put.temp);
    }
    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    log(&format!("{peer}: disconnected"));
    Ok(())
}

/// Whether `ip` is on the local network, where pairing works and no
/// login is asked.
fn from_lan(daemon: &Daemon, ip: std::net::IpAddr) -> bool {
    #[cfg(test)]
    if daemon.all_public.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let _ = daemon;
    is_lan(ip)
}

fn lang(lang: Option<&str>) -> Lang {
    match lang {
        Some(code) if code.starts_with("zh") => Lang::Zh,
        Some(_) => Lang::En,
        None => crate::i18n::current(),
    }
}

impl Conn {
    fn trusted(&self) -> Option<String> {
        self.connection.client.lock().unwrap().clone()
    }

    async fn frame(&mut self, frame: Frame) {
        if frame.channel != CONTROL {
            if self.trusted().is_some() && !self.login {
                self.data(frame).await;
            }
            return;
        }
        let call: Call = match serde_json::from_slice(&frame.data) {
            Ok(call) => call,
            Err(err) => {
                self.outbox.down(&Down::Reply {
                    re: 0,
                    error: Some(format!("malformed call: {err}")),
                    ok: Value::Null,
                });
                return;
            }
        };
        let result = self.call(call.request).await;
        let (ok, error) = match result {
            Ok(value) => (value, None),
            Err(err) => (Value::Null, Some(format!("{err:#}"))),
        };
        self.outbox.down(&Down::Reply {
            re: call.id,
            error,
            ok,
        });
    }

    async fn call(&mut self, request: Request) -> Result<Value> {
        let pairing = matches!(
            request,
            Request::PairBegin { .. } | Request::PairReveal { .. } | Request::PairFinish { .. }
        );
        if pairing {
            return self.pair(request).await;
        }
        let Some(client) = self.trusted() else {
            bail!("this client is not paired with the device");
        };
        if let Request::Login { password } = &request {
            return self.log_in(&client, password).await;
        }
        if self.login {
            bail!("log in first: this connection comes from outside the local network");
        }
        let daemon = self.daemon.clone();
        let value = match request {
            Request::Status { lang: code } => {
                let lines = crate::tui::manage::status(&daemon.state, lang(code.as_deref()));
                json(proto::Text {
                    text: lines.join("\n"),
                })
            }
            Request::Logs { lang: code, lines } => {
                let mut text =
                    crate::tui::manage::logs(&daemon.state, lang(code.as_deref())).join("\n");
                if let Some(lines) = lines {
                    let all: Vec<&str> = text.lines().collect();
                    text = all[all.len().saturating_sub(lines)..].join("\n");
                }
                json(proto::Text { text })
            }
            Request::Sessions => json(daemon.sessions.list()),
            Request::Open {
                channel,
                kind,
                cols,
                rows,
            } => {
                pty::check_size(cols, rows)?;
                self.free_channel(channel)?;
                let session = daemon.sessions.open(kind, cols, rows)?;
                session.attach(self.id, channel, self.outbox.clone(), cols, rows);
                log(&format!(
                    "{}: opened session {} ({kind:?})",
                    self.peer, session.id
                ));
                json(session.info())
            }
            Request::Attach {
                channel,
                session,
                cols,
                rows,
            } => {
                pty::check_size(cols, rows)?;
                self.free_channel(channel)?;
                let session = daemon
                    .sessions
                    .get(session)
                    .ok_or_else(|| anyhow!("no session {session}"))?;
                let info = session.info();
                session.attach(self.id, channel, self.outbox.clone(), cols, rows);
                json(info)
            }
            Request::Resize {
                channel,
                cols,
                rows,
            } => {
                pty::check_size(cols, rows)?;
                self.session_on(channel)?.resize(cols, rows);
                Value::Null
            }
            Request::Detach { channel } => {
                self.session_on(channel)?.detach(self.id, channel);
                Value::Null
            }
            Request::Kill { session } => {
                if !daemon.sessions.kill(session) {
                    bail!("no session {session}");
                }
                Value::Null
            }
            Request::FileGet { channel, path } => {
                self.free_channel(channel)?;
                self.get_file(channel, &path).await?
            }
            Request::FilePut {
                channel,
                path,
                size,
                mode,
            } => {
                self.free_channel(channel)?;
                self.put_file(channel, &path, size, mode)?;
                Value::Null
            }
            Request::Clients => json(daemon.clients(Some(&client))?),
            Request::Revoke { client: id } => json(daemon.revoke(&id)?),
            Request::Pairing { open } => {
                if open {
                    json(daemon.open_pairing()?)
                } else {
                    json(daemon.close_pairing())
                }
            }
            Request::Login { .. } => unreachable!(),
            Request::Unconfirmed => json(daemon.unconfirmed()),
            Request::Confirm { change, keep } => {
                let record = crate::changes::History::in_state(&daemon.state)
                    .confirm(change, keep, &format!("client:{client}"))
                    .await?;
                super::log(&format!(
                    "change #{change} {} by client {client}",
                    if keep { "confirmed" } else { "turned down" }
                ));
                json(proto::Text {
                    text: record.report(),
                })
            }
            Request::PairBegin { .. } | Request::PairReveal { .. } | Request::PairFinish { .. } => {
                unreachable!()
            }
        };
        Ok(value)
    }

    fn free_channel(&self, channel: u32) -> Result<()> {
        if channel == CONTROL
            || self.puts.contains_key(&channel)
            || self.daemon.sessions.on_channel(self.id, channel).is_some()
        {
            bail!("channel {channel} is in use");
        }
        Ok(())
    }

    fn session_on(&self, channel: u32) -> Result<Arc<pty::Session>> {
        self.daemon
            .sessions
            .on_channel(self.id, channel)
            .ok_or_else(|| anyhow!("no session on channel {channel}"))
    }

    /// Checks the device password of a trusted client outside the LAN.
    async fn log_in(&mut self, client: &str, password: &str) -> Result<Value> {
        if !self.login {
            return Ok(Value::Null);
        }
        let daemon = self.daemon.clone();
        if let Some(until) = daemon.logins.locked(client, super::now()) {
            bail!(
                "too many wrong passwords: try again after {}",
                super::time(until)
            );
        }
        if !daemon.trust.has_password() {
            bail!(
                "the device has no device password, so it cannot be reached from outside the local network"
            );
        }
        if daemon.trust.check_password(password) {
            daemon.logins.passed(client);
            self.login = false;
            log(&format!("{}: client {client} logged in", self.peer));
            return Ok(Value::Null);
        }
        // Slows guessing down; the lock-out stops it.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let locked = daemon.logins.failed(client, super::now());
        log(&format!(
            "{}: client {client} gave a wrong device password{}",
            self.peer,
            if locked {
                "; locked out for a while"
            } else {
                ""
            }
        ));
        bail!("wrong device password");
    }

    async fn pair(&mut self, request: Request) -> Result<Value> {
        if self.trusted().is_some() {
            bail!("this client is already paired");
        }
        if !from_lan(&self.daemon, self.peer.ip()) {
            bail!("pairing works only on the local network");
        }
        if !self.daemon.pairing().open {
            bail!(
                "the pairing window is closed: open it on the device (Pairing in the management view, or runo daemon pair)"
            );
        }
        match request {
            Request::PairBegin { commit } => {
                let commit = hex::decode(&commit)
                    .filter(|c| c.len() == 32)
                    .context("malformed commitment")?;
                let nonce = pair::nonce();
                self.pairing = Pairing {
                    device_nonce: Some(nonce),
                    commit: Some(commit),
                    code: None,
                };
                Ok(json(proto::PairNonce {
                    nonce: hex::encode(&nonce),
                }))
            }
            Request::PairReveal { nonce } => {
                let (Some(device_nonce), Some(commit)) =
                    (self.pairing.device_nonce, self.pairing.commit.as_ref())
                else {
                    bail!("pairing has not begun");
                };
                if self.pairing.code.is_some() {
                    bail!("the nonce was already revealed");
                }
                let nonce: pair::Nonce = hex::decode_array(&nonce).context("malformed nonce")?;
                if !pair::opens(commit, &nonce) {
                    self.pairing = Pairing::default();
                    bail!("the nonce does not match its commitment");
                }
                let code = pair::code(&self.secret, &device_nonce, &nonce);
                log(&format!("{}: pairing code {code}", self.peer));
                self.daemon.pending.lock().unwrap().insert(
                    self.id,
                    PendingPairing {
                        code: code.clone(),
                        address: self.peer.ip().to_string(),
                        client: device_id(&self.key),
                    },
                );
                self.pairing.code = Some(code);
                Ok(Value::Null)
            }
            Request::PairFinish { password, name } => {
                if self.pairing.code.is_none() {
                    bail!("pairing has not reached its code yet");
                }
                let daemon = self.daemon.clone();
                let right =
                    tokio::task::spawn_blocking(move || daemon.trust.check_password(&password))
                        .await
                        .unwrap_or(false);
                if !right {
                    let closed = self.daemon.window.lock().unwrap().failed();
                    log(&format!("{}: wrong device password", self.peer));
                    if closed {
                        self.daemon.close_pairing();
                        bail!(
                            "wrong device password; too many tries, so the pairing window closed"
                        );
                    }
                    bail!("wrong device password");
                }
                let client = self.daemon.trust.add(&self.key, &name)?;
                *self.connection.client.lock().unwrap() = Some(client.id.clone());
                self.daemon.pending.lock().unwrap().remove(&self.id);
                self.pairing = Pairing::default();
                log(&format!(
                    "{}: paired client {} ({})",
                    self.peer, client.id, client.name
                ));
                // One pairing per opening.
                self.daemon.close_pairing();
                Ok(json(proto::Paired { client: client.id }))
            }
            _ => unreachable!(),
        }
    }

    async fn get_file(&self, channel: u32, path: &str) -> Result<Value> {
        let path = absolute(path)?;
        let mut file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let meta = file.metadata().await?;
        if !meta.is_file() {
            bail!("{} is not a file", path.display());
        }
        use std::os::unix::fs::PermissionsExt;
        let info = proto::FileInfo {
            size: meta.len(),
            mode: meta.permissions().mode() & 0o7777,
        };
        let outbox = self.outbox.clone();
        let size = info.size;
        // After the reply, which goes out first.
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 64 * 1024];
            let mut sent = 0u64;
            while sent < size {
                if !outbox.room(FILE_QUEUE).await {
                    return;
                }
                let want = (size - sent).min(buffer.len() as u64) as usize;
                match file.read(&mut buffer[..want]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        outbox.send(Frame {
                            channel,
                            data: buffer[..n].to_vec(),
                        });
                        sent += n as u64;
                    }
                }
            }
            outbox.send(Frame {
                channel,
                data: vec![],
            });
        });
        Ok(json(info))
    }

    fn put_file(&mut self, channel: u32, path: &str, size: u64, mode: Option<u32>) -> Result<()> {
        let path = absolute(path)?;
        let parent = path.parent().context("no directory")?;
        if !parent.is_dir() {
            bail!("{} is not a directory", parent.display());
        }
        if path.is_dir() {
            bail!("{} is a directory", path.display());
        }
        let name = path.file_name().context("no file name")?.to_string_lossy();
        let temp = parent.join(format!(".{name}.runo-part-{}-{channel}", self.id));
        let file =
            std::fs::File::create(&temp).with_context(|| format!("writing {}", temp.display()))?;
        self.puts.insert(
            channel,
            Put {
                file,
                temp,
                path,
                size,
                written: 0,
                mode: mode.map(|m| m & 0o7777),
                error: None,
            },
        );
        Ok(())
    }

    async fn data(&mut self, frame: Frame) {
        if let Some(put) = self.puts.get_mut(&frame.channel) {
            if !frame.data.is_empty() {
                use std::io::Write;
                put.written += frame.data.len() as u64;
                if put.error.is_none() {
                    if put.written > put.size {
                        put.error = Some(format!("more than the {} bytes announced", put.size));
                    } else if let Err(err) = put.file.write_all(&frame.data) {
                        put.error = Some(format!("writing {}: {err}", put.temp.display()));
                    }
                }
                return;
            }
            let put = self.puts.remove(&frame.channel).unwrap();
            let error = finish_put(put).err().map(|err| format!("{err:#}"));
            self.outbox.event(Event::FileDone {
                channel: frame.channel,
                error,
            });
            return;
        }
        if frame.data.is_empty() {
            return;
        }
        if let Some(session) = self.daemon.sessions.on_channel(self.id, frame.channel) {
            let _ = session.write(&frame.data).await;
        }
    }
}

fn finish_put(put: Put) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let result = (|| {
        if let Some(error) = &put.error {
            bail!("{error}");
        }
        if put.written != put.size {
            bail!("got {} of {} bytes", put.written, put.size);
        }
        put.file.sync_all()?;
        let mode = put.mode.unwrap_or_else(|| {
            std::fs::metadata(&put.path)
                .map(|m| m.permissions().mode() & 0o7777)
                .unwrap_or(0o644)
        });
        std::fs::set_permissions(&put.temp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&put.temp, &put.path)
            .with_context(|| format!("writing {}", put.path.display()))?;
        log(&format!(
            "wrote {} ({} bytes)",
            put.path.display(),
            put.size
        ));
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&put.temp);
    }
    result
}

fn absolute(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        bail!("give an absolute path, not {}", path.display());
    }
    Ok(path.to_path_buf())
}

fn json<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}
