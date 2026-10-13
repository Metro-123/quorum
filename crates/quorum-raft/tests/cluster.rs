//! Deterministic cluster simulation: drive several RaftCores through an
//! in-memory network with a virtual clock.

use quorum_raft::{Message, RaftConfig, RaftCore, Role};
use std::collections::HashMap;

struct Cluster {
    nodes: HashMap<u32, RaftCore>,
    pending: Vec<(u32, Message)>,
}

impl Cluster {
    fn new(ids: &[u32]) -> Cluster {
        let mut nodes = HashMap::new();
        for &id in ids {
            let peers: Vec<u32> = ids.iter().copied().filter(|x| *x != id).collect();
            nodes.insert(id, RaftCore::new(RaftConfig::new(id, peers)));
        }
        Cluster {
            nodes,
            pending: Vec::new(),
        }
    }

    fn step(&mut self, now: u64) {
        // ticks
        let mut outs = Vec::new();
        for node in self.nodes.values_mut() {
            for out in node.tick(now) {
                outs.push((out.to, out.message));
            }
        }
        self.pending.extend(outs);

        // deliver everything currently queued
        let batch = std::mem::take(&mut self.pending);
        let mut generated = Vec::new();
        for (to, msg) in batch {
            if let Some(node) = self.nodes.get_mut(&to) {
                for out in node.handle(msg, now) {
                    generated.push((out.to, out.message));
                }
            }
        }
        self.pending.extend(generated);
    }

    fn leaders(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .nodes
            .values()
            .filter(|n| n.is_leader())
            .map(|n| n.id())
            .collect();
        v.sort_unstable();
        v
    }
}

#[test]
fn single_node_becomes_leader_and_commits() {
    let mut n = RaftCore::new(RaftConfig::new(1, vec![]));
    let _ = n.tick(1000);
    assert_eq!(n.role(), Role::Leader);
    let idx = n.propose(b"cmd".to_vec()).unwrap();
    assert_eq!(idx, 1);
    assert_eq!(n.commit_index(), 1);
    assert_eq!(n.take_applied(), vec![b"cmd".to_vec()]);
}

#[test]
fn followers_cannot_propose() {
    let mut n = RaftCore::new(RaftConfig::new(2, vec![1, 3]));
    assert!(n.propose(b"x".to_vec()).is_err());
}

#[test]
fn cluster_elects_exactly_one_leader() {
    let mut c = Cluster::new(&[1, 2, 3]);
    for t in 0..10_000u64 {
        c.step(t);
    }
    let leaders = c.leaders();
    assert_eq!(leaders.len(), 1, "expected one leader, got {:?}", leaders);
}

#[test]
fn committed_entry_replicates_to_all_nodes() {
    let mut c = Cluster::new(&[1, 2, 3]);
    for t in 0..10_000u64 {
        c.step(t);
    }
    let leader = c.leaders()[0];

    let idx = c
        .nodes
        .get_mut(&leader)
        .unwrap()
        .propose(b"hello".to_vec())
        .unwrap();

    for t in 10_000..12_000u64 {
        c.step(t);
    }

    for id in [1u32, 2, 3] {
        let node = c.nodes.get(&id).unwrap();
        assert!(
            node.commit_index() >= idx,
            "node {} commit_index {} < {}",
            id,
            node.commit_index(),
            idx
        );
    }
    let applied = c.nodes.get_mut(&leader).unwrap().take_applied();
    assert!(applied.contains(&b"hello".to_vec()));
}

#[test]
fn higher_term_causes_step_down() {
    let mut n = RaftCore::new(RaftConfig::new(1, vec![]));
    let _ = n.tick(1000);
    assert!(n.is_leader());

    let _ = n.handle(
        Message::RequestVote {
            from: 2,
            to: 1,
            term: 99,
            last_log_index: 0,
            last_log_term: 0,
        },
        1001,
    );
    assert_eq!(n.role(), Role::Follower);
    assert_eq!(n.current_term(), 99);
}

#[test]
fn stale_leader_is_rejected_by_follower() {
    let mut n = RaftCore::new(RaftConfig::new(1, vec![2]));
    // advance our term by granting a vote at term 5
    let _ = n.handle(
        Message::RequestVote {
            from: 2,
            to: 1,
            term: 5,
            last_log_index: 0,
            last_log_term: 0,
        },
        100,
    );
    assert_eq!(n.current_term(), 5);

    // an AppendEntries from a stale leader (term 3) must be rejected
    let outs = n.handle(
        Message::AppendEntries {
            from: 2,
            to: 1,
            term: 3,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        },
        200,
    );
    match &outs[0].message {
        Message::AppendEntriesResp { success, term, .. } => {
            assert!(!success);
            assert_eq!(*term, 5);
        }
        other => panic!("unexpected reply: {:?}", other),
    }
}
