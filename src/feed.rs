//! `runo feed`: the Agent without a screen, for `runod`'s feed sessions
//! (Runo 1.0, 4.11). It reads one `feed::Input` per line on its standard
//! input and writes one `feed::Body` per line on its standard output:
//! messages as they stream, a task card per turn with a step per tool, a
//! confirmation card where a tool asks first, and when a turn begins and
//! ends. `runod` numbers the lines, keeps them for clients that come back
//! and sends them on; each front end draws them its own way.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use runo_link::feed::{Action, Body, Card, CardBody, CardState, Input, Risk, Role, Step, Style};
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};

use crate::access::{self, Actor};
use crate::agent::{Agent, AgentEvent, Cancel, Model, Tools};
use crate::i18n::{Lang, Tr, tr};
use crate::llm::Role as ChatRole;
use crate::tools::{Approver, with_approver};

/// Earlier messages shown when the feed starts.
const HISTORY: usize = 200;
/// How much of a tool's arguments or output a step keeps for showing.
const DETAIL: usize = 4096;
/// How long a confirmation card waits before it counts as no.
const CONFIRM_SECS: u32 = 300;

const DO_IT: Tr = tr("Do it", "执行");
const DONT: Tr = tr("Cancel", "取消");
const STOPPED: Tr = tr("Stopped.", "已停止。");
const WORKING: Tr = tr("Working", "正在处理");

/// Where the lines go: the standard output this process started with.
/// Anything else that prints (a tool, a library) lands on stderr instead,
/// so it cannot break the stream.
#[derive(Clone)]
pub struct Out(mpsc::UnboundedSender<Body>);

impl Out {
    fn send(&self, body: Body) {
        let _ = self.0.send(body);
    }

    fn card(&self, card: &Card) {
        self.send(Body::Card { card: card.clone() });
    }

    fn note(&self, id: String, text: String) {
        self.send(Body::Message {
            id,
            role: Role::Note,
            text,
            append: false,
            done: true,
        });
    }
}

/// Takes over standard output: the returned writer gets the lines, and
/// file descriptor 1 now goes to stderr.
fn take_stdout() -> std::fs::File {
    use std::os::fd::FromRawFd;
    // SAFETY: dup and dup2 on the standard descriptors, which are open.
    unsafe {
        let fd = libc::dup(libc::STDOUT_FILENO);
        libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO);
        std::fs::File::from_raw_fd(fd)
    }
}

/// Starts writing lines; the task ends when every `Out` is gone.
fn writer() -> (Out, tokio::task::JoinHandle<()>) {
    let (sender, mut receiver) = mpsc::unbounded_channel::<Body>();
    let mut file = take_stdout();
    let task = tokio::task::spawn_blocking(move || {
        while let Some(body) = receiver.blocking_recv() {
            let mut line = serde_json::to_vec(&body).unwrap_or_default();
            line.push(b'\n');
            if file.write_all(&line).and_then(|()| file.flush()).is_err() {
                break;
            }
        }
    });
    (Out(sender), task)
}

/// Says why the Agent could not start, then ends.
pub async fn fail(error: &str) {
    let (out, task) = writer();
    out.note("start".into(), error.to_owned());
    drop(out);
    let _ = task.await;
}

/// Answers to cards, by card ID.
type Waiting = Arc<Mutex<HashMap<String, oneshot::Sender<(String, Option<String>)>>>>;

/// Asks before a tool does something risky, as a confirmation card.
struct FeedApprover {
    out: Out,
    waiting: Waiting,
    next: Arc<std::sync::atomic::AtomicU64>,
    lang: Lang,
}

#[async_trait]
impl Approver for FeedApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("confirm-{n}");
        let (reply, answer) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id.clone(), reply);
        let mut card = Card {
            id: id.clone(),
            rev: 1,
            title: tool.to_owned(),
            state: CardState::Waiting,
            body: CardBody::Confirm {
                summary: summary.to_owned(),
                effects: Vec::new(),
                risk: Risk::High,
                timeout_s: Some(CONFIRM_SECS),
            },
            actions: vec![
                Action {
                    id: "yes".into(),
                    label: DO_IT.get(self.lang).into(),
                    style: Style::Danger,
                },
                Action {
                    id: "no".into(),
                    label: DONT.get(self.lang).into(),
                    style: Style::Normal,
                },
            ],
        };
        self.out.card(&card);
        let answer = tokio::time::timeout(Duration::from_secs(CONFIRM_SECS.into()), answer).await;
        self.waiting.lock().unwrap().remove(&id);
        let yes = matches!(answer, Ok(Ok((action, _))) if action == "yes");
        card.rev += 1;
        card.state = if yes {
            CardState::Done
        } else {
            CardState::Cancelled
        };
        card.actions.clear();
        self.out.card(&card);
        yes
    }
}

