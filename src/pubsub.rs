//! A small pub/sub registry. Subscribers register an unbounded channel; a
//! `PUBLISH` fans a message out to matching channel and pattern subscribers.

use crate::db::glob_match;
use crate::resp::Frame;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

struct Subscriber {
    id: u64,
    tx: UnboundedSender<Frame>,
}

#[derive(Default)]
struct Inner {
    channels: HashMap<Bytes, Vec<Subscriber>>,
    patterns: HashMap<Bytes, Vec<Subscriber>>,
}

#[derive(Default)]
pub struct PubSub {
    inner: Mutex<Inner>,
}

impl PubSub {
    pub fn subscribe(&self, channel: Bytes, id: u64, tx: &UnboundedSender<Frame>) {
        let mut inner = self.inner.lock().unwrap();
        let subs = inner.channels.entry(channel).or_default();
        if !subs.iter().any(|s| s.id == id) {
            subs.push(Subscriber { id, tx: tx.clone() });
        }
    }

    pub fn unsubscribe(&self, channel: &[u8], id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(subs) = inner.channels.get_mut(channel) {
            subs.retain(|s| s.id != id);
            if subs.is_empty() {
                inner.channels.remove(channel);
            }
        }
    }

    pub fn psubscribe(&self, pattern: Bytes, id: u64, tx: &UnboundedSender<Frame>) {
        let mut inner = self.inner.lock().unwrap();
        let subs = inner.patterns.entry(pattern).or_default();
        if !subs.iter().any(|s| s.id == id) {
            subs.push(Subscriber { id, tx: tx.clone() });
        }
    }

    pub fn punsubscribe(&self, pattern: &[u8], id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(subs) = inner.patterns.get_mut(pattern) {
            subs.retain(|s| s.id != id);
            if subs.is_empty() {
                inner.patterns.remove(pattern);
            }
        }
    }

    /// Remove a client from every channel and pattern (called on disconnect).
    pub fn remove_client(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.channels.retain(|_, subs| {
            subs.retain(|s| s.id != id);
            !subs.is_empty()
        });
        inner.patterns.retain(|_, subs| {
            subs.retain(|s| s.id != id);
            !subs.is_empty()
        });
    }

    /// Deliver `payload` to `channel`. Returns the number of clients that
    /// received it (channel subscribers plus matching pattern subscribers).
    pub fn publish(&self, channel: &[u8], payload: &Bytes) -> i64 {
        let inner = self.inner.lock().unwrap();
        let mut count = 0i64;
        if let Some(subs) = inner.channels.get(channel) {
            for s in subs {
                let msg = Frame::Push(vec![
                    Frame::bulk("message"),
                    Frame::bulk(Bytes::copy_from_slice(channel)),
                    Frame::bulk(payload.clone()),
                ]);
                if s.tx.send(msg).is_ok() {
                    count += 1;
                }
            }
        }
        for (pattern, subs) in inner.patterns.iter() {
            if glob_match(pattern, channel) {
                for s in subs {
                    let msg = Frame::Push(vec![
                        Frame::bulk("pmessage"),
                        Frame::bulk(pattern.clone()),
                        Frame::bulk(Bytes::copy_from_slice(channel)),
                        Frame::bulk(payload.clone()),
                    ]);
                    if s.tx.send(msg).is_ok() {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// Channels with at least one subscriber, optionally filtered by a glob.
    pub fn channels(&self, pattern: Option<&[u8]>) -> Vec<Bytes> {
        let inner = self.inner.lock().unwrap();
        inner
            .channels
            .keys()
            .filter(|c| pattern.map_or(true, |p| glob_match(p, c)))
            .cloned()
            .collect()
    }

    /// Subscriber count for a specific channel.
    pub fn numsub(&self, channel: &[u8]) -> i64 {
        let inner = self.inner.lock().unwrap();
        inner.channels.get(channel).map_or(0, |s| s.len() as i64)
    }

    /// Number of distinct patterns with at least one subscriber.
    pub fn numpat(&self) -> i64 {
        self.inner.lock().unwrap().patterns.len() as i64
    }
}
