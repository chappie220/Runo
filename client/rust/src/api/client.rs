//! What the Flutter client calls: a thin layer over `runo_link`. The
//! client's identity (an Ed25519 key, PEM) is kept by the app in the
//! platform's keystore and handed in on each call; the paired devices
//! (public keys and address hints only) live in `dir`.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use flutter_rust_bridge::frb;
use runo_link::client::{Channel, Device, Home, Incoming, Link};
use runo_link::proto::{self, Kind, Request, PORT};
use runo_link::{hex, mdns, Identity};

use crate::frb_generated::StreamSink;

/// The runtime every network call runs on, whatever runs the bridge.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

async fn on_runtime<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(future).await.expect("task panicked")
}

#[frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

/// A new client identity, as PEM, for the app to keep.
#[frb(sync)]
pub fn new_identity() -> Result<String> {
    Ok(Identity::generate()?.to_pem())
}

/// The identity kept in `dir` (created there, readable only by this
/// user), for systems without a secure store the app can use.
#[frb(sync)]
pub fn file_identity(dir: String) -> Result<String> {
    Ok(Home::new(Path::new(&dir)).identity()?.to_pem())
}

/// The ID devices know this client by.
#[frb(sync)]
pub fn identity_id(pem: String) -> Result<String> {
    Ok(Identity::from_pem(&pem)?.id())
}

pub struct FoundDevice {
    pub id: String,
    pub name: String,
    pub pairing: bool,
    pub addresses: Vec<String>,
    pub paired: bool,
    /// The device speaks another protocol version.
    pub incompatible: bool,
}

pub struct PairedDevice {
    pub id: String,
    pub name: String,
    pub addresses: Vec<String>,
    /// Unix time.
    pub paired: i64,
}

/// The Runo devices answering on the LAN within `seconds`.
pub async fn discover(dir: String, seconds: u32) -> Result<Vec<FoundDevice>> {
    on_runtime(async move {
        let paired: Vec<String> = Home::new(Path::new(&dir))
            .devices()?
            .into_iter()
            .map(|d| d.id)
            .collect();
        let found = mdns::browse(std::time::Duration::from_secs(seconds.max(1).into())).await?;
        Ok(found
            .into_iter()
            .map(|d| FoundDevice {
                paired: paired.contains(&d.id),
                incompatible: d.version != proto::VERSION,
                id: d.id,
                name: d.name,
                pairing: d.pairing,
                addresses: d.addresses.iter().map(|a| a.to_string()).collect(),
            })
            .collect())
    })
    .await
}

#[frb(sync)]
pub fn paired_devices(dir: String) -> Result<Vec<PairedDevice>> {
    Ok(Home::new(Path::new(&dir))
        .devices()?
        .into_iter()
        .map(|d| PairedDevice {
            id: d.id,
            name: d.name,
            addresses: d.addresses,
            paired: d.paired,
        })
        .collect())
}

#[frb(sync)]
pub fn forget_device(dir: String, id: String) -> Result<bool> {
    Home::new(Path::new(&dir)).forget(&id)
}

/// `text` as addresses: `host:port`, an address, or several of either
/// separated by commas.
async fn addresses(text: &[String]) -> Result<Vec<SocketAddr>> {
    let mut out = vec![];
    for item in text {
        let item = item.trim();
        if let Ok(address) = item.parse::<SocketAddr>() {
            out.push(address);
        } else if let Ok(ip) = item.parse::<std::net::IpAddr>() {
            out.push(SocketAddr::new(ip, PORT));
        } else if item.contains(':') {
            out.extend(tokio::net::lookup_host(item).await?);
        } else if !item.is_empty() {
            out.extend(tokio::net::lookup_host((item, PORT)).await?);
        }
    }
    if out.is_empty() {
        bail!("no address to connect to");
    }
    Ok(out)
}

/// Pairing with one device, between showing the code and the password.
#[frb(opaque)]
pub struct Pairing {
    link: Link,
    dir: String,
    code: Option<String>,
}

impl Pairing {
    /// Connects to `addresses` and, unless the device already trusts this
    /// client, runs the code exchange.
    pub async fn start(dir: String, identity: String, addresses: Vec<String>) -> Result<Pairing> {
        on_runtime(async move {
            let identity = Identity::from_pem(&identity)?;
            let link = Link::connect(&self::addresses(&addresses).await?, &identity, None).await?;
            let code = if link.welcome().trusted {
                None
            } else {
                Some(link.begin_pairing().await?)
            };
            let pairing = Pairing { link, dir, code };
            if pairing.code.is_none() {
                pairing.remember()?;
            }
            Ok(pairing)
        })
        .await
    }

