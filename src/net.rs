//! The network proxy (`network.proxy`): the model providers, ChatGPT's
//! sign-in, search, voice and updates go through it; chat apps (their
//! WebSockets too) only when their own `proxy` switch is on. Addresses on this host and the local network never do, and
//! neither does `web_fetch`, which checks every address itself.

use std::sync::RwLock;

use anyhow::{Context, Result, bail};
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Hosts reached directly: this machine and private networks.
const DIRECT: &str = "localhost,127.0.0.0/8,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16,fc00::/7,fe80::/10";

static PROXY: RwLock<Option<reqwest::Url>> = RwLock::new(None);

/// Reads a proxy address as typed: `http://host:port`, with or without
/// the scheme, optionally `user:password@`.
pub fn parse(text: &str) -> Result<reqwest::Url> {
    let text = text.trim();
    let with_scheme = if text.contains("://") {
        text.to_owned()
    } else {
        format!("http://{text}")
    };
    let url = reqwest::Url::parse(&with_scheme)
        .with_context(|| format!("{text:?} is not a proxy address"))?;
    if url.scheme() != "http" {
        bail!(
            "only http:// proxies are supported, not {}://",
            url.scheme()
        );
    }
    if url.host_str().is_none_or(str::is_empty) || url.port_or_known_default().is_none() {
        bail!("{text:?} has no host and port");
    }
    Ok(url)
}

/// Uses `proxy` from now on; `None` (or an empty text) goes direct. An
/// address that does not parse goes direct too, with a warning.
pub fn set(proxy: Option<&str>) {
    let url = proxy
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .and_then(|p| {
            parse(p)
                .map_err(|err| eprintln!("network.proxy is ignored: {err:#}"))
                .ok()
        });
    *PROXY.write().unwrap_or_else(|e| e.into_inner()) = url;
}

/// The proxy in use.
pub fn current() -> Option<reqwest::Url> {
    PROXY.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A client builder that goes through the proxy when one is set.
pub fn builder() -> reqwest::ClientBuilder {
    builder_for(true)
}

/// A client builder for a service with its own proxy switch: through the
/// proxy only when `proxy` is on and one is set.
pub fn builder_for(proxy: bool) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if !proxy {
        return builder.no_proxy();
    }
    match current().and_then(|url| reqwest::Proxy::all(url.as_str()).ok()) {
        Some(proxy) => builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_string(DIRECT))),
        None => builder,
    }
}

