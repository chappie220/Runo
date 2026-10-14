//! A connection's outgoing frames, with a count of the bytes still queued
//! so a file can wait for the network instead of filling memory.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use runo_link::frame::{CONTROL, Frame};
use runo_link::proto::{Down, Event};
use tokio::sync::{Notify, mpsc};

#[derive(Clone)]
pub struct Outbox {
    sender: mpsc::UnboundedSender<Frame>,
    queued: Arc<AtomicUsize>,
    drained: Arc<Notify>,
}

/// The receiving end, for the writer task.
pub struct Outgoing {
    receiver: mpsc::UnboundedReceiver<Frame>,
    queued: Arc<AtomicUsize>,
    drained: Arc<Notify>,
}

pub fn outbox() -> (Outbox, Outgoing) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let queued = Arc::new(AtomicUsize::new(0));
    let drained = Arc::new(Notify::new());
    (
        Outbox {
            sender,
            queued: queued.clone(),
            drained: drained.clone(),
        },
        Outgoing {
            receiver,
            queued,
            drained,
        },
    )
}

impl Outbox {
    pub fn send(&self, frame: Frame) {
        let size = frame.data.len() + 8;
        if self.sender.send(frame).is_ok() {
            self.queued.fetch_add(size, Ordering::Relaxed);
        }
    }

    pub fn down(&self, down: &Down) {
        self.send(Frame {
            channel: CONTROL,
            data: serde_json::to_vec(down).unwrap(),
        });
    }

    pub fn event(&self, event: Event) {
        self.down(&Down::Event(event));
    }

    /// Waits until fewer than `limit` bytes are queued, or the connection
    /// is gone (false).
    pub async fn room(&self, limit: usize) -> bool {
        loop {
            let notified = self.drained.notified();
            if self.sender.is_closed() {
                return false;
            }
            if self.queued.load(Ordering::Relaxed) < limit {
                return true;
            }
            notified.await;
        }
    }
}

impl Outgoing {
    pub async fn recv(&mut self) -> Option<Frame> {
        let frame = self.receiver.recv().await?;
        self.queued
            .fetch_sub(frame.data.len() + 8, Ordering::Relaxed);
        self.drained.notify_waiters();
        Some(frame)
    }
}

impl Drop for Outgoing {
    fn drop(&mut self) {
        self.receiver.close();
        self.drained.notify_waiters();
    }
}
