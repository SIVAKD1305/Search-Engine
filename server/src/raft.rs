//! The Raft consensus algorithm (Ongaro & Ousterhout, 2014), as a pure state
//! machine: no clocks, threads, disks or sockets. A driver (`cluster`) feeds
//! it ticks, messages from peers and proposals, and after each call takes a
//! `Ready`: state to make durable, messages to send, and committed entries
//! to apply. It must save before it sends or applies (votes and log
//! acknowledgements are promises about what is on disk).
//!
//! What is implemented:
//!
//! - Leader election with randomized timeouts and Pre-Vote (Raft thesis
//!   §9.6): a node first asks whether it would win, and raises its term only
//!   if a majority says yes. A node that has heard from a leader recently
//!   says no (§4.2.3). So a node cut off from the others cannot raise its
//!   term and depose a working leader when it returns.
//! - Check quorum: a leader that no longer hears from a majority steps
//!   down, so its clients go to the new leader instead of waiting.
//! - Log replication with the consistency check, a conflict hint to skip
//!   back a whole term at a time, batching, and a no-op entry at the start of
//!   each term so earlier entries can commit.
//! - Commitment only of entries from the leader's own term (§5.4.2).
//! - Log compaction: the driver snapshots its state and calls `compact`;
//!   followers too far behind get the snapshot (`InstallSnapshot`).
//!
//! Membership is fixed: the node list given at startup.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub type NodeId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub term: u64,
    pub index: u64,
    /// The driver's command, opaque here (empty for the no-op).
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Message {
    /// "Would you vote for me in `term`?" Changes nothing on the receiver.
    PreVote { term: u64, last_index: u64, last_term: u64 },
    /// `term`: the asked-for term when granted, the receiver's otherwise.
    PreVoteResult { term: u64, granted: bool },
    RequestVote { term: u64, last_index: u64, last_term: u64 },
    Vote { term: u64, granted: bool },
    Append { term: u64, prev_index: u64, prev_term: u64, entries: Vec<LogEntry>, commit: u64 },
    /// `matched`: on success, the last index known to match the leader's
    /// log; on failure, a hint of where to try next.
    AppendResult { term: u64, success: bool, matched: u64 },
    /// The driver sends the snapshot's data along with this message.
    InstallSnapshot { term: u64, last_index: u64, last_term: u64 },
    SnapshotResult { term: u64, last_index: u64 },
}

impl Message {
    pub fn term(&self) -> u64 {
        match self {
            Message::PreVote { term, .. }
            | Message::PreVoteResult { term, .. }
            | Message::RequestVote { term, .. }
            | Message::Vote { term, .. }
            | Message::Append { term, .. }
            | Message::AppendResult { term, .. }
            | Message::InstallSnapshot { term, .. }
            | Message::SnapshotResult { term, .. } => *term,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Follower,
    /// Asking for pre-votes, before raising the term.
    PreCandidate,
    Candidate,
    Leader,
}

/// State that must survive a restart, besides the log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardState {
    pub term: u64,
    pub voted_for: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Ticks without hearing from a leader before starting an election,
    /// drawn at random from this range each time.
    pub election_ticks: (u32, u32),
    /// Ticks between a leader's messages to each follower.
    pub heartbeat_ticks: u32,
    /// Most entries in one `Append`.
    pub max_batch: usize,
    /// Ticks to wait for a snapshot transfer before trying again.
    pub snapshot_timeout_ticks: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { election_ticks: (10, 20), heartbeat_ticks: 2, max_batch: 512, snapshot_timeout_ticks: 600 }
    }
}

/// What the driver must do after a call, in this order: save `hard_state`
/// and the log (`truncate_from`, then `entries`), restore `restore` if set,
/// send `messages`, apply `committed`.
#[derive(Debug, Default)]
pub struct Ready {
    pub hard_state: Option<HardState>,
    /// The saved log is to be cut from this index (inclusive) before
    /// `entries` are appended.
    pub truncate_from: Option<u64>,
    pub entries: Vec<LogEntry>,
    /// A snapshot received from the leader, to load in place of the
    /// current state: (last index, last term).
    pub restore: Option<(u64, u64)>,
    pub messages: Vec<(NodeId, Message)>,
    pub committed: Vec<LogEntry>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProposeError {
    /// Send the proposal to this node instead, if known.
    NotLeader(Option<NodeId>),
}

#[derive(Debug, Clone, Default)]
struct Progress {
    /// Next index to send.
    next: u64,
    /// Highest index known to be on the follower.
    matched: u64,
    /// An `Append` is on its way; more are sent on its answer or a heartbeat.
    in_flight: bool,
    /// Ticks since a snapshot was sent; `None` when none is pending.
    snapshot_sent: Option<u32>,
    /// Answered since the leader last checked for a quorum.
    active: bool,
}

/// A small deterministic generator for election timeouts.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) % u64::from(n.max(1))) as u32
    }
}