    #[frb(sync, getter)]
    pub fn device_id(&self) -> String {
        self.link.welcome().device.clone()
    }

    #[frb(sync, getter)]
    pub fn device_name(&self) -> String {
        self.link.welcome().name.clone()
    }

    /// The six digits to compare with the device's; none when it already
    /// trusted this client (it is paired then).
    #[frb(sync, getter)]
    pub fn code(&self) -> Option<String> {
        self.code.clone()
    }

    fn remember(&self) -> Result<()> {
        Home::new(Path::new(&self.dir)).remember(Device {
            id: self.link.welcome().device.clone(),
            name: self.link.welcome().name.clone(),
            key: hex::encode(&self.link.device_key()),
            addresses: vec![self.link.address().to_string()],
            domain: None,
            paired: chrono_now(),
        })
    }

    /// Gives the device password once the person saw the codes match.
    pub async fn finish(&self, password: String, client_name: String) -> Result<()> {
        let link = self.link.clone();
        on_runtime(async move {
            link.finish_pairing(&password, &client_name)
                .await
                .map(|_| ())
        })
        .await?;
        self.remember()
    }
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct SessionInfo {
    pub id: u64,
    pub agent: bool,
    pub started: i64,
    pub running: bool,
    pub attached: u32,
}

pub struct ClientInfo {
    pub id: String,
    pub name: String,
    pub added: i64,
    pub last_seen: Option<i64>,
    pub connected: bool,
    pub this: bool,
}

pub struct TerminalEvent {
    /// Output to show; empty for the end.
    pub data: Vec<u8>,
    /// The session ended (or the connection did).
    pub ended: bool,
    pub code: Option<i32>,
    pub error: Option<String>,
}

/// A trusted connection to a paired device.
#[frb(opaque)]
pub struct Connection {
    link: Link,
}

impl Connection {
    /// Connects to the paired device `id`: its last addresses first, then
    /// wherever it answers on the LAN.
    pub async fn open(dir: String, identity: String, id: String) -> Result<Connection> {
        on_runtime(async move {
            let home = Home::new(Path::new(&dir));
            let device = home.find(&id)?;
            let identity = Identity::from_pem(&identity)?;
            let key = device.key().context("the kept device key is malformed")?;
            let first = Link::connect(&device.socket_addresses(), &identity, Some(key)).await;
            let link = match first {
                Ok(link) => link,
                Err(err) => {
                    let found = mdns::browse(std::time::Duration::from_secs(3))
                        .await
                        .unwrap_or_default();
                    match found.into_iter().find(|d| d.id == device.id) {
                        Some(found) => {
                            Link::connect(&found.addresses, &identity, Some(key)).await?
                        }
                        None => return Err(err),
                    }
                }
            };
            if !link.welcome().trusted {
                bail!("{} no longer trusts this client: pair again", device.name);
            }
            home.reached(&device.id, link.address())?;
            Ok(Connection { link })
        })
        .await
    }

    #[frb(sync, getter)]
    pub fn device_name(&self) -> String {
        self.link.welcome().name.clone()
    }

    #[frb(sync, getter)]
    pub fn device_id(&self) -> String {
        self.link.welcome().device.clone()
    }

    /// Why the connection closed, once it has.
    #[frb(sync)]
    pub fn closed(&self) -> Option<String> {
        self.link.closed()
    }

    async fn call<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        request: Request,
    ) -> Result<T> {
        let link = self.link.clone();
        on_runtime(async move { link.call(request).await }).await
    }

    pub async fn status(&self, lang: String) -> Result<String> {
        let text: proto::Text = self.call(Request::Status { lang: Some(lang) }).await?;
        Ok(text.text)
    }

    pub async fn logs(&self, lang: String) -> Result<String> {
        let text: proto::Text = self
            .call(Request::Logs {
                lang: Some(lang),
                lines: None,
            })
            .await?;
        Ok(text.text)
    }

