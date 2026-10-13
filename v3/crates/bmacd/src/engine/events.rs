// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The live event stream behind `GET /api/v1/events` (`ARCHITECTURE.md`,
//! "Live updates"). Every message gets an ID `<epoch>-<sequence>`; a recent
//! window is kept so a reconnecting tab can replay what it missed. A tab
//! that fell too far behind, or whose ID is from before the engine started,
//! is told to fetch fresh snapshots instead. Publishing never waits for a
//! slow tab.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use bmac_core::{HostSlot, JobId};
use bmac_proto::stream::{StreamBody, StreamMessage};
use ring::rand::SystemRandom;
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::auth::sessions::new_token;

/// Messages kept for replay, by count and by encoded size.
pub const REPLAY_MESSAGES: usize = 4096;
pub const REPLAY_BYTES: usize = 8 * 1024 * 1024;
/// Messages queued per tab before it counts as fallen behind.
pub const TAB_QUEUE: usize = 1024;
/// Jobs one tab may watch.
pub const MAX_WATCHED: usize = 100;

struct Kept {
    seq: u64,
    bytes: usize,
    message: Arc<StreamMessage>,
}

struct Window {
    next_seq: u64,
    kept: VecDeque<Kept>,
    bytes: usize,
}

struct Tab {
    session: String,
    jobs: watch::Sender<HashSet<JobId>>,
}

pub struct EventHub {
    epoch: String,
    window: Mutex<Window>,
    sender: broadcast::Sender<(u64, Arc<StreamMessage>)>,
    tabs: Mutex<HashMap<String, Tab>>,
    outbox: broadcast::Sender<StreamBody>,
    random: SystemRandom,
    latest: Mutex<Latest>,
}

/// The newest observation from each host's collectors, and whether each
/// peer's stream is up, from every message published here.
#[derive(Default)]
struct Latest {
    observations: BTreeMap<(HostSlot, String), Value>,
    peers: BTreeMap<HostSlot, Reachability>,
}

/// A peer's host-to-host stream: up or down, and when it last sent anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reachability {
    pub reachable: bool,
    pub last_seen_ms: i64,
}

/// Messages that peers learn of through their stream from this host: what
/// changed here, never what was learned from another host.
fn for_peers(body: &StreamBody) -> bool {
    matches!(
        body,
        StreamBody::Job { .. } | StreamBody::Observation { .. } | StreamBody::Alert { .. }
    )
}

impl std::fmt::Debug for EventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub")
            .field("epoch", &self.epoch)
            .finish()
    }
}

/// Where a new subscription starts.
#[derive(Debug)]
pub enum Start {
    /// Replay these, then continue live.
    Replay(Vec<Arc<StreamMessage>>),
    /// Fetch fresh snapshots, then continue live.
    Resnapshot(String),
}

/// One tab's subscription. Drop it when the tab leaves.
pub struct Subscription {
    hub: Arc<EventHub>,
    pub stream: String,
    pub hello: StreamMessage,
    pub start: Start,
    receiver: broadcast::Receiver<(u64, Arc<StreamMessage>)>,
    jobs: watch::Receiver<HashSet<JobId>>,
    last_seq: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.tabs().remove(&self.stream);
    }
}

/// What a subscription yields next.
#[derive(Debug)]
pub enum Next {
    Message(Arc<StreamMessage>),
    /// The tab fell behind; tell it to fetch fresh snapshots.
    Resnapshot(StreamMessage),
    Closed,
}

impl Subscription {
    fn wanted(&self, message: &StreamMessage) -> bool {
        match &message.body {
            StreamBody::JobEvent { event } => self.jobs.borrow().contains(&event.job),
            _ => true,
        }
    }

    /// The messages to replay that this tab wants, from [`Start::Replay`].
    pub fn replay(&self) -> Vec<Arc<StreamMessage>> {
        match &self.start {
            Start::Replay(messages) => messages
                .iter()
                .filter(|m| self.wanted(m))
                .cloned()
                .collect(),
            Start::Resnapshot(_) => Vec::new(),
        }
    }

