//! The Agent as a structured event stream (Runo 1.0, 4.11): messages,
//! cards and the device's status, drawn by each front end in its own way
//! instead of as a terminal.
//!
//! The Agent process (`runo feed`) writes one [`Body`] per line on its
//! standard output and reads one [`Input`] per line on its standard input.
//! `runod` numbers what it writes ([`Event`]), keeps the latest state of
//! each message and card in a [`Log`], and sends the events to the clients
//! showing the session, one JSON event per frame on the session's channel.
//! A client that comes back gives the last number it saw and gets what
//! changed since, so nothing is lost or shown twice.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

/// What the Agent process says, before `runod` numbers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// A message, or more of one: with `append` the text goes after what
    /// the message with this `id` already has, otherwise it replaces it.
    Message {
        id: String,
        role: Role,
        text: String,
        #[serde(default)]
        append: bool,
        done: bool,
    },
    /// A new card, or a card's new state: the one with the higher `rev`
    /// wins.
    Card { card: Card },
    /// The device's state, every 2 seconds while someone watches.
    Status { status: Status },
    /// A turn began (`busy`) or ended.
    Turn { busy: bool },
}

/// A [`Body`] numbered by `runod`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The person.
    You,
    Agent,
    /// Said by Runo itself: an error, a stopped turn, a hint.
    Note,
}

