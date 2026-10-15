//! `runo daemon` ("runod", Runo 1.0, 4.1): the management daemon. It needs
//! no model and no config, so the device stays reachable and repairable
//! when the Agent is not.
//!
//! - Announces the device on the LAN (`_runo._tcp`, see `runo_link::mdns`).
//! - Accepts management clients over TLS 1.3, each known by its key.
//!   Unknown clients may only pair, and only from the LAN while the pairing
//!   window is open (4.4).
//! - Holds the terminal sessions (the Agent, shells) so they outlive a
//!   connection (4.3); serves status, logs and files.
//! - Answers the local management view and `runo daemon …` on a Unix
//!   socket in its directory.
//!
//! Its directory, `<state>/runod/`, keeps the device identity, the trusted
//! clients and the device password.

pub mod cli;
mod conn;
mod control;
pub mod ddns;
mod outbox;
mod pty;
pub mod service;
pub mod trust;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use runo_link::Identity;
use runo_link::mdns::Advertiser;
use runo_link::proto::{self, PORT};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use trust::{Logins, Trust, Window, now};

pub use cli::{first_password, set_password};
pub use control::{Local, request as control_request};

pub const DIR: &str = "runod";
const IDENTITY: &str = "identity.pem";
const SOCKET: &str = "control.sock";

pub fn dir(state: &Path) -> PathBuf {
    state.join(DIR)
}

pub fn socket_path(state: &Path) -> PathBuf {
    dir(state).join(SOCKET)
}

/// Whether a daemon answers on this state's socket.
pub fn is_running(state: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path(state)).is_ok()
}

/// The device's identity, made on first use.
pub fn identity(state: &Path) -> Result<Identity> {
    Identity::load_or_create(&dir(state).join(IDENTITY))
}

/// The device's name: its host name.
pub fn device_name() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes to a valid buffer.
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(0);
    let name = String::from_utf8_lossy(&buffer[..end]).trim().to_owned();
    if ok && !name.is_empty() {
        name
    } else {
        "runo".into()
    }
}

pub struct Options {
    pub state: PathBuf,
    pub config: Option<PathBuf>,
    pub listen: Option<SocketAddr>,
    /// Whether to announce on the LAN (off in tests).
    pub announce: bool,
}

/// A pairing in progress, shown on the device so the person can compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPairing {
    pub code: String,
    pub address: String,
    /// The client's ID (from its key).
    pub client: String,
}

/// What the local view shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub pairing: proto::PairingState,
    pub pending: Vec<PendingPairing>,
    pub clients: Vec<proto::Client>,
    pub password: bool,
}

struct Connection {
    client: Mutex<Option<String>>,
    kill: tokio::sync::watch::Sender<Option<String>>,
}

pub struct Daemon {
    pub id: String,
    pub name: String,
    pub port: u16,
    state: PathBuf,
    trust: Trust,
    window: Mutex<Window>,
    logins: Logins,
    /// Tests: every peer counts as outside the LAN.
    #[cfg(test)]
    all_public: std::sync::atomic::AtomicBool,
    pending: Mutex<HashMap<u64, PendingPairing>>,
    connections: Mutex<HashMap<u64, Arc<Connection>>>,
    next_connection: AtomicU64,
    sessions: pty::Sessions,
    advertiser: Mutex<Option<Advertiser>>,
    tls: tokio_rustls::TlsAcceptor,
}

impl Daemon {
    pub fn pairing(&self) -> proto::PairingState {
        let until = self.window.lock().unwrap().until(now());
        proto::PairingState {
            open: until.is_some(),
            until,
        }
    }

    fn announce(&self) {
        let open = self.pairing().open;
        if let Some(advertiser) = self.advertiser.lock().unwrap().as_ref()
            && let Err(err) = advertiser.set_pairing(open)
        {
            log(&format!("mDNS: {err:#}"));
        }
    }

    /// Opens the pairing window; it needs a device password.
    pub fn open_pairing(&self) -> Result<proto::PairingState> {
        if !self.trust.has_password() {
            anyhow::bail!("set a device password first: runo daemon password");
        }
        let until = self.window.lock().unwrap().open(now());
        log(&format!("pairing window open until {}", time(until)));
        self.announce();
        Ok(self.pairing())
    }