#[derive(Debug)]
pub struct Raft {
    id: NodeId,
    peers: Vec<NodeId>,
    config: Config,
    rng: Rng,

    term: u64,
    voted_for: Option<NodeId>,
    /// The log after the snapshot: `entries[0]` has index `snapshot_index + 1`.
    entries: Vec<LogEntry>,
    snapshot_index: u64,
    snapshot_term: u64,

    role: Role,
    leader: Option<NodeId>,
    commit: u64,
    /// Highest index handed to the driver to apply.
    applied: u64,
    elapsed: u32,
    timeout: u32,
    votes: HashSet<NodeId>,
    progress: BTreeMap<NodeId, Progress>,
    /// Ticks since the leader last checked that a majority answers.
    quorum_elapsed: u32,

    ready: Ready,
    hard_state_changed: bool,
}

impl Raft {
    /// A node of the cluster `members` (which includes `id`), starting from
    /// saved state: the hard state, the snapshot's position and the log
    /// entries after it.
    pub fn new(
        id: NodeId,
        members: &[NodeId],
        config: Config,
        hard: HardState,
        snapshot: (u64, u64),
        entries: Vec<LogEntry>,
        seed: u64,
    ) -> Self {
        let peers = members.iter().filter(|m| **m != id).cloned().collect();
        let mut raft = Raft {
            id,
            peers,
            config,
            rng: Rng(seed | 1),
            term: hard.term,
            voted_for: hard.voted_for,
            entries,
            snapshot_index: snapshot.0,
            snapshot_term: snapshot.1,
            role: Role::Follower,
            leader: None,
            commit: snapshot.0,
            applied: snapshot.0,
            elapsed: 0,
            timeout: 0,
            votes: HashSet::new(),
            progress: BTreeMap::new(),
            quorum_elapsed: 0,
            ready: Ready::default(),
            hard_state_changed: false,
        };
        raft.reset_timeout();
        raft
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn term(&self) -> u64 {
        self.term
    }
    pub fn leader(&self) -> Option<&NodeId> {
        self.leader.as_ref()
    }
    pub fn commit_index(&self) -> u64 {
        self.commit
    }
    pub fn applied_index(&self) -> u64 {
        self.applied
    }
    pub fn last_index(&self) -> u64 {
        self.snapshot_index + self.entries.len() as u64
    }
    pub fn snapshot_position(&self) -> (u64, u64) {
        (self.snapshot_index, self.snapshot_term)
    }
    /// (peer, matched index) as the leader knows them.
    pub fn matched(&self) -> Vec<(NodeId, u64)> {
        self.progress.iter().map(|(p, pr)| (p.clone(), pr.matched)).collect()
    }

    fn last_term(&self) -> u64 {
        self.entries.last().map_or(self.snapshot_term, |e| e.term)
    }

    /// The term of the entry at `index`, if this node still knows it.
    pub fn term_at(&self, index: u64) -> Option<u64> {
        if index == self.snapshot_index {
            return Some(self.snapshot_term);
        }
        if index < self.snapshot_index {
            return None;
        }
        self.entries.get((index - self.snapshot_index - 1) as usize).map(|e| e.term)
    }

    fn entries_from(&self, index: u64, max: usize) -> Vec<LogEntry> {
        let start = (index - self.snapshot_index - 1) as usize;
        self.entries.iter().skip(start).take(max).cloned().collect()
    }

    fn quorum(&self) -> usize {
        let members = self.peers.len() + 1;
        members / 2 + 1
    }

    fn reset_timeout(&mut self) {
        let (lo, hi) = self.config.election_ticks;
        self.elapsed = 0;
        self.timeout = lo + self.rng.below(hi.saturating_sub(lo) + 1);
    }

    fn send(&mut self, to: &NodeId, message: Message) {
        self.ready.messages.push((to.clone(), message));
    }

    /// Takes what the driver must do since the last call.
    pub fn take_ready(&mut self) -> Ready {
        let mut ready = std::mem::take(&mut self.ready);
        if let Some(from) = ready.truncate_from {
            ready.entries = self.entries.iter().filter(|e| e.index >= from).cloned().collect();
        }
        if std::mem::take(&mut self.hard_state_changed) {
            ready.hard_state = Some(HardState { term: self.term, voted_for: self.voted_for.clone() });
        }
        if self.applied < self.commit {
            ready.committed = self.entries_from(self.applied + 1, usize::MAX);
            ready.committed.truncate((self.commit - self.applied) as usize);
            self.applied = self.commit;
        }
        ready
    }

    fn mark_log_changed(&mut self, from: u64) {
        // The saved log is cut at the lowest changed index; entries from
        // there on are (re)written (gathered in `take_ready`).
        self.ready.truncate_from = Some(self.ready.truncate_from.map_or(from, |f| f.min(from)));
    }

    fn become_follower(&mut self, term: u64, leader: Option<NodeId>) {
        if term > self.term {
            self.term = term;
            self.voted_for = None;
            self.hard_state_changed = true;
        }
        self.role = Role::Follower;
        self.leader = leader;
        self.votes.clear();
        self.progress.clear();
        self.reset_timeout();
    }

    /// The election timeout passed: ask for pre-votes first.
    fn start_pre_vote(&mut self) {
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes = HashSet::from([self.id.clone()]);
        self.reset_timeout();
        if self.votes.len() >= self.quorum() {
            self.start_election();
            return;
        }
        let (last_index, last_term, term) = (self.last_index(), self.last_term(), self.term + 1);
        for peer in self.peers.clone() {
            self.send(&peer, Message::PreVote { term, last_index, last_term });
        }
    }

    /// Whether this node heard from a leader within the minimum election
    /// timeout, and so votes for nobody.
    fn has_leader_lease(&self) -> bool {
        self.role == Role::Follower && self.leader.is_some() && self.elapsed < self.config.election_ticks.0
    }

    fn start_election(&mut self) {
        self.term += 1;
        self.role = Role::Candidate;
        self.leader = None;
        self.voted_for = Some(self.id.clone());
        self.hard_state_changed = true;
        self.votes = HashSet::from([self.id.clone()]);
        self.reset_timeout();
        if self.votes.len() >= self.quorum() {
            self.become_leader();
            return;
        }
        let (last_index, last_term, term) = (self.last_index(), self.last_term(), self.term);
        for peer in self.peers.clone() {
            self.send(&peer, Message::RequestVote { term, last_index, last_term });
        }
    }

    fn become_leader(&mut self) {
        self.role = Role::Leader;
        self.leader = Some(self.id.clone());
        let next = self.last_index() + 1;
        self.progress =
            self.peers.iter().map(|p| (p.clone(), Progress { next, matched: 0, ..Progress::default() })).collect();
        // A no-op from this term lets entries of earlier terms commit.
        self.append_own(String::new());
        self.elapsed = 0;
        self.quorum_elapsed = 0;
        for peer in self.peers.clone() {
            self.send_append(&peer);
        }
        self.advance_commit();
    }

    fn append_own(&mut self, command: String) -> u64 {
        let index = self.last_index() + 1;
        self.entries.push(LogEntry { term: self.term, index, command });
        self.mark_log_changed(index);
        index
    }

    /// Appends a command if this node is the leader. Returns its index and
    /// term: it is committed if the entry at that index ever applies with
    /// that term.
    pub fn propose(&mut self, command: String) -> Result<(u64, u64), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader(self.leader.clone()));
        }
        let index = self.append_own(command);
        for peer in self.peers.clone() {
            if !self.progress[&peer].in_flight {
                self.send_append(&peer);
            }
        }
        self.advance_commit();
        Ok((index, self.term))
    }

    fn send_append(&mut self, peer: &NodeId) {
        let (snapshot_index, snapshot_term, term) = (self.snapshot_index, self.snapshot_term, self.term);
        let progress = self.progress.get_mut(peer).expect("leader tracks every peer");
        if progress.next <= snapshot_index {
            if progress.snapshot_sent.is_none() {
                progress.snapshot_sent = Some(0);
                let message = Message::InstallSnapshot { term, last_index: snapshot_index, last_term: snapshot_term };
                self.send(peer, message);
            }
            return;
        }
        progress.in_flight = true;
        let next = progress.next;
        let prev_index = next - 1;
        let prev_term = self.term_at(prev_index).expect("prev is at or after the snapshot");
        let entries = self.entries_from(next, self.config.max_batch);
        let message = Message::Append { term, prev_index, prev_term, entries, commit: self.commit };
        self.send(peer, message);
    }

    fn advance_commit(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        let mut matched: Vec<u64> = self.progress.values().map(|p| p.matched).collect();
        matched.push(self.last_index());
        matched.sort_unstable_by(|a, b| b.cmp(a));
        let candidate = matched[self.quorum() - 1];
        // Only entries of the current term are committed by counting (§5.4.2).
        if candidate > self.commit && self.term_at(candidate) == Some(self.term) {
            self.commit = candidate;
        }
    }

    /// One unit of time: elections start and heartbeats go out from here.
    pub fn tick(&mut self) {
        self.elapsed += 1;
        match self.role {
            Role::Leader => {
                // A leader that cannot reach a majority steps down, so its
                // clients go elsewhere instead of waiting (check quorum).
                self.quorum_elapsed += 1;
                if self.quorum_elapsed >= self.config.election_ticks.1 {
                    self.quorum_elapsed = 0;
                    let active = 1 + self.progress.values_mut().filter_map(|p| std::mem::take(&mut p.active).then_some(())).count();
                    if active < self.quorum() {
                        let term = self.term;
                        self.become_follower(term, None);
                        return;
                    }
                }
                for progress in self.progress.values_mut() {
                    if let Some(t) = &mut progress.snapshot_sent {
                        *t += 1;
                        if *t > self.config.snapshot_timeout_ticks {
                            progress.snapshot_sent = None;
                        }
                    }
                }
                if self.elapsed >= self.config.heartbeat_ticks {
                    self.elapsed = 0;
                    for peer in self.peers.clone() {
                        self.send_append(&peer);
                    }
                }
            }
            Role::Follower | Role::PreCandidate | Role::Candidate => {
                if self.elapsed >= self.timeout {
                    self.start_pre_vote();
                }
            }
        }
    }

    /// Handles a message from `from`.
    pub fn step(&mut self, from: &NodeId, message: Message) {
        if !self.peers.contains(from) {
            return;
        }
        let term = message.term();
        if let Message::RequestVote { .. } = message
            && self.has_leader_lease()
            && term > self.term
        {
            // Heard from a leader recently: ignore it, keeping the term.
            return;
        }
        // Pre-votes are hypothetical: their terms change nothing, except a
        // refusal from a node already in a later term.
        let hypothetical = match message {
            Message::PreVote { .. } => true,
            Message::PreVoteResult { granted, .. } => granted,
            _ => false,
        };
        if term > self.term && !hypothetical {
            let leader = matches!(message, Message::Append { .. } | Message::InstallSnapshot { .. }).then(|| from.clone());
            self.become_follower(term, leader);
        }
        match message {
            Message::PreVote { term, last_index, last_term } => {
                let up_to_date = (last_term, last_index) >= (self.last_term(), self.last_index());
                let granted = term > self.term && up_to_date && !self.has_leader_lease();
                let reply_term = if granted { term } else { self.term };
                self.send(from, Message::PreVoteResult { term: reply_term, granted });
            }
            Message::PreVoteResult { term, granted } => {
                if self.role == Role::PreCandidate && granted && term == self.term + 1 {
                    self.votes.insert(from.clone());
                    if self.votes.len() >= self.quorum() {
                        self.start_election();
                    }
                }
            }
            Message::RequestVote { term, last_index, last_term } => {
                let up_to_date = (last_term, last_index) >= (self.last_term(), self.last_index());
                let free = self.voted_for.as_ref().is_none_or(|v| v == from);
                let granted = term == self.term && free && up_to_date;
                if granted {
                    self.voted_for = Some(from.clone());
                    self.hard_state_changed = true;
                    self.reset_timeout();
                }
                self.send(from, Message::Vote { term: self.term, granted });
            }
            Message::Vote { term, granted } => {
                if self.role == Role::Candidate && term == self.term && granted {
                    self.votes.insert(from.clone());
                    if self.votes.len() >= self.quorum() {
                        self.become_leader();
                    }
                }
            }
            Message::Append { term, prev_index, prev_term, entries, commit } => {
                if term < self.term {
                    self.send(from, Message::AppendResult { term: self.term, success: false, matched: 0 });
                    return;
                }
                if self.role != Role::Follower || self.leader.is_none() {
                    self.become_follower(term, Some(from.clone()));
                }
                self.elapsed = 0;
                let result = self.accept_entries(prev_index, prev_term, entries, commit);
                self.send(from, result);
            }
            Message::AppendResult { term, success, matched } => {
                if self.role != Role::Leader || term != self.term {
                    return;
                }
                let Some(progress) = self.progress.get_mut(from) else { return };
                progress.in_flight = false;
                progress.active = true;
                if success {
                    progress.matched = progress.matched.max(matched);
                    progress.next = progress.matched + 1;
                    self.advance_commit();
                    if self.progress[from].next <= self.last_index() {
                        self.send_append(from);
                    }
                } else {
                    // Skip back to the hint, but never below what matched.
                    progress.next = matched.max(progress.matched + 1).min(progress.next.saturating_sub(1)).max(1);
                    self.send_append(from);
                }
            }
            Message::InstallSnapshot { term, last_index, last_term } => {
                if term < self.term {
                    self.send(from, Message::SnapshotResult { term: self.term, last_index: 0 });
                    return;
                }
                if self.role != Role::Follower || self.leader.is_none() {
                    self.become_follower(term, Some(from.clone()));
                }
                self.elapsed = 0;
                self.install_snapshot(last_index, last_term);
                let result = Message::SnapshotResult { term: self.term, last_index: self.commit.max(last_index) };
                self.send(from, result);
            }
            Message::SnapshotResult { term, last_index } => {
                if self.role != Role::Leader || term != self.term {
                    return;
                }
                let Some(progress) = self.progress.get_mut(from) else { return };
                progress.snapshot_sent = None;
                progress.active = true;
                if last_index > 0 {
                    progress.matched = progress.matched.max(last_index);
                    progress.next = progress.matched + 1;
                    self.advance_commit();
                }
                self.send_append(from);
            }
        }
    }

    /// The follower's side of `Append`.
    fn accept_entries(&mut self, prev_index: u64, prev_term: u64, entries: Vec<LogEntry>, commit: u64) -> Message {
        let reject = |hint: u64, term: u64| Message::AppendResult { term, success: false, matched: hint };
        if prev_index > self.last_index() {
            return reject(self.last_index() + 1, self.term);
        }
        // Entries the snapshot already holds match by definition.
        let skip = self.snapshot_index.saturating_sub(prev_index) as usize;
        let (prev_index, prev_term) = if skip > 0 {
            match entries.get(skip - 1) {
                Some(e) => (e.index, e.term),
                None => (self.snapshot_index, self.snapshot_term),
            }
        } else {
            (prev_index, prev_term)
        };
        let entries: Vec<LogEntry> = entries.into_iter().skip(skip).collect();
        match self.term_at(prev_index) {
            Some(t) if t == prev_term => {}
            Some(t) => {
                // Skip the whole conflicting term at once.
                let mut first = prev_index;
                while first > self.snapshot_index + 1 && self.term_at(first - 1) == Some(t) {
                    first -= 1;
                }
                return reject(first.max(self.commit + 1), self.term);
            }
            None => return reject(self.snapshot_index + 1, self.term),
        }
        let last_new = prev_index + entries.len() as u64;
        for entry in entries {
            match self.term_at(entry.index) {
                Some(t) if t == entry.term => continue,
                Some(_) => {
                    // A conflicting suffix: never committed, so it can go.
                    assert!(entry.index > self.commit, "a committed entry conflicts with the leader's log");
                    self.entries.truncate((entry.index - self.snapshot_index - 1) as usize);
                }
                None => {}
            }
            let index = entry.index;
            self.entries.push(entry);
            self.mark_log_changed(index);
        }
        if commit > self.commit {
            self.commit = commit.min(last_new).max(self.commit);
        }
        Message::AppendResult { term: self.term, success: true, matched: last_new }
    }

    fn install_snapshot(&mut self, last_index: u64, last_term: u64) {
        if last_index <= self.commit {
            return; // Already have all of it.
        }
        if self.term_at(last_index) == Some(last_term) {
            // Keep the entries after it.
            let keep = (last_index - self.snapshot_index) as usize;
            self.entries.drain(..keep);
        } else {
            self.entries.clear();
        }
        self.snapshot_index = last_index;
        self.snapshot_term = last_term;
        self.commit = last_index;
        self.applied = last_index;
        self.ready.restore = Some((last_index, last_term));
        // The saved log is replaced by what remains.
        self.ready.truncate_from = Some(0);
    }

    /// The driver has saved a snapshot of its state up to `index` (which it
    /// has applied): entries up to it are dropped.
    pub fn compact(&mut self, index: u64) {
        if index <= self.snapshot_index || index > self.applied {
            return;
        }
        let term = self.term_at(index).expect("applied entries are known");
        let drop = (index - self.snapshot_index) as usize;
        self.entries.drain(..drop);
        self.snapshot_index = index;
        self.snapshot_term = term;
    }

    /// The driver could not deliver a snapshot to `peer`; try again soon.
    pub fn snapshot_failed(&mut self, peer: &NodeId) {
        if let Some(progress) = self.progress.get_mut(peer) {
            progress.snapshot_sent = None;
        }
    }
}