/// Whether `host` is reached without the proxy.
fn direct(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Opens a WebSocket at `url`, through the proxy (an HTTP CONNECT tunnel)
/// when `proxy` is on and one is set.
pub async fn websocket(url: &str, proxy: bool) -> Result<Socket> {
    let target = reqwest::Url::parse(url).with_context(|| format!("{url:?} is not a URL"))?;
    let host = target.host_str().context("the WebSocket URL has no host")?;
    let port = target
        .port_or_known_default()
        .or(match target.scheme() {
            "wss" => Some(443),
            "ws" => Some(80),
            _ => None,
        })
        .context("the WebSocket URL has no port")?;
    let proxy = current().filter(|_| proxy && !direct(host));
    let Some(proxy) = proxy else {
        let (socket, _) = tokio_tungstenite::connect_async(url).await?;
        return Ok(socket);
    };
    let stream = tunnel(&proxy, host, port).await?;
    let (socket, _) = tokio_tungstenite::client_async_tls(url, stream).await?;
    Ok(socket)
}

/// A TCP stream to `host:port` through the HTTP proxy at `proxy`.
async fn tunnel(proxy: &reqwest::Url, host: &str, port: u16) -> Result<TcpStream> {
    let proxy_host = proxy.host_str().context("the proxy has no host")?;
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let mut stream = TcpStream::connect((proxy_host, proxy_port))
        .await
        .with_context(|| format!("cannot reach the proxy {proxy_host}:{proxy_port}"))?;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if !proxy.username().is_empty() {
        let user = percent_decode(proxy.username());
        let password = percent_decode(proxy.password().unwrap_or(""));
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    // The reply ends with an empty line; nothing follows it until the
    // tunnel is used, so reading byte by byte takes no more than it.
    let mut reply = Vec::new();
    while !reply.ends_with(b"\r\n\r\n") {
        if reply.len() > 16 * 1024 {
            bail!("the proxy's reply is too long");
        }
        let byte = stream
            .read_u8()
            .await
            .context("the proxy closed the connection")?;
        reply.push(byte);
    }
    let status = String::from_utf8_lossy(&reply);
    let line = status.lines().next().unwrap_or_default();
    if line.split_whitespace().nth(1) != Some("200") {
        bail!("the proxy refused {authority}: {line}");
    }
    Ok(stream)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One part of a `multipart/form-data` body: a text field, or a file
/// with its name and MIME type.
pub enum Part<'a> {
    Text(&'a str),
    File {
        name: &'a str,
        mime: &'a str,
        data: &'a [u8],
    },
}

/// A `multipart/form-data` body and its `Content-Type`, for the uploads
/// that take no JSON.
pub fn multipart(fields: &[(&str, Part<'_>)]) -> (String, Vec<u8>) {
    let boundary = format!("runo-{:032x}", rand_u128());
    let mut body = Vec::new();
    for (field, part) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match part {
            Part::Text(value) => {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{field}\"\r\n\r\n").as_bytes(),
                );
                body.extend_from_slice(value.as_bytes());
            }
            Part::File { name, mime, data } => {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\n\
                         Content-Type: {mime}\r\n\r\n"
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(data);
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// A boundary nobody's file contains by chance.
fn rand_u128() -> u128 {
    use std::hash::{BuildHasher, Hasher};
    let mut out = 0u128;
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        out = (out << 64) | u128::from(h.finish());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proxy_addresses() {
        assert_eq!(
            parse("127.0.0.1:7890").unwrap().as_str(),
            "http://127.0.0.1:7890/"
        );
        assert_eq!(
            parse(" http://u:p@proxy.lan:8080 ").unwrap().username(),
            "u"
        );
        assert!(parse("socks5://127.0.0.1:1080").is_err());
        assert!(parse("http://").is_err());
        assert!(parse("not a url at all").is_err());
    }

    #[test]
    fn local_hosts_go_direct() {
        for host in ["localhost", "127.0.0.1", "192.168.1.5", "10.0.0.2", "[::1]"] {
            assert!(direct(host), "{host}");
        }
        for host in ["api.sgroup.qq.com", "8.8.8.8", "open.feishu.cn"] {
            assert!(!direct(host), "{host}");
        }
    }

    #[tokio::test]
    async fn websockets_tunnel_through_the_proxy() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        // An echo WebSocket server.
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = server.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if message.is_text() {
                    ws.send(message).await.unwrap();
                }
            }
        });
        // A proxy that checks its credentials and tunnels to the server.
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut client, _) = proxy.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(client.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            seen_tx.send(head.clone()).unwrap();
            let target = head.split_whitespace().nth(1).unwrap().to_owned();
            let mut upstream = TcpStream::connect(target).await.unwrap();
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut client, &mut upstream)
                .await
                .ok();
        });
        let proxy_url = parse(&format!("http://me:s%40cret@{proxy_addr}")).unwrap();
        // The server is on 127.0.0.1, which goes direct, so tunnel by hand.
        let stream = tunnel(&proxy_url, "127.0.0.1", server_addr.port())
            .await
            .unwrap();
        let url = format!("ws://{server_addr}/");
        let (mut ws, _) = tokio_tungstenite::client_async_tls(url.as_str(), stream)
            .await
            .unwrap();
        ws.send(Message::text("hi")).await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert_eq!(echoed.into_text().unwrap().as_str(), "hi");
        let head = seen_rx.await.unwrap();
        assert!(
            head.starts_with(&format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1",
                server_addr.port()
            )),
            "{head}"
        );
        let token = base64::engine::general_purpose::STANDARD.encode("me:s@cret");
        assert!(
            head.contains(&format!("Proxy-Authorization: Basic {token}")),
            "{head}"
        );
    }

    #[test]
    fn multipart_bodies_hold_text_and_files() {
        let (kind, body) = multipart(&[
            ("image_type", Part::Text("message")),
            (
                "image",
                Part::File {
                    name: "a.png",
                    mime: "image/png",
                    data: &[1, 2],
                },
            ),
        ]);
        let boundary = kind.strip_prefix("multipart/form-data; boundary=").unwrap();
        let mut expected = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image_type\"\r\n\r\nmessage\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\n\
             Content-Type: image/png\r\n\r\n"
        )
        .into_bytes();
        expected.extend_from_slice(&[1, 2]);
        expected.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        assert_eq!(body, expected);
    }
}
