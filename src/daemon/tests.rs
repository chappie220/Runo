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

#[tokio::test(flavor = "multi_thread")]
async fn a_change_that_may_cut_a_client_off_is_kept_by_a_client_or_put_back() {
    use crate::changes::{History, Request as Change, State};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let client = Identity::generate().unwrap();
    Trust::new(&super::dir(state))
        .add(&client.public_key(), "laptop")
        .unwrap();
    let (_daemon, address) = daemon(state).await;
    let link = Link::connect(&[address], &client, Some(daemon_key(state)))
        .await
        .unwrap();
    let conf = dir.path().join("wpa_supplicant.conf");
    std::fs::write(&conf, "old\n").unwrap();
    let history = History::in_state(state);
    let change = |content: &str, wait: u64| Change {
        summary: "wifi".into(),
        actor: "agent".into(),
        files: vec![(conf.clone(), Some(content.as_bytes().to_vec()))],
        services: Vec::new(),
        checks: Vec::new(),
        settle: Duration::ZERO,
        reverts: None,
        confirm: Some(Duration::from_secs(wait)),
    };

    let none: Vec<proto::Unconfirmed> = link.call(Request::Unconfirmed).await.unwrap();
    assert!(none.is_empty());
    let first = history.apply(change("new\n", 120)).await.unwrap();
    let waiting: Vec<proto::Unconfirmed> = link.call(Request::Unconfirmed).await.unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].id, first.id);
    assert_eq!(waiting[0].until, first.confirm_by.unwrap());
    let kept: proto::Text = link
        .call(Request::Confirm {
            change: first.id,
            keep: true,
        })
        .await
        .unwrap();
    assert!(kept.text.contains("confirmed by client:"), "{}", kept.text);
    assert_eq!(history.load(first.id).unwrap().state, State::Applied);

    // Nobody confirms this one: runod puts it back when its time is up.
    let second = history.apply(change("newer\n", 1)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "newer\n");
    let back = tokio::time::timeout(Duration::from_secs(10), async {
        while history.load(second.id).unwrap().state == State::Unconfirmed {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(back.is_ok(), "never put back");
    assert_eq!(history.load(second.id).unwrap().state, State::RolledBack);
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "new\n");
}

/// Feed events from `channel` until one passes `last`.
async fn until_event(
    channel: &mut runo_link::client::Channel,
    last: impl Fn(&runo_link::feed::Event) -> bool,
) -> Vec<runo_link::feed::Event> {
    let mut seen = Vec::new();
    let wait = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(incoming) = channel.recv().await {
            if let Incoming::Data(data) = incoming {
                let event: runo_link::feed::Event = serde_json::from_slice(&data).unwrap();
                let done = last(&event);
                seen.push(event);
                if done {
                    return;
                }
            }
        }
    })
    .await;
    assert!(wait.is_ok(), "never came; saw {seen:?}");
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn a_feed_streams_events_and_catches_up_a_client_that_comes_back() {
    use runo_link::feed::{Body, Input, Role};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let client = Identity::generate().unwrap();
    Trust::new(&super::dir(state))
        .add(&client.public_key(), "laptop")
        .unwrap();
    // Stands in for `runo feed`: answers each message in two pieces.
    let agent = dir.path().join("agent.sh");
    std::fs::write(
        &agent,
        r#"#!/bin/sh
echo '{"kind":"turn","busy":false}'
n=0
while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      n=$((n+1))
      echo '{"kind":"message","id":"y'$n'","role":"you","text":"hi","done":true}'
      echo 'not json'
      echo '{"kind":"message","id":"r'$n'","role":"agent","text":"Hel","append":true,"done":false}'
      echo '{"kind":"message","id":"r'$n'","role":"agent","text":"lo","append":true,"done":true}' ;;
    *'"op":"cancel"'*) exit 3 ;;
  esac
done
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (daemon, address) = daemon(state).await;
    *daemon.sessions.feed_program.lock().unwrap() = Some(agent);
    let link = Link::connect(&[address], &client, Some(daemon_key(state)))
        .await
        .unwrap();
    assert!(link.welcome().feed);

    let (session, mut channel) = link.open(Kind::Feed, 0, 0).await.unwrap();
    assert_eq!(session.kind, Kind::Feed);
    let first = until_event(&mut channel, |e| matches!(e.body, Body::Turn { .. })).await;
    assert_eq!(first[0].seq, 1);
    link.feed(
        session.id,
        Input::Send {
            text: "hi".into(),
            attachments: vec![],
        },
    )
    .await
    .unwrap();
    let reply = until_event(&mut channel, |e| {
        matches!(
            &e.body,
            Body::Message {
                done: true,
                role: Role::Agent,
                ..
            }
        )
    })
    .await;
    assert_eq!(reply.len(), 3, "{reply:?}");
    let seen = reply.last().unwrap().seq;
    drop(channel);
    drop(link);

    // Away while the next answer comes: back, it gets only that, whole.
    let link = Link::connect(&[address], &client, Some(daemon_key(state)))
        .await
        .unwrap();
    let (_, mut channel) = link.attach_feed(session.id, seen).await.unwrap();
    link.feed(
        session.id,
        Input::Send {
            text: "again".into(),
            attachments: vec![],
        },
    )
    .await
    .unwrap();
    let missed = until_event(
        &mut channel,
        |e| matches!(&e.body, Body::Message { id, done: true, .. } if id == "r2"),
    )
    .await;
    assert!(missed.iter().all(|e| e.seq > seen), "{missed:?}");
    let sessions: Vec<proto::Session> = link.call(Request::Sessions).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].running);

    // A new client sees the whole conversation with each message whole.
    let (_, mut all) = link.attach_feed(session.id, 0).await.unwrap();
    let whole = until_event(
        &mut all,
        |e| matches!(&e.body, Body::Message { id, .. } if id == "r2"),
    )
    .await;
    assert!(whole.iter().any(|e| matches!(&e.body, Body::Message { id, text, append: false, .. } if id == "r1" && text == "Hello")), "{whole:?}");

    link.feed(session.id, Input::Cancel { card: None })
        .await
        .unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(incoming) = channel.recv().await {
            if let Incoming::Exit(code) = incoming {
                return code;
            }
        }
        None
    })
    .await;
    assert_eq!(ended.unwrap(), Some(3));
    let late = link
        .feed(
            session.id,
            Input::Send {
                text: "late".into(),
                attachments: vec![],
            },
        )
        .await;
    assert!(late.unwrap_err().to_string().contains("ended"));
}

