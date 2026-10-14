//! The client end of a link: connects, checks the device, pairs, makes
//! calls and carries terminal and file channels. One reader task routes
//! what arrives; one writer task sends.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::frame::{self, CONTROL, Frame, MAX_PAYLOAD};
use crate::identity::Identity;
use crate::proto::{
    Call, Down, Event, FileInfo, Kind, PairNonce, Paired, Request, Session, VERSION,
};
use crate::{hex, pair, tls};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What arrives on a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Data(Vec<u8>),
    /// An empty frame: a file's end.
    End,
    /// The session ended.
    Exit(Option<i32>),
    /// A file put was written (`None`) or failed.
    FileDone(Option<String>),
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

struct Inner {
    out: mpsc::UnboundedSender<Frame>,
    pending: Mutex<Pending>,
    channels: Mutex<HashMap<u32, mpsc::UnboundedSender<Incoming>>>,
    /// Why the connection closed, once it has.
    closed: Mutex<Option<String>>,
    next_id: AtomicU64,
    next_channel: AtomicU32,
    device: Welcome,
    /// Whether the device still waits for the password (`Welcome::login`).
    login: AtomicBool,
    device_key: [u8; 32],
    session_secret: [u8; 32],
    address: SocketAddr,
}

/// What the device said about itself and this client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub device: String,
    pub name: String,
    pub version: u32,
    pub trusted: bool,
    pub pairing: bool,
    /// Whether it must log in with the device password first: a trusted
    /// client reaching the device from outside the local network.
    pub login: bool,
}

#[derive(Clone)]
pub struct Link {
    inner: Arc<Inner>,
}

/// A terminal or file channel.
pub struct Channel {
    pub channel: u32,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    link: Link,
}

impl Channel {
    /// The next thing to arrive; `None` once the connection is gone.
    pub async fn recv(&mut self) -> Option<Incoming> {
        self.incoming.recv().await
    }

    /// Sends input (a terminal's keys, a file's bytes).
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.link.send(self.channel, data)
    }

    pub fn link(&self) -> &Link {
        &self.link
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.link
            .inner
            .channels
            .lock()
            .unwrap()
            .remove(&self.channel);
    }
}