    pub fn close_pairing(&self) -> proto::PairingState {
        self.window.lock().unwrap().close();
        self.pending.lock().unwrap().clear();
        log("pairing window closed");
        self.announce();
        self.pairing()
    }

    /// The configuration changes waiting for a client to confirm them.
    pub fn unconfirmed(&self) -> Vec<proto::Unconfirmed> {
        crate::changes::History::in_state(&self.state)
            .unconfirmed()
            .into_iter()
            .map(|r| proto::Unconfirmed {
                id: r.id,
                summary: r.summary,
                until: r.confirm_by.unwrap_or_default(),
            })
            .collect()
    }

    pub fn clients(&self, asking: Option<&str>) -> Result<Vec<proto::Client>> {
        let connected: Vec<String> = self
            .connections
            .lock()
            .unwrap()
            .values()
            .filter_map(|c| c.client.lock().unwrap().clone())
            .collect();
        Ok(self
            .trust
            .clients()?
            .into_iter()
            .map(|c| proto::Client {
                connected: connected.contains(&c.id),
                this: asking == Some(c.id.as_str()),
                id: c.id,
                name: c.name,
                added: c.added,
                last_seen: c.last_seen,
            })
            .collect())
    }

    /// Takes a client's trust away and closes its connections at once.
    pub fn revoke(&self, id: &str) -> Result<proto::Client> {
        let client = self.trust.revoke(id)?;
        log(&format!("revoked client {} ({})", client.id, client.name));
        for connection in self.connections.lock().unwrap().values() {
            if connection.client.lock().unwrap().as_deref() == Some(client.id.as_str()) {
                let _ = connection
                    .kill
                    .send(Some("this client is no longer trusted".into()));
            }
        }
        Ok(proto::Client {
            id: client.id,
            name: client.name,
            added: client.added,
            last_seen: client.last_seen,
            connected: false,
            this: false,
        })
    }

    pub fn state(&self) -> Result<DaemonState> {
        Ok(DaemonState {
            id: self.id.clone(),
            name: self.name.clone(),
            port: self.port,
            pairing: self.pairing(),
            pending: self.pending.lock().unwrap().values().cloned().collect(),
            clients: self.clients(None)?,
            password: self.trust.has_password(),
        })
    }
}

