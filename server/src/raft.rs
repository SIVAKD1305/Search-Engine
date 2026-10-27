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
//! - Membership changes, one voter at a time (thesis §4.1): the leader
//!   appends an entry holding the whole new membership, which takes effect
//!   on every node as soon as it is in that node's log, not when it commits.
//!   Any majority of the old voters and any majority of the new ones
//!   overlap when the voters differ by one, so no two leaders can exist in
//!   one term. The leader accepts a change only when the previous one has
//!   committed and it has committed an entry of its own term (without the
//!   second condition, two successive leaders could each be elected by a
//!   majority of a different membership). New nodes join as learners:
//!   they receive the log but neither vote nor count towards commitment,
//!   so a node that is still catching up cannot stall the cluster. A node
//!   that is not a voter never starts an election, and a leader that is
//!   removed leads, without counting itself, until its removal commits.
//!   The leader keeps sending to a removed node for a short while so that
//!   it learns it was removed (a node that never hears stays harmless:
//!   Pre-Vote and the up-to-date check keep it from disrupting anything).
//!   Snapshots carry the membership as of their last entry.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub type NodeId = String;

/// Ticks a leader goes on trying to tell a removed node that it was removed.
const FAREWELL_TICKS: u32 = 200;

/// A node of the cluster. `url` is opaque here: the driver's address for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: NodeId,
    pub url: String,
}

/// Who is in the cluster: voters decide, learners only follow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    pub voters: Vec<Member>,
    #[serde(default)]
    pub learners: Vec<Member>,
}

impl Membership {
    /// A membership of voters only.
    pub fn of_voters(voters: Vec<Member>) -> Self {
        Membership { voters, learners: Vec::new() }
    }

    pub fn is_voter(&self, id: &str) -> bool {
        self.voters.iter().any(|m| m.id == id)
    }

