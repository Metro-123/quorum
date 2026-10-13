//! Raft wire messages and log entries.

use super::{Index, NodeId, Term};

/// A replicated log entry. `data` is an opaque command (for quorum: an
/// encoded SQL statement).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub term: Term,
    pub index: Index,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    RequestVote {
        from: NodeId,
        to: NodeId,
        term: Term,
        last_log_index: Index,
        last_log_term: Term,
    },
    RequestVoteResp {
        from: NodeId,
        to: NodeId,
        term: Term,
        granted: bool,
    },
    AppendEntries {
        from: NodeId,
        to: NodeId,
        term: Term,
        prev_log_index: Index,
        prev_log_term: Term,
        entries: Vec<Entry>,
        leader_commit: Index,
    },
    AppendEntriesResp {
        from: NodeId,
        to: NodeId,
        term: Term,
        success: bool,
        /// Highest index known to be replicated on the responder.
        match_index: Index,
    },
    /// Placeholder for the snapshot path (phase 2).
    InstallSnapshot {
        from: NodeId,
        to: NodeId,
        term: Term,
        last_index: Index,
        last_term: Term,
        data: Vec<u8>,
    },
    InstallSnapshotResp {
        from: NodeId,
        to: NodeId,
        term: Term,
        success: bool,
    },
}

impl Message {
    pub fn term(&self) -> Term {
        match self {
            Message::RequestVote { term, .. } => *term,
            Message::RequestVoteResp { term, .. } => *term,
            Message::AppendEntries { term, .. } => *term,
            Message::AppendEntriesResp { term, .. } => *term,
            Message::InstallSnapshot { term, .. } => *term,
            Message::InstallSnapshotResp { term, .. } => *term,
        }
    }

    pub fn from(&self) -> NodeId {
        match self {
            Message::RequestVote { from, .. } => *from,
            Message::RequestVoteResp { from, .. } => *from,
            Message::AppendEntries { from, .. } => *from,
            Message::AppendEntriesResp { from, .. } => *from,
            Message::InstallSnapshot { from, .. } => *from,
            Message::InstallSnapshotResp { from, .. } => *from,
        }
    }
}
