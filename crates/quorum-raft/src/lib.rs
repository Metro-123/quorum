//! A from-scratch Raft implementation: leader election, log replication, and
//! commit, as a deterministic state machine.

pub mod message;
pub mod node;

pub use message::{Entry, Message};
pub use node::{Index, NodeId, Outbound, RaftConfig, RaftCore, Role, Term};