#[tokio::test(flavor = "multi_thread")]
async fn from_outside_the_lan_only_trusted_clients_that_log_in_get_in() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let client = Identity::generate().unwrap();
    let trust = Trust::new(&super::dir(state));
    trust.add(&client.public_key(), "phone").unwrap();
    trust.set_password("device password").unwrap();
    let (daemon, address) = daemon(state).await;
    daemon
        .all_public
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let key = daemon_key(state);

    // A stranger cannot even pair from outside.
    let stranger = Identity::generate().unwrap();
    let Err(refused) = Link::connect(&[address], &stranger, None).await else {
        panic!("a stranger got in from outside");
    };
    assert!(format!("{refused:#}").contains("only on the local network"));

    let link = Link::connect(&[address], &client, Some(key)).await.unwrap();
    assert!(link.welcome().trusted && link.welcome().login);
    assert!(link.needs_login());
    let before = link
        .call::<proto::Text>(Request::Status { lang: None })
        .await;
    assert!(before.unwrap_err().to_string().contains("log in first"));
    assert!(link.open(Kind::Shell, 80, 24).await.is_err());
    let wrong = link.login("guess").await.unwrap_err();
    assert!(wrong.to_string().contains("wrong device password"));
    link.login("device password").await.unwrap();
    assert!(!link.needs_login());
    let status: proto::Text = link.call(Request::Status { lang: None }).await.unwrap();
    assert!(!status.text.is_empty());

    // Too many wrong passwords lock the client out for a while.
    for _ in 0..trust::MAX_FAILURES {
        daemon.logins.failed(&client.id(), now());
    }
    let again = Link::connect(&[address], &client, Some(key)).await.unwrap();
    let locked = again.login("device password").await.unwrap_err();
    assert!(locked.to_string().contains("too many"), "{locked}");

    // On the LAN nothing is asked.
    daemon
        .all_public
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let home = Link::connect(&[address], &client, Some(key)).await.unwrap();
    assert!(!home.welcome().login);
    let _: proto::Text = home.call(Request::Status { lang: None }).await.unwrap();
}