pub fn log(message: &str) {
    eprintln!(
        "{} runod: {message}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

fn time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

async fn bind(listen: Option<SocketAddr>) -> Result<TcpListener> {
    if let Some(address) = listen {
        return TcpListener::bind(address)
            .await
            .with_context(|| format!("listening on {address}"));
    }
    // Both IPv6 and IPv4 where the kernel allows; IPv4 alone otherwise.
    match TcpListener::bind(("::", PORT)).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind(("0.0.0.0", PORT))
            .await
            .with_context(|| format!("listening on port {PORT}")),
    }
}

/// Runs the daemon until it is stopped.
pub async fn run(options: Options) -> Result<()> {
    let daemon = start(options).await?;
    let (daemon, listener, local) = daemon;
    // A system update waiting for this boot is kept or put back (4.5).
    let state = daemon.state.clone();
    tokio::spawn(async move {
        match crate::sysupdate::after_boot(&state).await {
            crate::sysupdate::AfterBoot::Nothing => {}
            crate::sysupdate::AfterBoot::Kept(version) => {
                log(&format!("system update to {version} kept"))
            }
            crate::sysupdate::AfterBoot::PutBack(why) => {
                log(&format!("system update put back: {why}"))
            }
        }
    });
    let watchdog = feed_watchdog();
    let serving = serve(daemon.clone(), listener, local);
    let result = tokio::select! {
        result = serving => result,
        _ = shutdown_signal() => {
            log("stopping");
            daemon.sessions.kill_all();
            Ok(())
        }
    };
    watchdog.stop();
    result
}

/// Feeds the hardware watchdog from its own thread, so a stuck runtime
/// still lets the system restart (4.5).
struct Feeding {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<crate::sysupdate::Watchdog>>,
}

fn feed_watchdog() -> Feeding {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut watchdog = crate::sysupdate::Watchdog::start();
    if !watchdog.active() {
        return Feeding { stop, thread: None };
    }
    let stopped = stop.clone();
    let thread = std::thread::spawn(move || {
        while !stopped.load(Ordering::Relaxed) {
            watchdog.feed();
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        watchdog
    });
    Feeding {
        stop,
        thread: Some(thread),
    }
}

impl Feeding {
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread
            && let Ok(watchdog) = thread.join()
        {
            watchdog.stop();
        }
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Sets the daemon up: identity, listener, local socket, announcement.
pub async fn start(
    options: Options,
) -> Result<(Arc<Daemon>, TcpListener, tokio::net::UnixListener)> {
    let dir = dir(&options.state);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let identity = identity(&options.state)?;
    let listener = bind(options.listen).await?;
    let port = listener.local_addr()?.port();
    let local = control::listen(&socket_path(&options.state))?;
    let runo = std::env::current_exe().context("finding this program")?;
    let tls = tokio_rustls::TlsAcceptor::from(runo_link::tls::server_config(&identity)?);
    let daemon = Arc::new(Daemon {
        id: identity.id(),
        name: device_name(),
        port,
        state: options.state.clone(),
        trust: Trust::new(&dir),
        window: Mutex::new(Window::default()),
        logins: Logins::default(),
        #[cfg(test)]
        all_public: std::sync::atomic::AtomicBool::new(false),
        pending: Mutex::new(HashMap::new()),
        connections: Mutex::new(HashMap::new()),
        next_connection: AtomicU64::new(1),
        sessions: pty::Sessions::new(pty::Programs {
            runo,
            config: options.config.clone(),
        }),
        advertiser: Mutex::new(None),
        tls,
    });
    log(&format!(
        "device {} ({}) on port {port}",
        daemon.id, daemon.name
    ));
    // First start, nobody trusted yet: open the window by itself (4.4).
    if daemon.trust.clients()?.is_empty() {
        match daemon.open_pairing() {
            Ok(_) => {}
            Err(err) => log(&format!("pairing window stays closed: {err}")),
        }
    }
    if options.announce {
        match Advertiser::start(&daemon.id, &daemon.name, port, daemon.pairing().open) {
            Ok(advertiser) => *daemon.advertiser.lock().unwrap() = Some(advertiser),
            Err(err) => log(&format!("not announced on the LAN: {err:#}")),
        }
    }
    Ok((daemon, listener, local))
}

/// Accepts connections and local requests; closes the window when its
/// time is up.
pub async fn serve(
    daemon: Arc<Daemon>,
    listener: TcpListener,
    local: tokio::net::UnixListener,
) -> Result<()> {
    tokio::spawn(control::serve(daemon.clone(), local));
    let watcher = daemon.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            // Closing it otherwise goes through `close_pairing`.
            if watcher.window.lock().unwrap().expired(now()) {
                watcher.close_pairing();
            }
            // A change nobody confirmed in time is put back (4.2).
            let history = crate::changes::History::in_state(&watcher.state);
            for record in history.expire(now()).await {
                log(&format!(
                    "change #{} not confirmed in time: {}",
                    record.id,
                    record.state.word()
                ));
            }
        }
    });
    // Dynamic DNS (4.9): the domain follows the public IPv6 address.
    let state = daemon.state.clone();
    tokio::spawn(async move {
        loop {
            match ddns::round(&dir(&state), now()).await {
                ddns::Round::Updated(address) => log(&format!("dynamic DNS: now {address}")),
                ddns::Round::Failed(why) => log(&format!("dynamic DNS: {why}")),
                _ => {}
            }
            tokio::time::sleep(ddns::EVERY).await;
        }
    });
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                log(&format!("accept: {err}"));
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        let id = daemon.next_connection.fetch_add(1, Ordering::Relaxed);
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(err) = conn::handle(daemon, id, stream, peer).await {
                log(&format!("{peer}: {err:#}"));
            }
        });
    }
}

#[cfg(test)]
mod tests;