/// What a front end sends to the Agent process, through `runod`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Input {
    /// A message; `attachments` are paths on the device, sent before with
    /// `file_put`.
    Send {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// An answer to a card: one of its actions, and for a secret or a
    /// choice the value. A secret's value is never written to a log.
    Answer {
        card: String,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// Stops the turn, or the task on `card`.
    Cancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    /// Made by the device, unique in the session.
    pub id: String,
    /// One more on every change.
    pub rev: u32,
    pub title: String,
    pub state: CardState,
    pub body: CardBody,
    /// The buttons that can be pressed now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    /// Waits for the person to answer.
    Waiting,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl CardState {
    /// Whether nothing more will happen to it.
    pub fn ended(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// What `Input::Answer` carries.
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub style: Style,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    Primary,
    #[default]
    Normal,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CardBody {
    /// Something the Agent does: installing, changing a setting, a
    /// download. With `change` (a record of `runo changes`) it can be put
    /// back.
    Task {
        steps: Vec<Step>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<(u64, u64)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<u64>,
    },
    /// A risky step, done only once the person agrees; no answer within
    /// `timeout_s` counts as no.
    Confirm {
        summary: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        effects: Vec<String>,
        risk: Risk,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_s: Option<u32>,
    },
    /// A key, token or password, asked for outside the conversation.
    Secret {
        name: String,
        purpose: String,
        #[serde(default)]
        min_len: u32,
        saved: bool,
    },
    Choice {
        question: String,
        options: Vec<Choice>,
        #[serde(default)]
        multi: bool,
    },
    File {
        path: String,
        size: u64,
        direction: Direction,
        #[serde(default)]
        progress: u64,
    },
    /// The first start's checks: network, time, password, model, proxy,
    /// public access, updates.
    Checklist { items: Vec<Check> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub text: String,
    pub state: CardState,
    /// What a person may open: the tool's arguments or output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub text: String,
    pub state: CardState,
    /// "Handle it": asks the Agent to see to this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Action>,
}

/// The device's state, read from `/proc` and `/sys`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// Since the last snapshot.
    pub cpu_percent: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f32>,
    /// Used and total bytes.
    pub mem: (u64, u64),
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub swap: Vec<Swap>,
    /// Bytes per second written to the data partition.
    pub disk_write_bps: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// Unix time the pairing window closes, while open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_open_until: Option<i64>,
    /// A system update in progress, or how the last one went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    pub device: String,
    pub size: u64,
    pub used: u64,
    /// What `used` takes in memory once compressed (zram only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proxy {
    pub address: String,
    pub reachable: bool,
}

/// How many messages and cards a [`Log`] keeps.
pub const KEEP: usize = 1000;

/// What a session showed: the latest state of every message and card (the
/// newest [`KEEP`]), whether a turn runs, and the latest status, each with
/// the number of the event that last changed it.
#[derive(Debug, Default)]
pub struct Log {
    seq: u64,
    /// Messages and cards, oldest first, keyed by `Key`.
    items: VecDeque<(Key, u64)>,
    messages: HashMap<String, (Role, String, bool)>,
    cards: HashMap<String, Card>,
    turn: Option<(bool, u64)>,
    status: Option<(Status, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    Message(String),
    Card(String),
}

impl Log {
    /// The number of the last event.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Takes in what the Agent said; the numbered event to send, or `None`
    /// for a card older than the one kept.
    pub fn push(&mut self, body: Body) -> Option<Event> {
        let seq = self.seq + 1;
        match &body {
            Body::Message {
                id,
                role,
                text,
                append,
                done,
            } => {
                let key = Key::Message(id.clone());
                match self.messages.get_mut(id) {
                    Some(message) => {
                        if *append {
                            message.1.push_str(text);
                        } else {
                            message.1.clone_from(text);
                        }
                        message.2 = *done;
                        self.touch(&key, seq);
                    }
                    None => {
                        self.messages
                            .insert(id.clone(), (*role, text.clone(), *done));
                        self.add(key, seq);
                    }
                }
            }
            Body::Card { card } => {
                let key = Key::Card(card.id.clone());
                match self.cards.get_mut(&card.id) {
                    Some(kept) if kept.rev >= card.rev => return None,
                    Some(kept) => {
                        *kept = card.clone();
                        self.touch(&key, seq);
                    }
                    None => {
                        self.cards.insert(card.id.clone(), card.clone());
                        self.add(key, seq);
                    }
                }
            }
            Body::Status { status } => self.status = Some((status.clone(), seq)),
            Body::Turn { busy } => self.turn = Some((*busy, seq)),
        }
        self.seq = seq;
        Some(Event { seq, body })
    }

    fn add(&mut self, key: Key, seq: u64) {
        self.items.push_back((key, seq));
        while self.items.len() > KEEP {
            match self.items.pop_front() {
                Some((Key::Message(id), _)) => {
                    self.messages.remove(&id);
                }
                Some((Key::Card(id), _)) => {
                    self.cards.remove(&id);
                }
                None => break,
            }
        }
    }

    fn touch(&mut self, key: &Key, seq: u64) {
        if let Some(item) = self.items.iter_mut().find(|(k, _)| k == key) {
            item.1 = seq;
        }
    }

    /// What changed after event `since` (0: everything), as whole messages
    /// and cards in the order they first came, then the turn and the
    /// status. Each event keeps the number of the last change it holds, so
    /// a front end that applies them is where one would be that saw every
    /// event up to [`Log::seq`].
    pub fn since(&self, since: u64) -> Vec<Event> {
        let mut events: Vec<Event> = self
            .items
            .iter()
            .filter(|(_, seq)| *seq > since)
            .filter_map(|(key, seq)| {
                let body = match key {
                    Key::Message(id) => {
                        let (role, text, done) = self.messages.get(id)?;
                        Body::Message {
                            id: id.clone(),
                            role: *role,
                            text: text.clone(),
                            append: false,
                            done: *done,
                        }
                    }
                    Key::Card(id) => Body::Card {
                        card: self.cards.get(id)?.clone(),
                    },
                };
                Some(Event { seq: *seq, body })
            })
            .collect();
        if let Some((busy, seq)) = self.turn.filter(|(_, seq)| *seq > since) {
            events.push(Event {
                seq,
                body: Body::Turn { busy },
            });
        }
        if let Some((status, seq)) = self.status.as_ref().filter(|(_, seq)| *seq > since) {
            events.push(Event {
                seq: *seq,
                body: Body::Status {
                    status: status.clone(),
                },
            });
        }
        events
    }

    /// The cards still waiting for an answer.
    pub fn waiting(&self) -> impl Iterator<Item = &Card> {
        self.cards
            .values()
            .filter(|c| c.state == CardState::Waiting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str, append: bool, done: bool) -> Body {
        Body::Message {
            id: id.into(),
            role: Role::Agent,
            text: text.into(),
            append,
            done,
        }
    }

    fn task(rev: u32, state: CardState) -> Body {
        Body::Card {
            card: Card {
                id: "t1".into(),
                rev,
                title: "Install htop".into(),
                state,
                body: CardBody::Task {
                    steps: vec![],
                    progress: None,
                    change: None,
                },
                actions: vec![],
            },
        }
    }

    #[test]
    fn events_have_a_plain_json_shape() {
        let event = Event {
            seq: 7,
            body: message("m1", "Hi", true, false),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"seq": 7, "kind": "message", "id": "m1", "role": "agent", "text": "Hi", "append": true, "done": false})
        );
        let card = serde_json::to_value(Body::Card {
            card: Card {
                id: "c1".into(),
                rev: 1,
                title: "Restart?".into(),
                state: CardState::Waiting,
                body: CardBody::Confirm {
                    summary: "reboot".into(),
                    effects: vec![],
                    risk: Risk::High,
                    timeout_s: Some(60),
                },
                actions: vec![Action {
                    id: "yes".into(),
                    label: "Restart".into(),
                    style: Style::Danger,
                }],
            },
        })
        .unwrap();
        assert_eq!(card["card"]["body"]["type"], "confirm");
        assert_eq!(card["card"]["actions"][0]["style"], "danger");
        let input: Input = serde_json::from_str(r#"{"op":"send","text":"hello"}"#).unwrap();
        assert_eq!(
            input,
            Input::Send {
                text: "hello".into(),
                attachments: vec![]
            }
        );
    }

    #[test]
    fn a_client_that_comes_back_gets_what_changed_once() {
        let mut log = Log::default();
        log.push(Body::Turn { busy: true });
        log.push(message("m1", "Hel", true, false));
        log.push(task(1, CardState::Running));
        let seen = log.seq();
        log.push(message("m1", "lo", true, true));
        log.push(task(2, CardState::Done));
        log.push(message("m2", "next", false, true));
        log.push(Body::Turn { busy: false });

        let missed = log.since(seen);
        assert_eq!(missed.len(), 4);
        assert_eq!(missed[0].body, message("m1", "Hello", false, true));
        assert!(matches!(&missed[1].body, Body::Card { card } if card.rev == 2));
        assert_eq!(missed[2].body, message("m2", "next", false, true));
        assert_eq!(missed[3].body, Body::Turn { busy: false });
        assert!(log.since(log.seq()).is_empty());
        // From the start, in the order things first came.
        let all = log.since(0);
        assert_eq!(all.len(), 4);
        assert!(matches!(&all[0].body, Body::Message { id, .. } if id == "m1"));
    }

    #[test]
    fn an_older_card_does_not_win() {
        let mut log = Log::default();
        assert!(log.push(task(2, CardState::Done)).is_some());
        assert!(log.push(task(1, CardState::Running)).is_none());
        assert_eq!(log.seq(), 1);
        assert_eq!(log.waiting().count(), 0);
    }

    #[test]
    fn only_the_newest_items_are_kept() {
        let mut log = Log::default();
        for i in 0..KEEP + 5 {
            log.push(message(&format!("m{i}"), "x", false, true));
        }
        let all = log.since(0);
        assert_eq!(all.len(), KEEP);
        assert!(matches!(&all[0].body, Body::Message { id, .. } if id == "m5"));
    }
}
