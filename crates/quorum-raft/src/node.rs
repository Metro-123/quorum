//! The Raft state machine.
//!
//! This is deliberately a *pure* state machine: it performs no I/O and reads
//! no wall clock. The driver supplies the current time to [`RaftCore::tick`]
//! and delivers [`Message`]s to [`RaftCore::handle`]; both return the
//! messages that should be sent. That makes elections, partitions, and
//! message loss fully deterministic and unit-testable.

use crate::message::{Entry, Message};
use quorum_types::{DbError, Result};
use std::collections::{HashMap, HashSet};

pub type NodeId = u32;
pub type Term = u64;
pub type Index = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Debug, Clone)]
pub struct RaftConfig {
    pub id: NodeId,
    pub peers: Vec<NodeId>,
    pub election_timeout_min_ms: u64,
    pub election_timeout_max_ms: u64,
    pub heartbeat_ms: u64,
}

impl RaftConfig {
    pub fn new(id: NodeId, peers: Vec<NodeId>) -> Self {
        RaftConfig {
            id,
            peers,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 300,
            heartbeat_ms: 50,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outbound {
    pub to: NodeId,
    pub message: Message,
}

pub struct RaftCore {
    id: NodeId,
    cfg: RaftConfig,
    role: Role,
    current_term: Term,
    voted_for: Option<NodeId>,
    /// Entries at index 1..=len (index i stored at `log[i-1]`).
    log: Vec<Entry>,
    commit_index: Index,
    last_applied: Index,
    /// Commands committed since the last `take_applied`.
    applied_log: Vec<Vec<u8>>,

    // leader bookkeeping
    next_index: HashMap<NodeId, Index>,
    match_index: HashMap<NodeId, Index>,
    votes: HashSet<NodeId>,

    // timers (caller-supplied milliseconds)
    election_deadline: u64,
    last_heartbeat: u64,

    rng: u64,
}

impl RaftCore {
    pub fn new(cfg: RaftConfig) -> RaftCore {
        let id = cfg.id;
        let mut core = RaftCore {
            id,
            rng: 0x9E3779B97F4A7C15 ^ ((id as u64 + 1).wrapping_mul(0xBF58476D1CE4E5B9)),
            cfg,
            role: Role::Follower,
            current_term: 0,
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            applied_log: Vec::new(),
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            votes: HashSet::new(),
            election_deadline: 0,
            last_heartbeat: 0,
        };
        core.reset_election_deadline(0);
        core
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn config(&self) -> &RaftConfig {
        &self.cfg
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn current_term(&self) -> Term {
        self.current_term
    }

    pub fn commit_index(&self) -> Index {
        self.commit_index
    }

    pub fn last_index(&self) -> Index {
        self.log.len() as Index
    }

    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }

    fn total_nodes(&self) -> usize {
        self.cfg.peers.len() + 1
    }

    fn majority(&self) -> usize {
        self.total_nodes() / 2 + 1
    }

    fn entry_at(&self, index: Index) -> Option<&Entry> {
        if index == 0 {
            return None;
        }
        self.log.get((index - 1) as usize)
    }

    fn last_term(&self) -> Term {
        self.log.last().map(|e| e.term).unwrap_or(0)
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn reset_election_deadline(&mut self, now_ms: u64) {
        let span = self
            .cfg
            .election_timeout_max_ms
            .saturating_sub(self.cfg.election_timeout_min_ms);
        let extra = if span == 0 { 0 } else { self.rand() % (span + 1) };
        self.election_deadline = now_ms + self.cfg.election_timeout_min_ms + extra;
    }

    fn step_down(&mut self, term: Term) {
        self.role = Role::Follower;
        self.current_term = term;
        self.voted_for = None;
    }

    /// Advance the caller-supplied clock; may start an election or send
    /// heartbeats.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Outbound> {
        match self.role {
            Role::Leader => {
                if now_ms.saturating_sub(self.last_heartbeat) >= self.cfg.heartbeat_ms {
                    self.last_heartbeat = now_ms;
                    self.broadcast_append_entries()
                } else {
                    Vec::new()
                }
            }
            Role::Follower | Role::Candidate => {
                if now_ms >= self.election_deadline {
                    self.start_election(now_ms)
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn start_election(&mut self, now_ms: u64) -> Vec<Outbound> {
        self.role = Role::Candidate;
        self.current_term += 1;
        self.voted_for = Some(self.id);
        self.votes.clear();
        self.votes.insert(self.id);
        self.reset_election_deadline(now_ms);

        if self.votes.len() >= self.majority() {
            return self.become_leader();
        }
        self.cfg
            .peers
            .iter()
            .map(|&peer| Outbound {
                to: peer,
                message: Message::RequestVote {
                    from: self.id,
                    to: peer,
                    term: self.current_term,
                    last_log_index: self.last_index(),
                    last_log_term: self.last_term(),
                },
            })
            .collect()
    }

    fn become_leader(&mut self) -> Vec<Outbound> {
        self.role = Role::Leader;
        self.next_index.clear();
        self.match_index.clear();
        let last = self.last_index();
        for &peer in &self.cfg.peers {
            self.next_index.insert(peer, last + 1);
            self.match_index.insert(peer, 0);
        }
        self.match_index.insert(self.id, last);
        self.last_heartbeat = 0;
        // A single-node cluster can commit immediately.
        if self.cfg.peers.is_empty() {
            self.commit_index = last;
            self.apply_committed();
        }
        self.broadcast_append_entries()
    }

    fn broadcast_append_entries(&mut self) -> Vec<Outbound> {
        let peers = self.cfg.peers.clone();
        peers
            .into_iter()
            .map(|p| self.append_entries_to(p))
            .collect()
    }

    fn append_entries_to(&self, to: NodeId) -> Outbound {
        let next = *self.next_index.get(&to).unwrap_or(&(self.last_index() + 1));
        let next = next.max(1);
        let prev_index = next - 1;
        let prev_term = self.entry_at(prev_index).map(|e| e.term).unwrap_or(0);
        let entries: Vec<Entry> = (next..=self.last_index())
            .filter_map(|i| self.entry_at(i).cloned())
            .collect();
        Outbound {
            to,
            message: Message::AppendEntries {
                from: self.id,
                to,
                term: self.current_term,
                prev_log_index: prev_index,
                prev_log_term: prev_term,
                entries,
                leader_commit: self.commit_index,
            },
        }
    }

    /// Handle an inbound message; returns messages to send.
    pub fn handle(&mut self, msg: Message, now_ms: u64) -> Vec<Outbound> {
        if msg.term() > self.current_term {
            self.step_down(msg.term());
        }
        match msg {
            Message::RequestVote {
                from,
                term,
                last_log_index,
                last_log_term,
                ..
            } => self.handle_request_vote(from, term, last_log_index, last_log_term, now_ms),
            Message::RequestVoteResp {
                from, term, granted, ..
            } => self.handle_request_vote_resp(from, term, granted),
            Message::AppendEntries {
                from,
                term,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                ..
            } => self.handle_append_entries(
                from,
                term,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                now_ms,
            ),
            Message::AppendEntriesResp {
                from,
                term,
                success,
                match_index,
                ..
            } => self.handle_append_entries_resp(from, term, success, match_index),
            Message::InstallSnapshot { from, term, .. } => vec![Outbound {
                to: from,
                message: Message::InstallSnapshotResp {
                    from: self.id,
                    to: from,
                    term: self.current_term.max(term),
                    success: false,
                },
            }],
            Message::InstallSnapshotResp { .. } => Vec::new(),
        }
    }

    fn handle_request_vote(
        &mut self,
        from: NodeId,
        term: Term,
        last_log_index: Index,
        last_log_term: Term,
        now_ms: u64,
    ) -> Vec<Outbound> {
        let mut granted = false;
        if term >= self.current_term {
            let up_to_date = last_log_term > self.last_term()
                || (last_log_term == self.last_term() && last_log_index >= self.last_index());
            let can_vote = self.voted_for.is_none() || self.voted_for == Some(from);
            if can_vote && up_to_date {
                granted = true;
                self.voted_for = Some(from);
                self.reset_election_deadline(now_ms);
            }
        }
        vec![Outbound {
            to: from,
            message: Message::RequestVoteResp {
                from: self.id,
                to: from,
                term: self.current_term,
                granted,
            },
        }]
    }

    fn handle_request_vote_resp(&mut self, from: NodeId, term: Term, granted: bool) -> Vec<Outbound> {
        if self.role != Role::Candidate || term != self.current_term || !granted {
            return Vec::new();
        }
        self.votes.insert(from);
        if self.votes.len() >= self.majority() {
            return self.become_leader();
        }
        Vec::new()
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_append_entries(
        &mut self,
        from: NodeId,
        term: Term,
        prev_log_index: Index,
        prev_log_term: Term,
        entries: Vec<Entry>,
        leader_commit: Index,
        now_ms: u64,
    ) -> Vec<Outbound> {
        if term < self.current_term {
            return vec![self.append_resp(from, false)];
        }
        // Valid leader contact: be a follower and refresh the election timer.
        self.role = Role::Follower;
        self.reset_election_deadline(now_ms);

        if prev_log_index > self.last_index() {
            return vec![self.append_resp(from, false)];
        }
        if prev_log_index > 0 {
            match self.entry_at(prev_log_index) {
                Some(e) if e.term == prev_log_term => {}
                _ => return vec![self.append_resp(from, false)],
            }
        }

        for entry in entries {
            match self.entry_at(entry.index) {
                Some(existing) if existing.term == entry.term => {}
                Some(_) => {
                    self.log.truncate((entry.index - 1) as usize);
                    self.log.push(entry);
                }
                None => self.log.push(entry),
            }
        }

        if leader_commit > self.commit_index {
            self.commit_index = leader_commit.min(self.last_index());
            self.apply_committed();
        }

        vec![self.append_resp(from, true)]
    }

    fn append_resp(&self, to: NodeId, success: bool) -> Outbound {
        Outbound {
            to,
            message: Message::AppendEntriesResp {
                from: self.id,
                to,
                term: self.current_term,
                success,
                match_index: self.last_index(),
            },
        }
    }

    fn handle_append_entries_resp(
        &mut self,
        from: NodeId,
        term: Term,
        success: bool,
        match_index: Index,
    ) -> Vec<Outbound> {
        if self.role != Role::Leader || term != self.current_term {
            return Vec::new();
        }
        if success {
            let entry = self.match_index.entry(from).or_insert(0);
            *entry = (*entry).max(match_index);
            self.next_index.insert(from, *entry + 1);
            self.advance_commit();
            Vec::new()
        } else {
            let last_idx = self.last_index();
            let next = self
                .next_index
                .entry(from)
                .or_insert(last_idx + 1);
            *next = (*next).saturating_sub(1).max(1);
            vec![self.append_entries_to(from)]
        }
    }

    fn advance_commit(&mut self) {
        let majority = self.majority();
        let mut n = self.last_index();
        while n > self.commit_index {
            let replicated = 1
                + self
                    .cfg
                    .peers
                    .iter()
                    .filter(|p| self.match_index.get(p).copied().unwrap_or(0) >= n)
                    .count();
            if replicated >= majority {
                if self.entry_at(n).map(|e| e.term) == Some(self.current_term) {
                    self.commit_index = n;
                    self.apply_committed();
                }
                return;
            }
            n -= 1;
        }
    }

    fn apply_committed(&mut self) {
        while self.last_applied < self.commit_index {
            self.last_applied += 1;
            if let Some(e) = self.entry_at(self.last_applied) {
                self.applied_log.push(e.data.clone());
            }
        }
    }

    /// Drain the commands committed since the last call.
    pub fn take_applied(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.applied_log)
    }

    /// Propose a new command to the replicated log. Only valid on the leader.
    pub fn propose(&mut self, data: Vec<u8>) -> Result<Index> {
        if self.role != Role::Leader {
            return Err(DbError::Conflict("not the leader".into()));
        }
        let index = self.last_index() + 1;
        self.log.push(Entry {
            term: self.current_term,
            index,
            data,
        });
        self.match_index.insert(self.id, index);
        if self.cfg.peers.is_empty() {
            self.commit_index = index;
            self.apply_committed();
        }
        Ok(index)
    }
}