impl Link {
    /// Connects to the first of `addresses` that answers. With `pinned`
    /// the device must hold that key; without (only to pair) any will do,
    /// and [`Link::device_key`] says which it was.
    pub async fn connect(
        addresses: &[SocketAddr],
        identity: &Identity,
        pinned: Option<[u8; 32]>,
    ) -> Result<Self> {
        if addresses.is_empty() {
            bail!("no address to connect to");
        }
        let mut last = None;
        for address in addresses {
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => return Self::over(stream, identity, pinned).await,
                Ok(Err(err)) => last = Some(anyhow!("{address}: {err}")),
                Err(_) => last = Some(anyhow!("{address}: no answer")),
            }
        }
        Err(last.unwrap())
    }

    async fn over(
        stream: TcpStream,
        identity: &Identity,
        pinned: Option<[u8; 32]>,
    ) -> Result<Self> {
        stream.set_nodelay(true).ok();
        let address = stream.peer_addr()?;
        let connector = TlsConnector::from(tls::client_config(identity, pinned)?);
        let name = ServerName::try_from(tls::SERVER_NAME).unwrap();
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, stream))
            .await
            .context("TLS handshake: no answer")?
            .context("TLS handshake")?;
        let (device_key, session_secret) = {
            let (_, connection) = stream.get_ref();
            let key = tls::peer_key(connection).context("the device showed no key")?;
            (key, tls::session_secret(connection)?)
        };
        let first = tokio::time::timeout(CONNECT_TIMEOUT, frame::read(&mut stream))
            .await
            .context("the device said nothing")??
            .context("the device closed the connection")?;
        let device = match parse_down(&first)? {
            Down::Event(Event::Welcome {
                device,
                name,
                version,
                trusted,
                pairing,
                login,
            }) => Welcome {
                device,
                name,
                version,
                trusted,
                pairing,
                login,
            },
            Down::Event(Event::Closed { reason }) => bail!("the device refused: {reason}"),
            other => bail!("unexpected first message: {other:?}"),
        };
        if device.version != VERSION {
            bail!(
                "the device speaks protocol version {}, this client {VERSION}: update the older one",
                device.version
            );
        }
        if device.device != crate::device_id(&device_key) {
            bail!("the device's ID does not match its key");
        }
        let (read, mut write) = tokio::io::split(stream);
        let (out, mut outgoing) = mpsc::unbounded_channel::<Frame>();
        tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                if frame::write(&mut write, &frame).await.is_err() {
                    break;
                }
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
        });
        let link = Self {
            inner: Arc::new(Inner {
                out,
                pending: Mutex::new(HashMap::new()),
                channels: Mutex::new(HashMap::new()),
                closed: Mutex::new(None),
                next_id: AtomicU64::new(1),
                next_channel: AtomicU32::new(1),
                login: AtomicBool::new(device.login),
                device,
                device_key,
                session_secret,
                address,
            }),
        };
        tokio::spawn(read_loop(read, Arc::downgrade(&link.inner)));
        Ok(link)
    }

    pub fn welcome(&self) -> &Welcome {
        &self.inner.device
    }

    /// The address it answered on.
    pub fn address(&self) -> SocketAddr {
        self.inner.address
    }

    pub fn device_key(&self) -> [u8; 32] {
        self.inner.device_key
    }

    /// Why the connection closed, once it has.
    pub fn closed(&self) -> Option<String> {
        self.inner.closed.lock().unwrap().clone()
    }

    /// Makes `request` and waits for its answer.
    pub async fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (done, answer) = oneshot::channel();
        {
            // Checked under the lock the reader clears it with, so a call
            // never waits on a connection already gone.
            let mut pending = self.inner.pending.lock().unwrap();
            if self.closed().is_some() {
                bail!("{}", self.closed_reason());
            }
            pending.insert(id, done);
        }
        let data = serde_json::to_vec(&Call { id, request })?;
        self.queue(Frame {
            channel: CONTROL,
            data,
        })?;
        let value = match answer.await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => bail!("{error}"),
            Err(_) => bail!("{}", self.closed_reason()),
        };
        serde_json::from_value(value).context("reading the device's answer")
    }

    fn closed_reason(&self) -> String {
        self.closed()
            .unwrap_or_else(|| "the connection closed".to_owned())
    }

    fn queue(&self, frame: Frame) -> Result<()> {
        self.inner
            .out
            .send(frame)
            .map_err(|_| anyhow!("{}", self.closed_reason()))
    }

    /// Sends `data` on `channel`, in frames small enough.
    pub fn send(&self, channel: u32, data: &[u8]) -> Result<()> {
        for piece in data.chunks(MAX_PAYLOAD) {
            self.queue(Frame {
                channel,
                data: piece.to_vec(),
            })?;
        }
        Ok(())
    }

    fn channel(&self) -> Channel {
        let channel = self.inner.next_channel.fetch_add(1, Ordering::Relaxed);
        let (sender, incoming) = mpsc::unbounded_channel();
        let mut channels = self.inner.channels.lock().unwrap();
        if self.closed().is_none() {
            channels.insert(channel, sender);
        }
        drop(channels);
        Channel {
            channel,
            incoming,
            link: self.clone(),
        }
    }

    /// Runs the pairing exchange; the code to show the person, who checks
    /// it against the device's before [`Link::finish_pairing`].
    pub async fn begin_pairing(&self) -> Result<String> {
        let mine = pair::nonce();
        let theirs: PairNonce = self
            .call(Request::PairBegin {
                commit: hex::encode(&pair::commit(&mine)),
            })
            .await?;
        let theirs: pair::Nonce =
            hex::decode_array(&theirs.nonce).context("the device's nonce is malformed")?;
        let () = self
            .call(Request::PairReveal {
                nonce: hex::encode(&mine),
            })
            .await?;
        Ok(pair::code(&self.inner.session_secret, &theirs, &mine))
    }

    /// Whether the device waits for [`Link::login`] before anything else.
    pub fn needs_login(&self) -> bool {
        self.inner.login.load(Ordering::Relaxed)
    }

    /// Logs in with the device password, from outside the local network.
    pub async fn login(&self, password: &str) -> Result<()> {
        let () = self
            .call(Request::Login {
                password: password.to_owned(),
            })
            .await?;
        self.inner.login.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Sends the device password once the codes match; the device then
    /// trusts this client.
    pub async fn finish_pairing(&self, password: &str, name: &str) -> Result<Paired> {
        self.call(Request::PairFinish {
            password: password.to_owned(),
            name: name.to_owned(),
        })
        .await
    }

    /// A new terminal session.
    pub async fn open(&self, kind: Kind, cols: u16, rows: u16) -> Result<(Session, Channel)> {
        let channel = self.channel();
        let session = self
            .call(Request::Open {
                channel: channel.channel,
                kind,
                cols,
                rows,
            })
            .await?;
        Ok((session, channel))
    }

    /// Shows an existing session: its recent output, then live.
    pub async fn attach(&self, session: u64, cols: u16, rows: u16) -> Result<(Session, Channel)> {
        let channel = self.channel();
        let session = self
            .call(Request::Attach {
                channel: channel.channel,
                session,
                cols,
                rows,
            })
            .await?;
        Ok((session, channel))
    }

    pub async fn resize(&self, channel: u32, cols: u16, rows: u16) -> Result<()> {
        self.call(Request::Resize {
            channel,
            cols,
            rows,
        })
        .await
    }

    pub async fn detach(&self, channel: u32) -> Result<()> {
        self.call(Request::Detach { channel }).await
    }

    /// Fetches the file at `path` on the device.
    pub async fn get_file(&self, path: &str) -> Result<(FileInfo, Vec<u8>)> {
        let mut channel = self.channel();
        let info: FileInfo = self
            .call(Request::FileGet {
                channel: channel.channel,
                path: path.to_owned(),
            })
            .await?;
        let mut bytes = Vec::with_capacity(info.size.min(64 << 20) as usize);
        loop {
            match channel.recv().await {
                Some(Incoming::Data(data)) => bytes.extend_from_slice(&data),
                Some(Incoming::End) => break,
                Some(_) => {}
                None => bail!("{}", self.closed_reason()),
            }
        }
        if bytes.len() as u64 != info.size {
            bail!("got {} of {} bytes", bytes.len(), info.size);
        }
        Ok((info, bytes))
    }

    /// Writes `size` bytes from `reader` to `path` on the device.
    pub async fn put_file<R: AsyncRead + Unpin>(
        &self,
        path: &str,
        size: u64,
        mode: Option<u32>,
        mut reader: R,
    ) -> Result<()> {
        let mut channel = self.channel();
        let () = self
            .call(Request::FilePut {
                channel: channel.channel,
                path: path.to_owned(),
                size,
                mode,
            })
            .await?;
        let mut buffer = vec![0u8; MAX_PAYLOAD];
        let mut sent = 0u64;
        while sent < size {
            let want = (size - sent).min(buffer.len() as u64) as usize;
            let read = reader.read(&mut buffer[..want]).await?;
            if read == 0 {
                bail!("the file ended after {sent} of {size} bytes");
            }
            channel.send(&buffer[..read])?;
            sent += read as u64;
        }
        // An empty frame ends the file.
        self.queue(Frame {
            channel: channel.channel,
            data: vec![],
        })?;
        loop {
            match channel.recv().await {
                Some(Incoming::FileDone(None)) => return Ok(()),
                Some(Incoming::FileDone(Some(error))) => bail!("{error}"),
                Some(_) => {}
                None => bail!("{}", self.closed_reason()),
            }
        }
    }
}