/// How one turn shows: its reply, and a task card once it uses a tool.
struct TurnView {
    out: Out,
    turn: u64,
    lang: Lang,
    /// The reply being written and how many replies the turn gave.
    reply: u32,
    replied: bool,
    task: Option<Card>,
}

impl TurnView {
    fn reply_id(&self) -> String {
        format!("r{}-{}", self.turn, self.reply)
    }

    fn on(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(delta) => {
                self.replied = true;
                self.out.send(Body::Message {
                    id: self.reply_id(),
                    role: Role::Agent,
                    text: delta,
                    append: true,
                    done: false,
                });
            }
            AgentEvent::ToolStart { name, arguments } => {
                let lang = self.lang;
                let turn = self.turn;
                let card = self.task.get_or_insert_with(|| Card {
                    id: format!("task-{turn}"),
                    rev: 0,
                    title: WORKING.get(lang).into(),
                    state: CardState::Running,
                    body: CardBody::Task {
                        steps: Vec::new(),
                        progress: None,
                        change: None,
                    },
                    actions: Vec::new(),
                });
                if let CardBody::Task { steps, .. } = &mut card.body {
                    steps.push(Step {
                        text: name,
                        state: CardState::Running,
                        detail: Some(cut(&arguments)),
                    });
                }
                card.rev += 1;
                self.out.card(card);
            }
            AgentEvent::ToolEnd { name, output } => {
                let Some(card) = self.task.as_mut() else {
                    return;
                };
                if let CardBody::Task { steps, .. } = &mut card.body
                    && let Some(step) = steps
                        .iter_mut()
                        .rev()
                        .find(|s| s.text == name && s.state == CardState::Running)
                {
                    step.state = CardState::Done;
                    step.detail = Some(cut(&output));
                }
                card.rev += 1;
                self.out.card(card);
            }
            AgentEvent::FollowUp(texts) => {
                self.close_reply();
                for (i, text) in texts.into_iter().enumerate() {
                    self.out.send(Body::Message {
                        id: format!("f{}-{}-{i}", self.turn, self.reply),
                        role: Role::You,
                        text,
                        append: false,
                        done: true,
                    });
                }
                self.reply += 1;
                self.replied = false;
            }
        }
    }

    fn close_reply(&mut self) {
        if self.replied {
            self.out.send(Body::Message {
                id: self.reply_id(),
                role: Role::Agent,
                text: String::new(),
                append: true,
                done: true,
            });
        }
    }

    /// Ends the turn's reply and task once it returned `result`.
    fn finish(&mut self, result: &Result<String>, stopped: bool) {
        match result {
            Ok(text) if !self.replied && !text.trim().is_empty() => {
                self.out.send(Body::Message {
                    id: self.reply_id(),
                    role: Role::Agent,
                    text: crate::agent::for_people(text),
                    append: false,
                    done: true,
                });
            }
            Ok(_) => self.close_reply(),
            Err(err) => {
                self.close_reply();
                self.out.note(format!("e{}", self.turn), format!("{err:#}"));
            }
        }
        if stopped {
            self.out
                .note(format!("s{}", self.turn), STOPPED.get(self.lang).into());
        }
        if let Some(card) = self.task.as_mut() {
            let state = match (stopped, result.is_ok()) {
                (true, _) => CardState::Cancelled,
                (false, true) => CardState::Done,
                (false, false) => CardState::Failed,
            };
            if let CardBody::Task { steps, .. } = &mut card.body {
                for step in steps.iter_mut().filter(|s| s.state == CardState::Running) {
                    step.state = state;
                }
            }
            card.state = state;
            card.rev += 1;
            self.out.card(card);
        }
    }
}