    pub async fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let sessions: Vec<proto::Session> = self.call(Request::Sessions).await?;
        Ok(sessions
            .into_iter()
            .map(|s| SessionInfo {
                id: s.id,
                agent: s.kind == Kind::Agent,
                started: s.started,
                running: s.running,
                attached: s.attached,
            })
            .collect())
    }

    pub async fn kill(&self, session: u64) -> Result<()> {
        self.call(Request::Kill { session }).await
    }

    pub async fn clients(&self) -> Result<Vec<ClientInfo>> {
        let clients: Vec<proto::Client> = self.call(Request::Clients).await?;
        Ok(clients
            .into_iter()
            .map(|c| ClientInfo {
                id: c.id,
                name: c.name,
                added: c.added,
                last_seen: c.last_seen,
                connected: c.connected,
                this: c.this,
            })
            .collect())
    }

    pub async fn revoke(&self, client: String) -> Result<()> {
        let _: proto::Client = self.call(Request::Revoke { client }).await?;
        Ok(())
    }

    /// Opens the device's pairing window for another client.
    pub async fn open_pairing(&self) -> Result<()> {
        let _: proto::PairingState = self.call(Request::Pairing { open: true }).await?;
        Ok(())
    }

    /// Fetches `remote` into the local file `local`; the bytes fetched.
    pub async fn download(&self, remote: String, local: String) -> Result<u64> {
        let link = self.link.clone();
        on_runtime(async move {
            let (info, bytes) = link.get_file(&remote).await?;
            tokio::fs::write(&local, &bytes)
                .await
                .with_context(|| format!("writing {local}"))?;
            Ok(info.size)
        })
        .await
    }

    /// Sends the local file `local` to `remote` on the device.
    pub async fn upload(&self, local: String, remote: String) -> Result<u64> {
        let link = self.link.clone();
        on_runtime(async move {
            let file = tokio::fs::File::open(&local)
                .await
                .with_context(|| format!("opening {local}"))?;
            let size = file.metadata().await?.len();
            link.put_file(&remote, size, None, file).await?;
            Ok(size)
        })
        .await
    }

    /// The Agent (the running one, or a new one), a new shell (`kind`
    /// "shell"), or the session `attach`. Its output comes from
    /// [`Terminal::output`].
    pub async fn terminal(
        &self,
        kind: String,
        attach: Option<u64>,
        cols: u16,
        rows: u16,
    ) -> Result<Terminal> {
        let link = self.link.clone();
        on_runtime(async move {
            let (session, channel) = match (attach, kind.as_str()) {
                (Some(id), _) => link.attach(id, cols, rows).await?,
                (None, "shell") => link.open(Kind::Shell, cols, rows).await?,
                (None, _) => {
                    let sessions: Vec<proto::Session> = link.call(Request::Sessions).await?;
                    match sessions
                        .iter()
                        .rev()
                        .find(|s| s.kind == Kind::Agent && s.running)
                    {
                        Some(running) => link.attach(running.id, cols, rows).await?,
                        None => link.open(Kind::Agent, cols, rows).await?,
                    }
                }
            };
            Ok(Terminal {
                link: link.clone(),
                id: channel.channel,
                session: session.id,
                channel: Mutex::new(Some(channel)),
            })
        })
        .await
    }
}

async fn forward(mut channel: Channel, link: Link, sink: StreamSink<TerminalEvent>) {
    let event =
        |data: Vec<u8>, ended: bool, code: Option<i32>, error: Option<String>| TerminalEvent {
            data,
            ended,
            code,
            error,
        };
    loop {
        match channel.recv().await {
            Some(Incoming::Data(data)) => {
                if sink.add(event(data, false, None, None)).is_err() {
                    let _ = link.detach(channel.channel).await;
                    return;
                }
            }
            Some(Incoming::Exit(code)) => {
                let _ = sink.add(event(vec![], true, code, None));
                return;
            }
            Some(_) => {}
            None => {
                let _ = sink.add(event(vec![], true, None, link.closed()));
                return;
            }
        }
    }
}

/// A terminal session shown in the app.
#[frb(opaque)]
pub struct Terminal {
    link: Link,
    id: u32,
    session: u64,
    /// Until `output` takes it.
    channel: Mutex<Option<Channel>>,
}

impl Terminal {
    #[frb(sync, getter)]
    pub fn session(&self) -> u64 {
        self.session
    }

    /// What the session shows, from its recent past on; once.
    pub fn output(&self, sink: StreamSink<TerminalEvent>) -> Result<()> {
        let channel = self
            .channel
            .lock()
            .unwrap()
            .take()
            .context("the output is already being shown")?;
        runtime().spawn(forward(channel, self.link.clone(), sink));
        Ok(())
    }

    /// Sends typed keys.
    #[frb(sync)]
    pub fn write(&self, data: Vec<u8>) -> Result<()> {
        self.link.send(self.id, &data)
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let (link, channel) = (self.link.clone(), self.id);
        on_runtime(async move { link.resize(channel, cols, rows).await }).await
    }

    /// Stops showing it; it goes on on the device.
    pub async fn detach(&self) -> Result<()> {
        let (link, channel) = (self.link.clone(), self.id);
        on_runtime(async move { link.detach(channel).await }).await
    }
}
