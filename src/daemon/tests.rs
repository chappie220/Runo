use std::net::SocketAddr;
use std::time::Duration;

use runo_link::Identity;
use runo_link::client::{Incoming, Link};
use runo_link::proto::{self, Kind, Request};

use super::*;

async fn daemon(state: &Path) -> (Arc<Daemon>, SocketAddr) {
    let (daemon, listener, local) = start(Options {
        state: state.to_path_buf(),
        config: None,
        listen: Some("127.0.0.1:0".parse().unwrap()),
        announce: false,
    })
    .await
    .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(serve(daemon.clone(), listener, local));
    (daemon, address)
}

async fn until_output(channel: &mut runo_link::client::Channel, want: &str) -> String {
    let mut seen = String::new();
    let wait = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(incoming) = channel.recv().await {
            if let Incoming::Data(data) = incoming {
                seen.push_str(&String::from_utf8_lossy(&data));
                if seen.contains(want) {
                    return;
                }
            }
        }
    })
    .await;
    assert!(wait.is_ok(), "never saw {want:?} in {seen:?}");
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_pairs_on_the_lan_then_uses_the_device_until_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    // No password yet: the window cannot open.
    let (daemon, address) = daemon(state).await;
    assert!(!daemon.pairing().open);
    assert!(daemon.open_pairing().is_err());

    let client = Identity::generate().unwrap();
    let link = Link::connect(&[address], &client, None).await.unwrap();
    assert!(!link.welcome().trusted);
    assert_eq!(link.welcome().device, daemon.id);
    let refused = link
        .call::<proto::Text>(Request::Status { lang: None })
        .await;
    assert!(refused.unwrap_err().to_string().contains("not paired"));
    let closed = link.begin_pairing().await.unwrap_err();
    assert!(closed.to_string().contains("window is closed"), "{closed}");

    daemon.trust.set_password("device password").unwrap();
    let opened: proto::PairingState = control_request(state, Local::Pairing { open: true })
        .await
        .unwrap();
    assert!(opened.open);

    // Both ends show the same code.
    let code = link.begin_pairing().await.unwrap();
    let shown: DaemonState = control_request(state, Local::State).await.unwrap();
    assert_eq!(shown.pending.len(), 1);
    assert_eq!(shown.pending[0].code, code);
    assert_eq!(shown.pending[0].client, client.id());

    let wrong = link.finish_pairing("wrong password", "laptop").await;
    assert!(
        wrong
            .unwrap_err()
            .to_string()
            .contains("wrong device password")
    );
    let paired = link
        .finish_pairing("device password", "laptop")
        .await
        .unwrap();
    assert_eq!(paired.client, client.id());
    // One pairing per opening.
    assert!(!daemon.pairing().open);
    // Now trusted on this very connection.
    let status: proto::Text = link.call(Request::Status { lang: None }).await.unwrap();
    assert!(!status.text.is_empty());
    drop(link);

    // Back with the device's key pinned; a wrong pin is refused.
    let device_key = daemon_key(state);
    let wrong_pin = Link::connect(&[address], &client, Some([9u8; 32])).await;
    assert!(wrong_pin.is_err());
    let link = Link::connect(&[address], &client, Some(device_key))
        .await
        .unwrap();
    assert!(link.welcome().trusted);

    // A shell that outlives its connection.
    let (session, mut channel) = link.open(Kind::Shell, 80, 24).await.unwrap();
    assert!(session.running);
    channel.send(b"echo runo-$((40+2))\n").unwrap();
    until_output(&mut channel, "runo-42").await;
    drop(channel);
    drop(link);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let link = Link::connect(&[address], &client, Some(device_key))
        .await
        .unwrap();
    let sessions: Vec<proto::Session> = link.call(Request::Sessions).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].running);
    let (_, mut channel) = link.attach(session.id, 100, 30).await.unwrap();
    // What it showed before comes again.
    until_output(&mut channel, "runo-42").await;
    channel.send(b"exit\n").unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match channel.recv().await {
                Some(Incoming::Exit(code)) => return code,
                Some(_) => {}
                None => panic!("connection closed"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ended, Some(0));

    // Files both ways.
    let target = dir.path().join("sent.txt");
    let body = vec![b'x'; 300 * 1024];
    link.put_file(
        target.to_str().unwrap(),
        body.len() as u64,
        Some(0o600),
        &body[..],
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), body);
    let (info, back) = link.get_file(target.to_str().unwrap()).await.unwrap();
    assert_eq!(info.mode, 0o600);
    assert_eq!(back, body);
    assert!(link.get_file("relative.txt").await.is_err());
    let short = link
        .put_file(target.to_str().unwrap(), 10, None, &b"abc"[..])
        .await;
    assert!(short.is_err());
    assert_eq!(std::fs::read(&target).unwrap(), body);

    // Revoking cuts the connection at once, and the key is no longer welcome.
    let clients: Vec<proto::Client> = link.call(Request::Clients).await.unwrap();
    assert!(clients[0].this && clients[0].connected);
    control_request::<proto::Client>(
        state,
        Local::Revoke {
            client: client.id(),
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        link.call::<proto::Text>(Request::Status { lang: None })
            .await
            .is_err()
    );
    assert!(link.closed().unwrap().contains("no longer trusted"));
    let again = Link::connect(&[address], &client, Some(device_key))
        .await
        .unwrap();
    assert!(!again.welcome().trusted);
}

fn daemon_key(state: &Path) -> [u8; 32] {
    identity(state).unwrap().public_key()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_window_closes_after_too_many_wrong_passwords() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    Trust::new(&super::dir(state))
        .set_password("device password")
        .unwrap();
    // Nobody trusted yet: it opens by itself.
    let (daemon, address) = daemon(state).await;
    assert!(daemon.pairing().open);
    let link = Link::connect(&[address], &Identity::generate().unwrap(), None)
        .await
        .unwrap();
    assert!(link.welcome().pairing);
    link.begin_pairing().await.unwrap();
    for _ in 0..trust::MAX_FAILURES {
        assert!(link.finish_pairing("nope nope", "x").await.is_err());
    }
    assert!(!daemon.pairing().open);
    let late = link.finish_pairing("device password", "x").await;
    assert!(late.unwrap_err().to_string().contains("window is closed"));
}