/// `text` cut to what a step keeps.
fn cut(text: &str) -> String {
    if text.len() <= DETAIL {
        return text.to_owned();
    }
    let mut end = DETAIL;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The session's earlier messages.
fn history<M: Model, T: Tools>(agent: &Agent<M, T>, session: &str, out: &Out) {
    let Ok(id) = agent.store.session_id(session) else {
        return;
    };
    let Ok(messages) = agent.store.history(id, HISTORY) else {
        return;
    };
    for (i, m) in messages.into_iter().enumerate() {
        let role = match m.role {
            ChatRole::User => Role::You,
            ChatRole::Assistant => Role::Agent,
            ChatRole::Tool | ChatRole::System => continue,
        };
        let mut text = crate::agent::for_people(m.content.as_deref().unwrap_or(""));
        if !m.attachments.is_empty() {
            let names: Vec<&str> = m.attachments.iter().map(|a| a.name.as_str()).collect();
            text = format!("{text}\n📎 {}", names.join(", ")).trim().to_owned();
        }
        if text.is_empty() {
            continue;
        }
        out.send(Body::Message {
            id: format!("h{i}"),
            role,
            text,
            append: false,
            done: true,
        });
    }
}

/// A message to send, as it came in.
struct Outgoing {
    text: String,
    attachments: Vec<String>,
}

type Turn<'a> = Pin<Box<dyn Future<Output = Result<String>> + 'a>>;

async fn wait(turn: &mut Option<Turn<'_>>) -> Result<String> {
    match turn {
        Some(turn) => turn.await,
        None => std::future::pending().await,
    }
}

/// Runs the feed until standard input closes.
pub async fn run<M: Model, T: Tools>(agent: &Agent<M, T>, session: String) -> Result<()> {
    let lang = crate::i18n::current();
    let (out, writer) = writer();
    let waiting: Waiting = Arc::default();
    let next_card = Arc::new(std::sync::atomic::AtomicU64::new(1));
    history(agent, &session, &out);
    out.send(Body::Turn { busy: false });

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut queue: VecDeque<Outgoing> = VecDeque::new();
    let mut turn: Option<Turn> = None;
    let mut view: Option<Arc<Mutex<TurnView>>> = None;
    let mut cancel = Cancel::default();
    let mut stopped = false;
    let mut turns = 0u64;
    let mut sent = 0u64;

    loop {
        if turn.is_none()
            && let Some(message) = queue.pop_front()
        {
            turns += 1;
            cancel = Cancel::default();
            stopped = false;
            let shown = Arc::new(Mutex::new(TurnView {
                out: out.clone(),
                turn: turns,
                lang,
                reply: 0,
                replied: false,
                task: None,
            }));
            view = Some(shown.clone());
            out.send(Body::Turn { busy: true });
            let approver: Arc<dyn Approver> = Arc::new(FeedApprover {
                out: out.clone(),
                waiting: waiting.clone(),
                next: next_card.clone(),
                lang,
            });
            turn = Some(start_turn(
                agent,
                session.clone(),
                message,
                shown,
                cancel.clone(),
                approver,
            ));
        }
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let input = match serde_json::from_str::<Input>(&line) {
                    Ok(input) => input,
                    Err(err) => {
                        eprintln!("feed: a line that is not an input: {err}");
                        continue;
                    }
                };
                match input {
                    Input::Send { text, attachments } => {
                        sent += 1;
                        let mut shown = text.clone();
                        if !attachments.is_empty() {
                            let names: Vec<&str> = attachments
                                .iter()
                                .map(|p| p.rsplit('/').next().unwrap_or(p))
                                .collect();
                            shown = format!("{shown}\n📎 {}", names.join(", ")).trim().to_owned();
                        }
                        out.send(Body::Message {
                            id: format!("y{sent}"),
                            role: Role::You,
                            text: shown,
                            append: false,
                            done: true,
                        });
                        queue.push_back(Outgoing { text, attachments });
                    }
                    Input::Answer { card, action, value } => {
                        if let Some(reply) = waiting.lock().unwrap().remove(&card) {
                            let _ = reply.send((action, value));
                        }
                    }
                    Input::Cancel { .. } => {
                        if turn.is_some() {
                            stopped = true;
                            cancel.cancel();
                        }
                    }
                }
            }
            result = wait(&mut turn) => {
                turn = None;
                if let Some(view) = view.take() {
                    view.lock().unwrap().finish(&result, stopped);
                }
                out.send(Body::Turn { busy: false });
            }
        }
    }
    // Standard input closed: runod is gone or ended the session.
    cancel.cancel();
    drop(turn);
    drop(out);
    drop(view);
    let _ = writer.await;
    Ok(())
}