    pub async fn next(&mut self) -> Next {
        loop {
            match self.receiver.recv().await {
                Ok((seq, message)) => {
                    if seq <= self.last_seq {
                        continue;
                    }
                    self.last_seq = seq;
                    if self.wanted(&message) {
                        return Next::Message(message);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let current = {
                        let window = self.hub.window();
                        self.receiver = self.receiver.resubscribe();
                        window.next_seq - 1
                    };
                    self.last_seq = current;
                    return Next::Resnapshot(StreamMessage {
                        id: self.hub.id(current),
                        body: StreamBody::Resnapshot {
                            reason: "this tab fell behind the live updates".into(),
                        },
                    });
                }
                Err(broadcast::error::RecvError::Closed) => return Next::Closed,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WatchError {
    #[error("no such event stream; it may have closed")]
    UnknownStream,
    #[error("a tab may watch at most {MAX_WATCHED} jobs")]
    TooMany,
}

impl EventHub {
    pub fn new() -> Arc<EventHub> {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        EventHub::with_epoch(format!("{started:x}"))
    }

    pub fn with_epoch(epoch: impl Into<String>) -> Arc<EventHub> {
        Arc::new(EventHub {
            epoch: epoch.into(),
            window: Mutex::new(Window {
                next_seq: 1,
                kept: VecDeque::new(),
                bytes: 0,
            }),
            sender: broadcast::channel(TAB_QUEUE).0,
            tabs: Mutex::new(HashMap::new()),
            outbox: broadcast::channel(TAB_QUEUE).0,
            random: SystemRandom::new(),
            latest: Mutex::new(Latest::default()),
        })
    }

    fn latest(&self) -> std::sync::MutexGuard<'_, Latest> {
        self.latest.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn remember(&self, body: &StreamBody) {
        match body {
            StreamBody::Observation {
                host,
                collector,
                data,
            } => {
                self.latest()
                    .observations
                    .insert((*host, collector.clone()), data.clone());
            }
            StreamBody::Host {
                host,
                reachable,
                last_seen_ms,
            } => {
                self.latest().peers.insert(
                    *host,
                    Reachability {
                        reachable: *reachable,
                        last_seen_ms: *last_seen_ms,
                    },
                );
            }
            _ => {}
        }
    }

    /// The newest observation of `collector` on `host`.
    pub fn observation(&self, host: HostSlot, collector: &str) -> Option<Value> {
        self.latest()
            .observations
            .get(&(host, collector.to_string()))
            .cloned()
    }

    /// Every newest observation `host` made, by collector.
    pub fn observations_of(&self, host: HostSlot) -> Vec<(String, Value)> {
        self.latest()
            .observations
            .iter()
            .filter(|((h, _), _)| *h == host)
            .map(|((_, collector), data)| (collector.clone(), data.clone()))
            .collect()
    }

    /// The peers whose streams this host has heard about.
    pub fn reachability(&self) -> BTreeMap<HostSlot, Reachability> {
        self.latest().peers.clone()
    }

    fn window(&self) -> std::sync::MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn tabs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Tab>> {
        self.tabs.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn id(&self, seq: u64) -> String {
        format!("{}-{seq}", self.epoch)
    }

    /// Sends a message about this host to every tab, keeps it for replay,
    /// and queues job, observation, and alert changes for peers.
    pub fn publish(&self, body: StreamBody) -> String {
        if for_peers(&body) {
            let _ = self.outbox.send(body.clone());
        }
        self.publish_copy(body)
    }

    /// The changes [`EventHub::publish`] queued for peers.
    pub fn outbox(&self) -> broadcast::Receiver<StreamBody> {
        self.outbox.subscribe()
    }

    /// Sends a message learned from another host to every tab, without
    /// passing it on to peers.
    pub fn publish_copy(&self, body: StreamBody) -> String {
        self.remember(&body);
        let mut window = self.window();
        let seq = window.next_seq;
        window.next_seq += 1;
        let message = Arc::new(StreamMessage {
            id: self.id(seq),
            body,
        });
        let bytes = serde_json::to_vec(&*message).map_or(0, |v| v.len());
        window.kept.push_back(Kept {
            seq,
            bytes,
            message: message.clone(),
        });
        window.bytes += bytes;
        while window.kept.len() > REPLAY_MESSAGES
            || (window.bytes > REPLAY_BYTES && window.kept.len() > 1)
        {
            if let Some(old) = window.kept.pop_front() {
                window.bytes -= old.bytes;
            }
        }
        let id = message.id.clone();
        // Sent under the lock, so a new subscription sees each message
        // exactly once: in its replay or live.
        let _ = self.sender.send((seq, message));
        id
    }

    /// Opens a tab's stream, replaying from `last_event_id` when it can.
    pub fn subscribe(self: &Arc<Self>, session: &str, last_event_id: Option<&str>) -> Subscription {
        let stream = new_token(&self.random)
            .unwrap_or_else(|| format!("{}-{}", self.epoch, self.window().next_seq));
        let (jobs, jobs_rx) = watch::channel(HashSet::new());
        self.tabs().insert(
            stream.clone(),
            Tab {
                session: session.to_string(),
                jobs,
            },
        );
        let window = self.window();
        let receiver = self.sender.subscribe();
        let current = window.next_seq - 1;
        let start = match last_event_id {
            None => Start::Replay(Vec::new()),
            Some(id) => self.start_from(&window, id, current),
        };
        drop(window);
        Subscription {
            hub: self.clone(),
            hello: StreamMessage {
                id: self.id(current),
                body: StreamBody::Hello {
                    stream: stream.clone(),
                },
            },
            stream,
            start,
            receiver,
            jobs: jobs_rx,
            last_seq: current,
        }
    }

    fn start_from(&self, window: &Window, id: &str, current: u64) -> Start {
        let Some((epoch, seq)) = id.rsplit_once('-') else {
            return Start::Resnapshot("the last event ID is not one of ours".into());
        };
        if epoch != self.epoch {
            return Start::Resnapshot("BMAC restarted on this host since the last update".into());
        }
        let Ok(seq) = seq.parse::<u64>() else {
            return Start::Resnapshot("the last event ID is not one of ours".into());
        };
        if seq > current {
            return Start::Resnapshot("the last event ID is not one of ours".into());
        }
        let oldest = window.kept.front().map_or(current + 1, |k| k.seq);
        if seq + 1 < oldest {
            return Start::Resnapshot("too much changed since the last update to replay it".into());
        }
        Start::Replay(
            window
                .kept
                .iter()
                .filter(|k| k.seq > seq)
                .map(|k| k.message.clone())
                .collect(),
        )
    }

    /// Sets the jobs whose events `stream` carries. Only the session that
    /// opened the stream may change it.
    pub fn watch(&self, session: &str, stream: &str, jobs: Vec<JobId>) -> Result<(), WatchError> {
        if jobs.len() > MAX_WATCHED {
            return Err(WatchError::TooMany);
        }
        let tabs = self.tabs();
        let tab = tabs
            .get(stream)
            .filter(|t| t.session == session)
            .ok_or(WatchError::UnknownStream)?;
        tab.jobs.send_replace(jobs.into_iter().collect());
        Ok(())
    }

    pub fn tab_count(&self) -> usize {
        self.tabs().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quorum(quorate: bool) -> StreamBody {
        StreamBody::Quorum { quorate }
    }

    async fn ids(subscription: &mut Subscription, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..n {
            match subscription.next().await {
                Next::Message(m) => out.push(m.id.clone()),
                other => panic!("{other:?}"),
            }
        }
        out
    }

    #[tokio::test]
    async fn reconnecting_replays_what_was_missed() {
        let hub = EventHub::with_epoch("e1");
        let mut first = hub.subscribe("s", None);
        assert_eq!(first.hello.id, "e1-0");
        hub.publish(quorum(true));
        hub.publish(quorum(false));
        assert_eq!(ids(&mut first, 2).await, ["e1-1", "e1-2"]);
        drop(first);
        assert_eq!(hub.tab_count(), 0);
        hub.publish(quorum(true));
        hub.publish(quorum(false));

        let mut again = hub.subscribe("s", Some("e1-2"));
        let replayed: Vec<_> = again.replay().iter().map(|m| m.id.clone()).collect();
        assert_eq!(replayed, ["e1-3", "e1-4"]);
        hub.publish(quorum(true));
        assert_eq!(
            ids(&mut again, 1).await,
            ["e1-5"],
            "nothing twice, nothing lost"
        );

        for id in ["e0-2", "e1-99", "garbage", "e1-x"] {
            assert!(
                matches!(hub.subscribe("s", Some(id)).start, Start::Resnapshot(_)),
                "{id}"
            );
        }
        assert!(hub.subscribe("s", Some("e1-0")).replay().len() == 5);
    }

    #[tokio::test]
    async fn an_old_gap_asks_for_a_snapshot() {
        let hub = EventHub::with_epoch("e");
        for _ in 0..REPLAY_MESSAGES + 10 {
            hub.publish(quorum(true));
        }
        let subscription = hub.subscribe("s", Some("e-5"));
        let Start::Resnapshot(reason) = &subscription.start else {
            panic!("expected a resnapshot");
        };
        assert!(reason.contains("too much changed"));
        let edge = format!("e-{}", 10);
        assert_eq!(
            hub.subscribe("s", Some(&edge)).replay().len(),
            REPLAY_MESSAGES
        );
    }

    #[tokio::test]
    async fn a_slow_tab_is_told_to_resnapshot_and_others_carry_on() {
        let hub = EventHub::with_epoch("e");
        let mut slow = hub.subscribe("s", None);
        let mut fast = hub.subscribe("s", None);
        for n in 0..TAB_QUEUE + 5 {
            hub.publish(quorum(n % 2 == 0));
            assert_eq!(ids(&mut fast, 1).await, [format!("e-{}", n + 1)]);
        }
        match slow.next().await {
            Next::Resnapshot(message) => {
                assert!(matches!(message.body, StreamBody::Resnapshot { .. }));
                assert_eq!(message.id, format!("e-{}", TAB_QUEUE + 5));
            }
            other => panic!("{other:?}"),
        }
        hub.publish(quorum(true));
        let id = format!("e-{}", TAB_QUEUE + 6);
        assert_eq!(ids(&mut slow, 1).await, std::slice::from_ref(&id));
        assert_eq!(ids(&mut fast, 1).await, [id]);
    }

    #[test]
    fn the_newest_observations_and_peer_states_are_kept() {
        let hub = EventHub::with_epoch("e");
        let (mox1, mox2) = (HostSlot::new(1).unwrap(), HostSlot::new(2).unwrap());
        let observe = |host, n: u64| StreamBody::Observation {
            host,
            collector: "host".into(),
            data: serde_json::json!({ "n": n }),
        };
        hub.publish(observe(mox1, 1));
        hub.publish(observe(mox1, 2));
        hub.publish_copy(observe(mox2, 3));
        hub.publish_copy(StreamBody::Host {
            host: mox2,
            reachable: false,
            last_seen_ms: 9,
        });
        assert_eq!(hub.observation(mox1, "host").unwrap()["n"], 2);
        assert_eq!(hub.observation(mox2, "host").unwrap()["n"], 3);
        assert!(hub.observation(mox2, "storage").is_none());
        assert_eq!(hub.observations_of(mox1).len(), 1);
        assert_eq!(
            hub.reachability()[&mox2],
            Reachability {
                reachable: false,
                last_seen_ms: 9
            }
        );
    }

    #[test]
    fn only_the_owning_session_changes_a_watch() {
        let hub = EventHub::with_epoch("e");
        let tab = hub.subscribe("alice", None);
        let job: JobId = "6f9619ff-8b86-4011-b42d-00c04fc964ff".parse().unwrap();
        assert_eq!(
            hub.watch("mallory", &tab.stream, vec![job]),
            Err(WatchError::UnknownStream)
        );
        assert_eq!(
            hub.watch("alice", "nope", vec![job]),
            Err(WatchError::UnknownStream)
        );
        assert_eq!(
            hub.watch("alice", &tab.stream, vec![job; MAX_WATCHED + 1]),
            Err(WatchError::TooMany)
        );
        hub.watch("alice", &tab.stream, vec![job]).unwrap();
        assert!(tab.jobs.borrow().contains(&job));
    }
}