    pub fn is_learner(&self, id: &str) -> bool {
        self.learners.iter().any(|m| m.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.is_voter(id) || self.is_learner(id)
    }

    /// Voters, then learners.
    pub fn all(&self) -> impl Iterator<Item = &Member> {
        self.voters.iter().chain(&self.learners)
    }

    pub fn url_of(&self, id: &str) -> Option<&str> {
        self.all().find(|m| m.id == id).map(|m| m.url.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub term: u64,
    pub index: u64,
    /// The driver's command, opaque here (empty for the no-op and for
    /// membership changes).
    pub command: String,
    /// Set on a membership change: the whole new membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Membership>,
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
    /// `members`: the membership as of `last_index`.
    InstallSnapshot { term: u64, last_index: u64, last_term: u64, members: Membership },
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
    /// Most command bytes in one `Append` (at least one entry is sent).
    pub max_batch_bytes: usize,
    /// Ticks to wait for a snapshot transfer before trying again.
    pub snapshot_timeout_ticks: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            election_ticks: (10, 20),
            heartbeat_ticks: 2,
            max_batch: 512,
            max_batch_bytes: 16 * 1024 * 1024,
            snapshot_timeout_ticks: 600,
        }
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
    /// current state: (last index, last term, membership at that entry).
    pub restore: Option<(u64, u64, Membership)>,
    pub messages: Vec<(NodeId, Message)>,
    pub committed: Vec<LogEntry>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProposeError {
    /// Send the proposal to this node instead, if known.
    NotLeader(Option<NodeId>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum MembershipError {
    NotLeader(Option<NodeId>),
    /// The previous membership change has not committed yet.
    Pending,
    /// This leader has not committed an entry of its own term yet.
    NotReady,
    /// The change is not allowed (more than one voter at a time, nobody left
    /// to vote, a node in both lists, no change at all).
    Invalid(String),
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
    /// The commit index in the last `Append` sent.
    commit_sent: u64,
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
    /// The membership as of the snapshot, and the changes in the log after
    /// it (index, membership). The last of them, else the snapshot's, is in
    /// effect.
    snapshot_members: Membership,
    member_log: Vec<(u64, Membership)>,

    role: Role,
    leader: Option<NodeId>,
    commit: u64,
    /// Highest index handed to the driver to apply.
    applied: u64,
    elapsed: u32,
    timeout: u32,
    /// Ticks since the last message from a leader this node follows (not
    /// reset by election timers, which a node that never campaigns, such as
    /// a learner, still runs).
    heard: u32,
    votes: HashSet<NodeId>,
    progress: BTreeMap<NodeId, Progress>,
    /// Ticks since the leader last checked that a majority answers.
    quorum_elapsed: u32,
    /// The index of the no-op this leader appended when elected.
    term_start: u64,
    /// Nodes just removed that this leader still tries to tell: the index
    /// of the entry that removed them, and the ticks left to try. They
    /// stay in `progress`, but are no peers.
    departing: BTreeMap<NodeId, (u64, u32)>,

    ready: Ready,
    hard_state_changed: bool,
}

impl Raft {
    /// A node starting from saved state: the hard state, the snapshot's
    /// position and its membership (for a node that has never seen one: the
    /// cluster it is started in, or none to wait to be added), and the log
    /// entries after the snapshot.
    pub fn new(
        id: NodeId,
        config: Config,
        hard: HardState,
        snapshot: (u64, u64),
        snapshot_members: Membership,
        entries: Vec<LogEntry>,
        seed: u64,
    ) -> Self {
        let member_log = entries.iter().filter_map(|e| e.members.clone().map(|m| (e.index, m))).collect();
        let mut raft = Raft {
            id,
            peers: Vec::new(),
            config,
            rng: Rng(seed | 1),
            term: hard.term,
            voted_for: hard.voted_for,
            entries,
            snapshot_index: snapshot.0,
            snapshot_term: snapshot.1,
            snapshot_members,
            member_log,
            role: Role::Follower,
            leader: None,
            commit: snapshot.0,
            applied: snapshot.0,
            elapsed: 0,
            timeout: 0,
            heard: u32::MAX / 2,
            votes: HashSet::new(),
            progress: BTreeMap::new(),
            quorum_elapsed: 0,
            term_start: 0,
            departing: BTreeMap::new(),
            ready: Ready::default(),
            hard_state_changed: false,
        };
        raft.refresh_members();
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
    /// The log entries after the snapshot.
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// The membership in effect: the last change in the log, else the
    /// snapshot's.
    pub fn members(&self) -> &Membership {
        self.member_log.last().map_or(&self.snapshot_members, |(_, m)| m)
    }
    /// The membership as of the snapshot.
    pub fn snapshot_members(&self) -> &Membership {
        &self.snapshot_members
    }
    /// Whether this node is a voter in the membership in effect.
    pub fn is_voter(&self) -> bool {
        self.members().is_voter(&self.id)
    }

    /// Whether this node may start an election: voters may, and so may a
    /// node whose own removal is in its log but not committed yet. Its log
    /// can be the only one holding that change (a leader that removed itself
    /// and lost leadership before replicating it), and if the voters that
    /// remain are behind it they can never elect themselves without its
    /// vote. Votes still come only from the voters of its membership, and a
    /// term has at most one winner, as the node that is being removed and
    /// the voters that still believe it is one need the same voter's vote.
    /// Once the removal commits, it can no longer stand.
    fn can_campaign(&self) -> bool {
        if self.is_voter() {
            return true;
        }
        let Some((index, _)) = self.member_log.last() else { return false };
        if *index <= self.commit {
            return false;
        }
        let before = match self.member_log.len() {
            1 => &self.snapshot_members,
            n => &self.member_log[n - 2].1,
        };
        before.is_voter(&self.id)
    }

    /// (peer, matched index) as the leader knows them: voters and learners.
    pub fn matched(&self) -> Vec<(NodeId, u64)> {
        self.progress.iter().filter(|(id, _)| self.peers.contains(id)).map(|(p, pr)| (p.clone(), pr.matched)).collect()
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
        self.members().voters.len() / 2 + 1
    }

    /// Votes received from current voters.
    fn votes_granted(&self) -> usize {
        let members = self.members();
        self.votes.iter().filter(|v| members.is_voter(v)).count()
    }

    /// The other voters.
    fn voter_peers(&self) -> Vec<NodeId> {
        self.members().voters.iter().filter(|m| m.id != self.id).map(|m| m.id.clone()).collect()
    }

    /// Recomputes who the peers are after the membership in effect may have
    /// changed; a leader starts tracking new peers and forgets removed ones.
    fn refresh_members(&mut self) {
        let members = self.members();
        let peers: Vec<NodeId> = members.all().filter(|m| m.id != self.id).map(|m| m.id.clone()).collect();
        let campaign = self.can_campaign();
        if self.role == Role::Leader {
            let next = self.last_index() + 1;
            let change = self.member_log.last().map_or(0, |(index, _)| *index);
            for id in self.progress.keys() {
                if !peers.contains(id) {
                    self.departing.entry(id.clone()).or_insert((change, FAREWELL_TICKS));
                }
            }
            for peer in &peers {
                // A node added again is a peer again.
                self.departing.remove(peer);
                self.progress.entry(peer.clone()).or_insert_with(|| Progress { next, ..Progress::default() });
            }
        }
        self.peers = peers;
        if !campaign && matches!(self.role, Role::PreCandidate | Role::Candidate) {
            self.role = Role::Follower;
            self.votes.clear();
        }
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
        self.departing.clear();
        self.reset_timeout();
    }

    /// The election timeout passed: ask for pre-votes first.
    fn start_pre_vote(&mut self) {
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes = HashSet::from([self.id.clone()]);
        self.reset_timeout();
        if self.votes_granted() >= self.quorum() {
            self.start_election();
            return;
        }
        let (last_index, last_term, term) = (self.last_index(), self.last_term(), self.term + 1);
        for peer in self.voter_peers() {
            self.send(&peer, Message::PreVote { term, last_index, last_term });
        }
    }

    /// Whether this node heard from a leader within the minimum election
    /// timeout, and so votes for nobody.
    fn has_leader_lease(&self) -> bool {
        self.role == Role::Follower && self.leader.is_some() && self.heard < self.config.election_ticks.0
    }

    fn start_election(&mut self) {
        self.term += 1;
        self.role = Role::Candidate;
        self.leader = None;
        self.voted_for = Some(self.id.clone());
        self.hard_state_changed = true;
        self.votes = HashSet::from([self.id.clone()]);
        self.reset_timeout();
        if self.votes_granted() >= self.quorum() {
            self.become_leader();
            return;
        }
        let (last_index, last_term, term) = (self.last_index(), self.last_term(), self.term);
        for peer in self.voter_peers() {
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
        self.term_start = self.append_own(String::new());
        self.elapsed = 0;
        self.quorum_elapsed = 0;
        for peer in self.peers.clone() {
            self.send_append(&peer);
        }
        self.advance_commit();
    }

    fn append_own(&mut self, command: String) -> u64 {
        let index = self.last_index() + 1;
        self.entries.push(LogEntry { term: self.term, index, command, members: None });
        self.mark_log_changed(index);
        index
    }

    /// Appends a membership change if this node is the leader and the change
    /// is allowed now. Returns its index and term, like `propose`. The new
    /// membership is in effect from this moment.
    pub fn propose_membership(&mut self, new: Membership) -> Result<(u64, u64), MembershipError> {
        if self.role != Role::Leader {
            return Err(MembershipError::NotLeader(self.leader.clone()));
        }
        if self.member_log.last().is_some_and(|(index, _)| *index > self.commit) {
            return Err(MembershipError::Pending);
        }
        if self.commit < self.term_start {
            return Err(MembershipError::NotReady);
        }
        let invalid = |m: &str| Err(MembershipError::Invalid(m.to_owned()));
        let current = self.members();
        if new == *current {
            return invalid("the membership would not change");
        }
        if new.voters.is_empty() {
            return invalid("a cluster needs at least one voter");
        }
        let mut seen = HashSet::new();
        if new.all().any(|m| m.id.is_empty() || !seen.insert(m.id.as_str())) {
            return invalid("every node needs a distinct, non-empty id");
        }
        let changed = new.voters.iter().filter(|m| !current.is_voter(&m.id)).count()
            + current.voters.iter().filter(|m| !new.is_voter(&m.id)).count();
        if changed > 1 {
            return invalid("voters change one at a time");
        }
        let index = self.last_index() + 1;
        self.entries.push(LogEntry { term: self.term, index, command: String::new(), members: Some(new.clone()) });
        self.member_log.push((index, new));
        self.mark_log_changed(index);
        self.refresh_members();
        for peer in self.peers.clone() {
            if !self.progress[&peer].in_flight {
                self.send_append(&peer);
            }
        }
        self.advance_commit();
        Ok((index, self.term))
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
        let members = self.snapshot_members.clone();
        let progress = self.progress.get_mut(peer).expect("leader tracks every peer");
        if progress.next <= snapshot_index {
            if progress.snapshot_sent.is_none() {
                progress.snapshot_sent = Some(0);
                let message = Message::InstallSnapshot { term, last_index: snapshot_index, last_term: snapshot_term, members };
                self.send(peer, message);
            }
            return;
        }
        progress.in_flight = true;
        let next = progress.next;
        let prev_index = next - 1;
        let prev_term = self.term_at(prev_index).expect("prev is at or after the snapshot");
        let mut entries = self.entries_from(next, self.config.max_batch);
        let mut bytes = 0;
        let fits = entries
            .iter()
            .position(|e| {
                bytes += e.command.len();
                bytes > self.config.max_batch_bytes
            })
            .unwrap_or(entries.len());
        entries.truncate(fits.max(1));
        let commit = self.commit;
        self.progress.get_mut(peer).expect("leader tracks every peer").commit_sent = commit;
        let message = Message::Append { term, prev_index, prev_term, entries, commit };
        self.send(peer, message);
    }

    /// Sends a removed node what it needs to learn that it was removed, or
    /// gives up on one that is too far behind to be worth a snapshot.
    fn send_farewell(&mut self, peer: &NodeId) {
        let Some(progress) = self.progress.get(peer) else { return };
        if progress.next <= self.snapshot_index {
            self.departing.remove(peer);
            self.progress.remove(peer);
            return;
        }
        self.send_append(peer);
    }

    fn advance_commit(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        // Only voters count, this node among them only if it is one.
        let mut matched: Vec<u64> = self
            .members()
            .voters
            .iter()
            .map(|m| if m.id == self.id { self.last_index() } else { self.progress.get(&m.id).map_or(0, |p| p.matched) })
            .collect();
        if matched.is_empty() {
            return;
        }
        matched.sort_unstable_by(|a, b| b.cmp(a));
        let candidate = matched[self.quorum() - 1];
        // Only entries of the current term are committed by counting (§5.4.2).
        if candidate > self.commit && self.term_at(candidate) == Some(self.term) {
            self.commit = candidate;
            // Tell idle followers now rather than at the next heartbeat, so
            // they apply (and answer their clients) sooner.
            for peer in self.peers.clone() {
                if !self.progress[&peer].in_flight {
                    self.send_append(&peer);
                }
            }
            // A leader that is no longer a voter has led the cluster until
            // the change that removed it committed.
            if !self.is_voter() && self.member_log.last().is_some_and(|(index, _)| *index <= self.commit) {
                let term = self.term;
                self.become_follower(term, None);
            }
        }
    }

    /// One unit of time: elections start and heartbeats go out from here.
    pub fn tick(&mut self) {
        self.elapsed += 1;
        self.heard = self.heard.saturating_add(1);
        match self.role {
            Role::Leader => {
                // A leader that cannot reach a majority steps down, so its
                // clients go elsewhere instead of waiting (check quorum).
                self.quorum_elapsed += 1;
                if self.quorum_elapsed >= self.config.election_ticks.1 {
                    self.quorum_elapsed = 0;
                    let members = self.members();
                    let active = members
                        .voters
                        .iter()
                        .filter(|m| m.id == self.id || self.progress.get(&m.id).is_some_and(|p| p.active))
                        .count();
                    for progress in self.progress.values_mut() {
                        progress.active = false;
                    }
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
                let mut gone = Vec::new();
                for (id, (_, ticks)) in &mut self.departing {
                    *ticks = ticks.saturating_sub(1);
                    if *ticks == 0 {
                        gone.push(id.clone());
                    }
                }
                for id in gone {
                    self.departing.remove(&id);
                    self.progress.remove(&id);
                }
                if self.elapsed >= self.config.heartbeat_ticks {
                    self.elapsed = 0;
                    for peer in self.peers.clone() {
                        self.send_append(&peer);
                    }
                    for peer in self.departing.keys().cloned().collect::<Vec<_>>() {
                        if !self.progress[&peer].in_flight {
                            self.send_farewell(&peer);
                        }
                    }
                }
            }
            Role::Follower | Role::PreCandidate | Role::Candidate => {
                if self.elapsed >= self.timeout {
                    if self.can_campaign() {
                        self.start_pre_vote();
                    } else {
                        // Learners, removed nodes and nodes not yet added wait.
                        self.reset_timeout();
                    }
                }
            }
        }
    }

    /// Handles a message from `from`.
    pub fn step(&mut self, from: &NodeId, message: Message) {
        // Messages from nodes this one does not know are answered: a node
        // may be ahead of this one in learning who is in the cluster.
        if *from == self.id {
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
                    if self.votes_granted() >= self.quorum() {
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
                    if self.votes_granted() >= self.quorum() {
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
                self.heard = 0;
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
                    if let Some(&(change, _)) = self.departing.get(from) {
                        // Told once it has the entry that removed it.
                        if self.progress[from].matched >= change {
                            self.departing.remove(from);
                            self.progress.remove(from);
                        } else {
                            self.send_farewell(from);
                        }
                        return;
                    }
                    self.advance_commit();
                    // Committing may have been the leader's own removal.
                    if self.role != Role::Leader {
                        return;
                    }
                    let p = &self.progress[from];
                    if !p.in_flight && (p.next <= self.last_index() || p.commit_sent < self.commit) {
                        self.send_append(from);
                    }
                } else {
                    // Skip back to the hint, but never below what matched.
                    progress.next = matched.max(progress.matched + 1).min(progress.next.saturating_sub(1)).max(1);
                    if self.departing.contains_key(from) {
                        self.send_farewell(from);
                    } else {
                        self.send_append(from);
                    }
                }
            }
            Message::InstallSnapshot { term, last_index, last_term, members } => {
                if term < self.term {
                    self.send(from, Message::SnapshotResult { term: self.term, last_index: 0 });
                    return;
                }
                if self.role != Role::Follower || self.leader.is_none() {
                    self.become_follower(term, Some(from.clone()));
                }
                self.elapsed = 0;
                self.heard = 0;
                self.install_snapshot(last_index, last_term, members);
                let result = Message::SnapshotResult { term: self.term, last_index: self.commit.max(last_index) };
                self.send(from, result);
            }
            Message::SnapshotResult { term, last_index } => {
                if self.role != Role::Leader || term != self.term {
                    return;
                }
                if self.departing.remove(from).is_some() {
                    // Removed while a snapshot was on its way: not worth more.
                    self.progress.remove(from);
                    return;
                }
                let Some(progress) = self.progress.get_mut(from) else { return };
                progress.snapshot_sent = None;
                progress.active = true;
                if last_index > 0 {
                    progress.matched = progress.matched.max(last_index);
                    progress.next = progress.matched + 1;
                    self.advance_commit();
                    if self.role != Role::Leader {
                        return;
                    }
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
        let mut members_changed = false;
        for entry in entries {
            match self.term_at(entry.index) {
                Some(t) if t == entry.term => continue,
                Some(_) => {
                    // A conflicting suffix: never committed, so it can go,
                    // and so can the membership changes in it.
                    assert!(entry.index > self.commit, "a committed entry conflicts with the leader's log");
                    self.entries.truncate((entry.index - self.snapshot_index - 1) as usize);
                    while self.member_log.last().is_some_and(|(index, _)| *index >= entry.index) {
                        self.member_log.pop();
                        members_changed = true;
                    }
                }
                None => {}
            }
            let index = entry.index;
            if let Some(members) = &entry.members {
                self.member_log.push((index, members.clone()));
                members_changed = true;
            }
            self.entries.push(entry);
            self.mark_log_changed(index);
        }
        if members_changed {
            self.refresh_members();
        }
        if commit > self.commit {
            self.commit = commit.min(last_new).max(self.commit);
        }
        Message::AppendResult { term: self.term, success: true, matched: last_new }
    }

    fn install_snapshot(&mut self, last_index: u64, last_term: u64, members: Membership) {
        if last_index <= self.commit {
            return; // Already have all of it.
        }
        if self.term_at(last_index) == Some(last_term) {
            // Keep the entries after it, and their membership changes.
            let keep = (last_index - self.snapshot_index) as usize;
            self.entries.drain(..keep);
            self.member_log.retain(|(index, _)| *index > last_index);
        } else {
            self.entries.clear();
            self.member_log.clear();
        }
        self.snapshot_index = last_index;
        self.snapshot_term = last_term;
        self.snapshot_members = members.clone();
        self.refresh_members();
        self.commit = last_index;
        self.applied = last_index;
        self.ready.restore = Some((last_index, last_term, members));
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
        // The membership as of the snapshot, and what remains to be logged.
        let covered = self.member_log.partition_point(|(i, _)| *i <= index);
        if let Some((_, members)) = self.member_log[..covered].last() {
            self.snapshot_members = members.clone();
        }
        self.member_log.drain(..covered);
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

    fn member(id: &str) -> Member {
        Member { id: id.into(), url: format!("http://{id}") }
    }

    /// The membership of the first `n` nodes of the pool, all voters.
    fn first_voters(n: usize) -> Membership {
        Membership::of_voters((0..n).map(|i| member(&format!("n{i}"))).collect())
    }

    /// What a node has on disk.
    #[derive(Clone, Default)]
    struct Disk {
        hard: HardState,
        snapshot: (u64, u64),
        /// The membership as of the snapshot (or, with no snapshot, the
        /// cluster the node was started in).
        snapshot_members: Membership,
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
            Sim::with_pool(n, n, seed, loss)
        }

        /// `pool` nodes exist; the first `n` form the cluster, the others
        /// wait to be added.
        fn with_pool(n: usize, pool: usize, seed: u64, loss: u64) -> Sim {
            let names: Vec<NodeId> = (0..pool).map(|i| format!("n{i}")).collect();
            let config = Config {
                election_ticks: (10, 20),
                heartbeat_ticks: 2,
                max_batch: 7,
                max_batch_bytes: 40,
                snapshot_timeout_ticks: 30,
            };
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
            for (i, name) in names.into_iter().enumerate() {
                let snapshot_members = if i < n { first_voters(n) } else { Membership::default() };
                let disk = Disk { snapshot_members, ..Disk::default() };
                sim.nodes.insert(name.clone(), Node { raft: None, disk, applied: Vec::new() });
                sim.start(&name);
            }
            sim
        }

        fn start(&mut self, name: &NodeId) {
            let seed = self.rand.next(u64::MAX);
            let node = self.nodes.get_mut(name).unwrap();
            let d = node.disk.clone();
            node.applied = d.snapshot_state.clone();
            node.raft =
                Some(Raft::new(name.clone(), self.config.clone(), d.hard, d.snapshot, d.snapshot_members, d.log, seed));
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
            if let Some((index, term, members)) = ready.restore {
                // The leader's snapshot: its applied commands up to `index`.
                let state: Vec<String> = self.committed.range(1..=index).map(|(_, (_, c))| c.clone()).collect();
                assert_eq!(state.len() as u64, index, "a snapshot covers committed entries only");
                node.applied = state.clone();
                node.disk.snapshot = (index, term);
                node.disk.snapshot_members = members;
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
                // A membership change is a command too: all nodes must agree on it.
                let command = match &entry.members {
                    Some(m) => format!("members:{}", serde_json::to_string(m).unwrap()),
                    None => entry.command.clone(),
                };
                match self.committed.get(&entry.index) {
                    Some((t, c)) => assert_eq!((*t, c), (entry.term, &command), "state machine safety at {}", entry.index),
                    None => {
                        self.committed.insert(entry.index, (entry.term, command.clone()));
                    }
                }
                node.applied.push(command);
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
                node.disk.snapshot_members = raft.snapshot_members().clone();
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

        fn run(&mut self, steps: usize) {
            for _ in 0..steps {
                self.step_time();
            }
        }

        fn raft(&self, name: &str) -> &Raft {
            self.nodes[name].raft.as_ref().unwrap_or_else(|| panic!("{name} is down"))
        }

        /// The membership the leader has in effect.
        fn members(&self) -> Membership {
            self.raft(&self.leader().expect("a leader")).members().clone()
        }

        /// Asks the leader for a membership change.
        fn change(&mut self, new: Membership) -> Result<(u64, u64), MembershipError> {
            let Some(leader) = self.leader() else { return Err(MembershipError::NotLeader(None)) };
            let result = self.nodes.get_mut(&leader).unwrap().raft.as_mut().unwrap().propose_membership(new);
            self.drive(&leader);
            result
        }

        /// Adds `name` as a learner, one change at a time, as a driver would.
        fn add_learner(&mut self, name: &str) {
            let mut members = self.members();
            members.learners.push(member(name));
            self.change(members).unwrap();
            self.run(60);
        }

        fn promote(&mut self, name: &str) {
            let mut members = self.members();
            members.learners.retain(|m| m.id != name);
            members.voters.push(member(name));
            self.change(members).unwrap();
            self.run(60);
        }

        fn remove(&mut self, name: &str) {
            let mut members = self.members();
            members.voters.retain(|m| m.id != name);
            members.learners.retain(|m| m.id != name);
            self.change(members).unwrap();
            self.run(60);
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
    fn a_node_joins_as_a_learner_and_is_then_promoted() {
        let mut sim = Sim::with_pool(3, 4, 11, 0);
        sim.run(100);
        // Enough history that the leader has compacted its log.
        for i in 0..40 {
            assert!(sim.propose(&format!("c{i}")));
            sim.step_time();
        }
        let leader = sim.leader().unwrap();
        assert!(sim.raft(&leader).snapshot_position().0 > 0);
        let term = sim.raft(&leader).term();

        sim.add_learner("n3");
        // The newcomer got the snapshot, then the rest of the log, and the
        // membership with it, without voting or raising the term.
        let joined = sim.raft("n3");
        assert!(joined.members().is_learner("n3") && !joined.is_voter());
        assert_eq!(joined.term(), term);
        assert_eq!(joined.role(), Role::Follower);
        assert_eq!(sim.nodes["n3"].applied, sim.nodes[&leader].applied);
        assert!(sim.nodes["n3"].disk.snapshot.0 > 0, "it was restored from a snapshot");
        assert!(sim.nodes["n3"].disk.snapshot_members.contains("n3") || sim.raft("n3").members().contains("n3"));

        sim.promote("n3");
        for name in ["n0", "n1", "n2", "n3"] {
            assert_eq!(sim.raft(name).members().voters.len(), 4, "{name}");
        }
        // Four voters: a majority is three, so one can be down...
        let down = sim.names.iter().find(|n| **n != leader && **n != "n3").unwrap().clone();
        sim.crash(&down);
        assert!(sim.propose("with-three"));
        sim.run(60);
        assert!(sim.committed.values().any(|(_, c)| c == "with-three"));
        // ...but not two.
        let other = sim.names.iter().find(|n| **n != leader && **n != down).unwrap().clone();
        sim.crash(&other);
        sim.propose("with-two");
        sim.run(200);
        assert!(!sim.committed.values().any(|(_, c)| c == "with-two"), "two of four voters committed");
    }

    #[test]
    fn a_learner_that_is_down_does_not_stall_the_cluster() {
        let mut sim = Sim::with_pool(3, 4, 12, 0);
        sim.run(100);
        sim.crash(&"n3".to_string());
        let mut members = sim.members();
        members.learners.push(member("n3"));
        sim.change(members).unwrap();
        sim.run(30);
        for i in 0..20 {
            assert!(sim.propose(&format!("c{i}")));
        }
        sim.run(60);
        assert_eq!(sim.committed.values().filter(|(_, c)| c.starts_with('c')).count(), 20);
    }

    #[test]
    fn a_voter_is_removed_and_the_rest_carry_on() {
        let mut sim = Sim::with_pool(3, 3, 13, 0);
        sim.run(100);
        let leader = sim.leader().unwrap();
        let gone = sim.names.iter().find(|n| **n != leader).unwrap().clone();
        sim.remove(&gone);
        for name in sim.names.clone() {
            assert_eq!(sim.raft(&name).members().voters.len(), 2, "{name}");
        }
        // The removed node was told, and no longer takes part.
        assert!(!sim.raft(&gone).is_voter());
        let term = sim.raft(&leader).term();
        sim.run(300);
        assert_eq!(sim.raft(&gone).term(), term, "a removed node started elections");
        assert_eq!(sim.raft(&leader).role(), Role::Leader);
        // Two voters left: both are needed.
        let other = sim.names.iter().find(|n| **n != leader && **n != gone).unwrap().clone();
        assert!(sim.propose("both"));
        sim.run(40);
        assert!(sim.committed.values().any(|(_, c)| c == "both"));
        sim.crash(&other);
        sim.propose("alone");
        sim.run(200);
        assert!(!sim.committed.values().any(|(_, c)| c == "alone"));
    }

    #[test]
    fn a_leader_can_remove_itself() {
        let mut sim = Sim::with_pool(3, 3, 14, 0);
        sim.run(100);
        for i in 0..5 {
            sim.propose(&format!("c{i}"));
        }
        let old = sim.leader().unwrap();
        sim.remove(&old);
        sim.run(200);
        // It handed over: someone else leads, the old leader follows and never campaigns.
        let new = sim.leader().unwrap();
        assert_ne!(new, old);
        assert!(!sim.raft(&old).is_voter());
        assert_ne!(sim.raft(&old).role(), Role::Leader);
        let term = sim.raft(&new).term();
        assert!(sim.propose("after"));
        sim.run(100);
        assert!(sim.committed.values().any(|(_, c)| c == "after"));
        assert_eq!(sim.raft(&new).term(), term, "the removed leader disturbed the cluster");
        assert_eq!(sim.raft(&new).members().voters.len(), 2);
    }

    #[test]
    fn only_allowed_changes_are_accepted() {
        let mut sim = Sim::with_pool(3, 5, 15, 0);
        sim.run(100);
        let current = sim.members();
        let mut two_added = current.clone();
        two_added.voters.push(member("n3"));
        two_added.voters.push(member("n4"));
        assert!(matches!(sim.change(two_added), Err(MembershipError::Invalid(_))));
        let mut swapped = current.clone();
        swapped.voters.retain(|m| m.id != "n1");
        swapped.voters.push(member("n3"));
        assert!(matches!(sim.change(swapped), Err(MembershipError::Invalid(_))), "a swap changes two voters");
        assert!(matches!(sim.change(current.clone()), Err(MembershipError::Invalid(_))), "no change");
        assert!(matches!(sim.change(Membership::default()), Err(MembershipError::Invalid(_))), "no voters");
        let mut twice = current.clone();
        twice.learners.push(member("n3"));
        twice.voters.push(member("n3"));
        assert!(matches!(sim.change(twice), Err(MembershipError::Invalid(_))), "in both lists");

        // A second change waits for the first to commit.
        let mut first = current.clone();
        first.learners.push(member("n3"));
        sim.change(first.clone()).unwrap();
        let mut second = first.clone();
        second.learners.push(member("n4"));
        assert_eq!(sim.change(second.clone()), Err(MembershipError::Pending));
        sim.run(40);
        assert!(sim.change(second).is_ok());

        // Followers do not take changes.
        let follower = sim.names.iter().find(|n| Some((*n).clone()) != sim.leader()).unwrap().clone();
        let result = sim.nodes.get_mut(&follower).unwrap().raft.as_mut().unwrap().propose_membership(current);
        assert!(matches!(result, Err(MembershipError::NotLeader(_))));
    }

    #[test]
    fn a_new_leader_waits_for_an_entry_of_its_own_term() {
        let mut sim = Sim::with_pool(3, 4, 16, 0);
        sim.run(100);
        let old = sim.leader().unwrap();
        sim.crash(&old);
        // Run until someone else is elected; right then, its no-op has not committed.
        let mut refused = false;
        for _ in 0..400 {
            sim.step_time();
            if let Some(leader) = sim.leader().filter(|l| *l != old) {
                let raft = sim.raft(&leader);
                if raft.commit_index() < raft.last_index() {
                    let mut members = raft.members().clone();
                    members.learners.push(member("n3"));
                    let result = sim.nodes.get_mut(&leader).unwrap().raft.as_mut().unwrap().propose_membership(members);
                    assert_eq!(result, Err(MembershipError::NotReady));
                    refused = true;
                }
                break;
            }
        }
        assert!(refused, "the new leader committed its no-op before the test could look");
    }

    #[test]
    fn a_removed_node_that_was_never_told_cannot_disrupt() {
        let mut sim = Sim::with_pool(3, 3, 17, 0);
        sim.run(100);
        let leader = sim.leader().unwrap();
        let gone = sim.names.iter().find(|n| **n != leader).unwrap().clone();
        let rest: Vec<NodeId> = sim.names.iter().filter(|n| **n != gone).cloned().collect();
        // Cut off, it cannot hear that it was removed.
        sim.partition(&[vec![gone.clone()], rest]);
        sim.step_time();
        let mut members = sim.members();
        members.voters.retain(|m| m.id != gone);
        sim.change(members).unwrap();
        sim.run(100);
        let term = sim.raft(&leader).term();
        sim.run(400);
        assert!(sim.raft(&gone).is_voter(), "the cut-off node still believes it is a voter");
        sim.heal();
        sim.run(400);
        // It tried, but pre-votes were refused and its log is behind: the
        // cluster did not notice.
        assert_eq!(sim.raft(&leader).role(), Role::Leader);
        assert_eq!(sim.raft(&leader).term(), term);
        assert!(sim.propose("still-works"));
        sim.run(60);
        assert!(sim.committed.values().any(|(_, c)| c == "still-works"));
    }

    #[test]
    fn membership_survives_snapshots_and_restarts() {
        let mut sim = Sim::with_pool(3, 4, 18, 0);
        sim.run(100);
        sim.add_learner("n3");
        sim.promote("n3");
        for i in 0..60 {
            sim.propose(&format!("c{i}"));
            sim.step_time();
        }
        sim.run(60);
        // Every node has compacted past the membership changes.
        for name in sim.names.clone() {
            assert!(sim.raft(&name).snapshot_position().0 > 5, "{name}");
        }
        for name in sim.names.clone() {
            sim.crash(&name);
        }
        for name in sim.names.clone() {
            sim.start(&name);
        }
        sim.run(200);
        for name in sim.names.clone() {
            assert_eq!(sim.raft(&name).members().voters.len(), 4, "{name} forgot the membership");
        }
        assert!(sim.propose("after-restart"));
        sim.run(60);
        assert!(sim.committed.values().any(|(_, c)| c == "after-restart"));
    }

    /// Nodes are added, promoted and removed while others crash, the network
    /// splits and messages are lost; safety is checked at every step, and
    /// once things calm down the current members must agree.
    /// `RAFT_SEEDS=1000` runs longer.
    #[test]
    fn safety_holds_while_the_membership_changes() {
        let seeds: u64 = std::env::var("RAFT_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
        for seed in 0..seeds {
            let pool = 6;
            let mut sim = Sim::with_pool(3, pool, 500 + seed, 8);
            let mut changes = 0;
            for round in 0..2500 {
                match sim.rand.next(100) {
                    0..=1 => {
                        let name = sim.names[sim.rand.next(pool as u64) as usize].clone();
                        sim.crash(&name);
                    }
                    2..=5 => {
                        let name = sim.names[sim.rand.next(pool as u64) as usize].clone();
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
                        let split = 1 + sim.rand.next(pool as u64 - 1) as usize;
                        sim.partition(&[shuffled[..split].to_vec(), shuffled[split..].to_vec()]);
                    }
                    7..=9 => sim.heal(),
                    10..=14 => {
                        // Add, promote or remove somebody.
                        let Some(leader) = sim.leader() else { continue };
                        let current = sim.raft(&leader).members().clone();
                        let mut new = current.clone();
                        let choice = sim.rand.next(3);
                        let outside: Vec<NodeId> = sim.names.iter().filter(|n| !current.contains(n)).cloned().collect();
                        if choice == 0 && !outside.is_empty() {
                            let pick = &outside[sim.rand.next(outside.len() as u64) as usize];
                            new.learners.push(member(pick));
                        } else if choice == 1 && !current.learners.is_empty() {
                            let pick = current.learners[sim.rand.next(current.learners.len() as u64) as usize].clone();
                            new.learners.retain(|m| m.id != pick.id);
                            new.voters.push(pick);
                        } else {
                            let all: Vec<&Member> = current.all().collect();
                            let pick = all[sim.rand.next(all.len() as u64) as usize].id.clone();
                            new.voters.retain(|m| m.id != pick);
                            new.learners.retain(|m| m.id != pick);
                        }
                        if sim.change(new).is_ok() {
                            changes += 1;
                        }
                    }
                    15..=40 => {
                        sim.propose(&format!("s{seed}-{round}"));
                    }
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
            sim.run(600);
            assert!(sim.propose("final"), "seed {seed}: no leader after calming down");
            sim.run(300);
            let leader = sim.leader().unwrap();
            let members = sim.raft(&leader).members().clone();
            let reference = sim.nodes[&leader].applied.clone();
            assert!(reference.contains(&"final".to_owned()), "seed {seed}");
            for m in members.all() {
                assert_eq!(sim.nodes[&m.id].applied, reference, "seed {seed}: {} disagrees", m.id);
            }
            assert!(changes > 0, "seed {seed} never changed the membership");
        }
    }

    #[test]
    fn a_single_node_cluster_commits_alone() {
        let mut raft =
            Raft::new("a".into(), Config::default(), HardState::default(), (0, 0), Membership::of_voters(vec![member("a")]), vec![], 1);
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
        let both = Membership::of_voters(vec![member("a"), member("b")]);
        let mut raft = Raft::new("a".into(), Config::default(), HardState::default(), (0, 0), both, vec![], 1);
        assert_eq!(raft.propose("x".into()), Err(ProposeError::NotLeader(None)));
        raft.step(&"b".into(), Message::Append { term: 1, prev_index: 0, prev_term: 0, entries: vec![], commit: 0 });
        assert_eq!(raft.propose("x".into()), Err(ProposeError::NotLeader(Some("b".into()))));
    }
}