fn start_turn<'a, M: Model, T: Tools>(
    agent: &'a Agent<M, T>,
    session: String,
    message: Outgoing,
    view: Arc<Mutex<TurnView>>,
    cancel: Cancel,
    approver: Arc<dyn Approver>,
) -> Turn<'a> {
    Box::pin(async move {
        let actor = Actor::owner(access::CLI);
        let mut uploads = Vec::new();
        let mut problems = Vec::new();
        for path in &message.attachments {
            match crate::read_upload(&PathBuf::from(path)) {
                Ok(upload) => uploads.push(upload),
                Err(err) => problems.push(format!("{err:#}")),
            }
        }
        let text = if problems.is_empty() {
            message.text
        } else {
            format!("{}\n\n{}", message.text, problems.join("\n"))
        };
        let (files, text) = agent.receive(&actor, &session, &text, &uploads).await;
        let mut on_event = move |event| view.lock().unwrap().on(event);
        let turn = agent.run_turn_until(actor, &session, &text, &files, &cancel, &mut on_event);
        with_approver(approver, turn).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_view() -> (TurnView, mpsc::UnboundedReceiver<Body>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            TurnView {
                out: Out(sender),
                turn: 1,
                lang: Lang::En,
                reply: 0,
                replied: false,
                task: None,
            },
            receiver,
        )
    }

    fn drain(receiver: &mut mpsc::UnboundedReceiver<Body>) -> Vec<Body> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    #[test]
    fn a_turn_shows_as_a_reply_and_a_task_card() {
        let (mut view, mut lines) = new_view();
        view.on(AgentEvent::ToolStart {
            name: "shell".into(),
            arguments: r#"{"command":"apk add htop"}"#.into(),
        });
        view.on(AgentEvent::ToolEnd {
            name: "shell".into(),
            output: "OK".into(),
        });
        view.on(AgentEvent::Text("Installed ".into()));
        view.on(AgentEvent::Text("htop.".into()));
        view.finish(&Ok("Installed htop.".into()), false);

        let bodies = drain(&mut lines);
        let Body::Card { card } = &bodies[1] else {
            panic!("{bodies:?}")
        };
        assert_eq!(card.rev, 2);
        let CardBody::Task { steps, .. } = &card.body else {
            panic!()
        };
        assert_eq!(steps[0].state, CardState::Done);
        assert_eq!(steps[0].detail.as_deref(), Some("OK"));
        assert!(
            matches!(&bodies[2], Body::Message { id, text, append: true, done: false, .. } if id == "r1-0" && text == "Installed ")
        );
        assert!(matches!(&bodies[4], Body::Message { text, done: true, .. } if text.is_empty()));
        let Body::Card { card } = &bodies[5] else {
            panic!("{bodies:?}")
        };
        assert_eq!(card.state, CardState::Done);
        assert_eq!(bodies.len(), 6);
    }

    #[test]
    fn a_reply_that_did_not_stream_comes_whole_and_a_stop_is_said() {
        let (mut view, mut lines) = new_view();
        view.finish(&Ok("Over the daily limit.".into()), false);
        assert!(
            matches!(&drain(&mut lines)[..], [Body::Message { role: Role::Agent, text, done: true, .. }] if text == "Over the daily limit.")
        );

        let (mut view, mut lines) = new_view();
        view.on(AgentEvent::ToolStart {
            name: "shell".into(),
            arguments: "{}".into(),
        });
        view.finish(&Ok(String::new()), true);
        let bodies = drain(&mut lines);
        assert!(matches!(
            &bodies[1],
            Body::Message {
                role: Role::Note,
                ..
            }
        ));
        let Body::Card { card } = &bodies[2] else {
            panic!("{bodies:?}")
        };
        assert_eq!(card.state, CardState::Cancelled);
        let CardBody::Task { steps, .. } = &card.body else {
            panic!()
        };
        assert_eq!(steps[0].state, CardState::Cancelled);
    }

    #[test]
    fn long_details_are_cut_on_a_character() {
        let text = "汉".repeat(DETAIL);
        let cut = cut(&text);
        assert!(cut.len() <= DETAIL + "…".len());
        assert!(cut.ends_with('…'));
    }
}