#[cfg(test)]
mod tests {
    //! A simulated cluster: messages are delayed, lost, duplicated and
    //! reordered at random, the network splits and heals, and nodes crash
    //! and restart from what they saved. Raft's safety properties are
    //! checked throughout.

    use super::*;
    use std::collections::HashMap;

    struct Rand(u64);
    impl Rand {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % n.max(1)
        }
        fn chance(&mut self, percent: u64) -> bool {
            self.next(100) < percent
        }
    }

    /// What a node has on disk.
    #[derive(Clone, Default)]
    struct Disk {
        hard: HardState,
        snapshot: (u64, u64),
        /// The applied commands the snapshot holds.
        snapshot_state: Vec<String>,
        log: Vec<LogEntry>,
    }

    struct Node {
        raft: Option<Raft>,
        disk: Disk,
        /// Applied commands, in order (the state machine).
        applied: Vec<String>,
    }

    struct Sim {
        rand: Rand,
        names: Vec<NodeId>,
        nodes: HashMap<NodeId, Node>,
        /// (deliver at, from, to, message)
        network: Vec<(u64, NodeId, NodeId, Message)>,
        now: u64,
        /// Pairs that cannot talk.
        cut: HashSet<(NodeId, NodeId)>,
        /// term -> leader seen.
        leaders: HashMap<u64, NodeId>,
        /// index -> (term, command) committed anywhere.
        committed: BTreeMap<u64, (u64, String)>,
        config: Config,
        loss: u64,
    }

    impl Sim {
        fn new(n: usize, seed: u64, loss: u64) -> Sim {
            let names: Vec<NodeId> = (0..n).map(|i| format!("n{i}")).collect();
            let config = Config { election_ticks: (10, 20), heartbeat_ticks: 2, max_batch: 7, snapshot_timeout_ticks: 30 };
            let mut sim = Sim {
                rand: Rand(seed),
                names: names.clone(),
                nodes: HashMap::new(),
                network: Vec::new(),
                now: 0,
                cut: HashSet::new(),
                leaders: HashMap::new(),
                committed: BTreeMap::new(),
                config,
                loss,
            };
            for name in names {
                sim.nodes.insert(name.clone(), Node { raft: None, disk: Disk::default(), applied: Vec::new() });
                sim.start(&name);
            }
            sim
        }

        fn start(&mut self, name: &NodeId) {
            let seed = self.rand.next(u64::MAX);
            let node = self.nodes.get_mut(name).unwrap();
            let d = node.disk.clone();
            node.applied = d.snapshot_state.clone();
            node.raft = Some(Raft::new(name.clone(), &self.names, self.config.clone(), d.hard, d.snapshot, d.log, seed));
        }

        fn crash(&mut self, name: &NodeId) {
            self.nodes.get_mut(name).unwrap().raft = None;
        }

        /// Does what the node's Ready says, as a driver would.
        fn drive(&mut self, name: &NodeId) {
            let node = self.nodes.get_mut(name).unwrap();
            let Some(raft) = node.raft.as_mut() else { return };
            let ready = raft.take_ready();
            if let Some(hard) = ready.hard_state {
                node.disk.hard = hard;
            }
            if let Some(from) = ready.truncate_from {
                node.disk.log.retain(|e| e.index < from);
                node.disk.log.extend(ready.entries.iter().cloned());
            }
            if let Some((index, term)) = ready.restore {
                // The leader's snapshot: its applied commands up to `index`.
                let state: Vec<String> = self.committed.range(1..=index).map(|(_, (_, c))| c.clone()).collect();
                assert_eq!(state.len() as u64, index, "a snapshot covers committed entries only");
                node.applied = state.clone();
                node.disk.snapshot = (index, term);
                node.disk.snapshot_state = state;
            }
            let role = raft.role();
            let term = raft.term();
            if role == Role::Leader {
                let previous = self.leaders.insert(term, name.clone());
                assert!(previous.is_none_or(|p| p == *name), "two leaders in term {term}");
            }
            for entry in ready.committed {
                assert_eq!(entry.index, node.applied.len() as u64 + 1, "{name} applies in order");
                match self.committed.get(&entry.index) {
                    Some((t, c)) => assert_eq!((*t, c), (entry.term, &entry.command), "state machine safety at {}", entry.index),
                    None => {
                        self.committed.insert(entry.index, (entry.term, entry.command.clone()));
                    }
                }
                node.applied.push(entry.command);
            }
            let now = self.now;
            for (to, message) in ready.messages {
                let pair = (name.clone(), to.clone());
                if self.cut.contains(&pair) || self.rand.chance(self.loss) {
                    continue;
                }
                let copies = if self.rand.chance(3) { 2 } else { 1 };
                for _ in 0..copies {
                    let at = now + 1 + self.rand.next(3);
                    self.network.push((at, name.clone(), to.clone(), message.clone()));
                }
            }
        }

        /// Snapshots a node's applied state now and then, as the driver does.
        fn maybe_compact(&mut self, name: &NodeId) {
            let node = self.nodes.get_mut(name).unwrap();
            let Some(raft) = node.raft.as_mut() else { return };
            let applied = raft.applied_index();
            if applied >= raft.snapshot_position().0 + 15 {
                raft.compact(applied);
                let position = raft.snapshot_position();
                node.disk.snapshot = position;
                node.disk.snapshot_state = node.applied.clone();
                node.disk.log.retain(|e| e.index > position.0);
            }
        }

        fn step_time(&mut self) {
            self.now += 1;
            let now = self.now;
            let mut due = Vec::new();
            self.network.retain(|m| {
                if m.0 <= now {
                    due.push(m.clone());
                    false
                } else {
                    true
                }
            });
            // Reorder what arrives together.
            for i in (1..due.len()).rev() {
                let j = self.rand.next(i as u64 + 1) as usize;
                due.swap(i, j);
            }
            for (_, from, to, message) in due {
                if let Some(raft) = self.nodes.get_mut(&to).unwrap().raft.as_mut() {
                    raft.step(&from, message);
                }
                self.drive(&to);
            }
            for name in self.names.clone() {
                if let Some(raft) = self.nodes.get_mut(&name).unwrap().raft.as_mut() {
                    raft.tick();
                }
                self.drive(&name);
                self.maybe_compact(&name);
            }
            self.check_logs();
        }

        /// Log matching: equal (index, term) means equal history up to it.
        fn check_logs(&self) {
            let logs: Vec<(&Raft, u64)> =
                self.nodes.values().filter_map(|n| n.raft.as_ref()).map(|r| (r, r.commit_index())).collect();
            for (a, _) in &logs {
                for (b, _) in &logs {
                    let start = a.snapshot_index.max(b.snapshot_index) + 1;
                    let end = a.last_index().min(b.last_index());
                    let mut matched = false;
                    for i in (start..=end).rev() {
                        if a.term_at(i) == b.term_at(i) {
                            matched = true;
                        }
                        if matched {
                            assert_eq!(a.term_at(i), b.term_at(i), "log matching at {i}");
                        }
                    }
                }
            }
        }

        fn leader(&self) -> Option<NodeId> {
            let mut leaders: Vec<&Raft> =
                self.nodes.values().filter_map(|n| n.raft.as_ref()).filter(|r| r.role() == Role::Leader).collect();
            leaders.sort_by_key(|r| std::cmp::Reverse(r.term()));
            leaders.first().map(|r| r.id().clone())
        }

        fn propose(&mut self, command: &str) -> bool {
            let Some(leader) = self.leader() else { return false };
            let ok = self.nodes.get_mut(&leader).unwrap().raft.as_mut().unwrap().propose(command.into()).is_ok();
            self.drive(&leader);
            ok
        }

        fn partition(&mut self, groups: &[Vec<NodeId>]) {
            self.cut.clear();
            for a in &self.names {
                for b in &self.names {
                    let together = groups.iter().any(|g| g.contains(a) && g.contains(b));
                    if !together {
                        self.cut.insert((a.clone(), b.clone()));
                    }
                }
            }
        }

        fn heal(&mut self) {
            self.cut.clear();
        }
    }

    #[test]
    fn a_leader_is_elected_and_entries_commit_everywhere() {
        let mut sim = Sim::new(3, 1, 0);
        for _ in 0..100 {
            sim.step_time();
        }
        assert!(sim.leader().is_some());
        for i in 0..20 {
            assert!(sim.propose(&format!("c{i}")));
        }
        for _ in 0..50 {
            sim.step_time();
        }
        for node in sim.nodes.values() {
            let commands: Vec<&String> = node.applied.iter().filter(|c| !c.is_empty()).collect();
            assert_eq!(commands.len(), 20);
        }
    }

    #[test]
    fn a_minority_cannot_commit_and_the_majority_moves_on() {
        let mut sim = Sim::new(5, 2, 0);
        for _ in 0..100 {
            sim.step_time();
        }
        let old = sim.leader().unwrap();
        let others: Vec<NodeId> = sim.names.iter().filter(|n| **n != old).cloned().collect();
        // The old leader and one follower on one side, three on the other.
        sim.partition(&[vec![old.clone(), others[0].clone()], others[1..].to_vec()]);
        sim.step_time();
        let before = sim.committed.len();
        sim.nodes.get_mut(&old).unwrap().raft.as_mut().unwrap().propose("lost".into()).unwrap();
        sim.drive(&old);
        for _ in 0..200 {
            sim.step_time();
        }
        assert!(!sim.committed.values().any(|(_, c)| c == "lost"), "a minority committed");
        // The old leader noticed it lost the majority.
        assert_ne!(sim.nodes[&old].raft.as_ref().unwrap().role(), Role::Leader);
        let new = sim.leader().unwrap();
        assert!(others[1..].contains(&new));
        assert!(sim.propose("kept"));
        for _ in 0..50 {
            sim.step_time();
        }
        assert!(sim.committed.len() > before);
        sim.heal();
        for _ in 0..200 {
            sim.step_time();
        }
        // Everyone agrees, and the minority's entry is gone.
        let reference: Vec<String> = sim.nodes[&new].applied.clone();
        assert!(reference.contains(&"kept".to_owned()) && !reference.contains(&"lost".to_owned()));
        for node in sim.nodes.values() {
            assert_eq!(node.applied, reference);
        }
    }

    #[test]
    fn a_returning_node_does_not_depose_a_working_leader() {
        let mut sim = Sim::new(3, 3, 0);
        for _ in 0..100 {
            sim.step_time();
        }
        let leader = sim.leader().unwrap();
        let term = sim.nodes[&leader].raft.as_ref().unwrap().term();
        let loner = sim.names.iter().find(|n| **n != leader).unwrap().clone();
        let rest: Vec<NodeId> = sim.names.iter().filter(|n| **n != loner).cloned().collect();
        sim.partition(&[vec![loner.clone()], rest]);
        for _ in 0..300 {
            sim.step_time();
        }
        // The loner kept timing out, but without a majority's pre-votes it
        // never raised its term.
        assert_eq!(sim.nodes[&loner].raft.as_ref().unwrap().term(), term);
        assert_eq!(sim.nodes[&loner].raft.as_ref().unwrap().role(), Role::PreCandidate);
        sim.heal();
        for _ in 0..100 {
            sim.step_time();
        }
        let after = sim.nodes[&leader].raft.as_ref().unwrap();
        assert_eq!(after.role(), Role::Leader, "the leader was deposed");
        // The loner caught up and follows.
        assert_eq!(sim.nodes[&loner].raft.as_ref().unwrap().leader(), Some(&leader));
    }

    #[test]
    fn a_far_behind_follower_gets_a_snapshot() {
        let mut sim = Sim::new(3, 4, 0);
        for _ in 0..100 {
            sim.step_time();
        }
        let leader = sim.leader().unwrap();
        let slow = sim.names.iter().find(|n| **n != leader).unwrap().clone();
        sim.crash(&slow);
        for i in 0..100 {
            sim.propose(&format!("c{i}"));
            sim.step_time();
        }
        // The leader compacted past what the slow node has.
        assert!(sim.nodes[&leader].raft.as_ref().unwrap().snapshot_position().0 > 50);
        sim.start(&slow);
        for _ in 0..200 {
            sim.step_time();
        }
        assert_eq!(sim.nodes[&slow].applied, sim.nodes[&leader].applied);
        assert!(sim.nodes[&slow].disk.snapshot.0 > 0);
    }

    /// Random faults for a long time; safety is checked at every step, and
    /// the cluster must make progress once things calm down.
    /// `RAFT_SEEDS=1000` runs longer.
    #[test]
    fn safety_holds_under_random_faults() {
        let seeds: u64 = std::env::var("RAFT_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
        for seed in 0..seeds {
            let n = if seed % 2 == 0 { 3 } else { 5 };
            let mut sim = Sim::new(n, 100 + seed, 10);
            let mut proposed = 0;
            for round in 0..1500 {
                match sim.rand.next(100) {
                    0..=1 => {
                        let name = sim.names[sim.rand.next(n as u64) as usize].clone();
                        sim.crash(&name);
                    }
                    2..=5 => {
                        let name = sim.names[sim.rand.next(n as u64) as usize].clone();
                        if sim.nodes[&name].raft.is_none() {
                            sim.start(&name);
                        }
                    }
                    6 => {
                        let mut shuffled = sim.names.clone();
                        for i in (1..shuffled.len()).rev() {
                            let j = sim.rand.next(i as u64 + 1) as usize;
                            shuffled.swap(i, j);
                        }
                        let split = 1 + sim.rand.next(n as u64 - 1) as usize;
                        sim.partition(&[shuffled[..split].to_vec(), shuffled[split..].to_vec()]);
                    }
                    7..=9 => sim.heal(),
                    10..=40 if sim.propose(&format!("s{seed}-{round}")) => proposed += 1,
                    _ => {}
                }
                sim.step_time();
            }
            // Calm: everyone up, no partitions, no loss.
            sim.heal();
            sim.loss = 0;
            for name in sim.names.clone() {
                if sim.nodes[&name].raft.is_none() {
                    sim.start(&name);
                }
            }
            for _ in 0..300 {
                sim.step_time();
            }
            assert!(sim.propose("final"), "seed {seed}: no leader after calming down");
            for _ in 0..200 {
                sim.step_time();
            }
            let reference = sim.nodes[&sim.names[0]].applied.clone();
            assert!(reference.contains(&"final".to_owned()), "seed {seed}");
            for node in sim.nodes.values() {
                assert_eq!(node.applied, reference, "seed {seed}: nodes disagree");
            }
            assert!(proposed > 0);
        }
    }

    #[test]
    fn a_single_node_cluster_commits_alone() {
        let mut raft = Raft::new("a".into(), &["a".into()], Config::default(), HardState::default(), (0, 0), vec![], 1);
        for _ in 0..25 {
            raft.tick();
        }
        assert_eq!(raft.role(), Role::Leader);
        let (index, _) = raft.propose("x".into()).unwrap();
        let ready = raft.take_ready();
        assert!(ready.committed.iter().any(|e| e.index == index && e.command == "x"));
        assert!(ready.entries.iter().any(|e| e.index == index), "saved before applied");
    }

    #[test]
    fn followers_redirect_proposals() {
        let mut raft = Raft::new("a".into(), &["a".into(), "b".into()], Config::default(), HardState::default(), (0, 0), vec![], 1);
        assert_eq!(raft.propose("x".into()), Err(ProposeError::NotLeader(None)));
        raft.step(&"b".into(), Message::Append { term: 1, prev_index: 0, prev_term: 0, entries: vec![], commit: 0 });
        assert_eq!(raft.propose("x".into()), Err(ProposeError::NotLeader(Some("b".into()))));
    }
}