fn parse_down(frame: &Frame) -> Result<Down> {
    if frame.channel != CONTROL {
        bail!("data before the welcome");
    }
    serde_json::from_slice(&frame.data).context("a malformed message from the device")
}

async fn read_loop<R: AsyncRead + Unpin>(mut read: R, inner: std::sync::Weak<Inner>) {
    let reason = loop {
        let frame = match frame::read(&mut read).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break "the device closed the connection".to_owned(),
            Err(err) => break format!("the connection broke: {err}"),
        };
        let Some(inner) = inner.upgrade() else { return };
        if frame.channel != CONTROL {
            let channels = inner.channels.lock().unwrap();
            if let Some(sender) = channels.get(&frame.channel) {
                let incoming = if frame.data.is_empty() {
                    Incoming::End
                } else {
                    Incoming::Data(frame.data)
                };
                let _ = sender.send(incoming);
            }
            continue;
        }
        let Ok(down) = serde_json::from_slice::<Down>(&frame.data) else {
            continue;
        };
        match down {
            Down::Reply { re, error, ok } => {
                if let Some(done) = inner.pending.lock().unwrap().remove(&re) {
                    let _ = done.send(match error {
                        Some(error) => Err(error),
                        None => Ok(ok),
                    });
                }
            }
            Down::Event(Event::Exit { channel, code }) => {
                if let Some(sender) = inner.channels.lock().unwrap().get(&channel) {
                    let _ = sender.send(Incoming::Exit(code));
                }
            }
            Down::Event(Event::FileDone { channel, error }) => {
                if let Some(sender) = inner.channels.lock().unwrap().get(&channel) {
                    let _ = sender.send(Incoming::FileDone(error));
                }
            }
            Down::Event(Event::Closed { reason }) => {
                *inner.closed.lock().unwrap() = Some(reason);
            }
            Down::Event(Event::Welcome { .. }) => {}
        }
    };
    if let Some(inner) = inner.upgrade() {
        let mut closed = inner.closed.lock().unwrap();
        if closed.is_none() {
            *closed = Some(reason);
        }
        drop(closed);
        inner.pending.lock().unwrap().clear();
        inner.channels.lock().unwrap().clear();
    }
}

/// A device this client has paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Its public key, hex: the only thing that proves it is this device.
    pub key: String,
    /// Where it was last reached; only hints.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Its own domain name, for reaching it from outside the LAN.
    #[serde(default)]
    pub domain: Option<String>,
    /// Unix time.
    pub paired: i64,
}

impl Device {
    pub fn key(&self) -> Option<[u8; 32]> {
        hex::decode_array(&self.key)
    }

    pub fn socket_addresses(&self) -> Vec<SocketAddr> {
        self.addresses
            .iter()
            .filter_map(|a| a.parse().ok())
            .collect()
    }
}

/// A client's identity and paired devices, kept in one directory.
pub struct Home {
    dir: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct DeviceFile {
    devices: Vec<Device>,
}

impl Home {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    pub fn identity(&self) -> Result<Identity> {
        Identity::load_or_create(&self.dir.join("identity.pem"))
    }

    fn devices_path(&self) -> PathBuf {
        self.dir.join("devices.json")
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        match std::fs::read_to_string(self.devices_path()) {
            Ok(text) => Ok(serde_json::from_str::<DeviceFile>(&text)
                .with_context(|| format!("reading {}", self.devices_path().display()))?
                .devices),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(err) => Err(err.into()),
        }
    }

    fn save(&self, devices: Vec<Device>) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec_pretty(&DeviceFile { devices })?;
        crate::identity::write_private(&self.devices_path(), &json)
    }

    /// Adds or replaces the device with `device.id`.
    pub fn remember(&self, device: Device) -> Result<()> {
        let mut devices = self.devices()?;
        devices.retain(|d| d.id != device.id);
        devices.push(device);
        self.save(devices)
    }

    /// Notes where a device was reached, first.
    pub fn reached(&self, id: &str, address: SocketAddr) -> Result<()> {
        let mut devices = self.devices()?;
        if let Some(device) = devices.iter_mut().find(|d| d.id == id) {
            let address = address.to_string();
            device.addresses.retain(|a| *a != address);
            device.addresses.insert(0, address);
            device.addresses.truncate(8);
            self.save(devices)?;
        }
        Ok(())
    }

    pub fn forget(&self, id: &str) -> Result<bool> {
        let mut devices = self.devices()?;
        let before = devices.len();
        devices.retain(|d| d.id != id);
        let removed = devices.len() != before;
        self.save(devices)?;
        Ok(removed)
    }

    /// The device `query` names: its ID, the start of its ID, or its name.
    pub fn find(&self, query: &str) -> Result<Device> {
        let devices = self.devices()?;
        let query_lower = query.to_lowercase();
        let matches: Vec<&Device> = devices
            .iter()
            .filter(|d| {
                d.id == query_lower
                    || d.name == query
                    || (query_lower.len() >= 4 && d.id.starts_with(&query_lower))
            })
            .collect();
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no paired device {query:?}"),
            _ => bail!("{query:?} names more than one device: use its ID"),
        }
    }
}
