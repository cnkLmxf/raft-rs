//! The raw node of the raft module.
//!
//! This module contains the value types for the node and it's connection to other
//! nodes but not the raft consensus itself. Generally, you'll interact with the
//! RawNode first and use it to access the inner workings of the consensus protocol.
// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

// Copyright 2015 The etcd Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::mem;

use eraftpb::{
    ConfChange, ConfChangeType, ConfState, Entry, EntryType, HardState, Message, MessageType,
    Snapshot,
};
use protobuf::{self, RepeatedField};

use super::config::Config;
use super::errors::{Error, Result};
use super::read_only::ReadState;
use super::Status;
use super::Storage;
use super::{Raft, SoftState, INVALID_ID};

/// Represents a Peer node in the cluster.
///表示集群中的对等节点。
#[derive(Debug, Default)]
pub struct Peer {
    /// The ID of the peer.
    ///对等体的ID。
    pub id: u64,
    /// If there is context associated with the peer (like connection information), it can be
    /// serialized and stored here.
    ///如果存在与对等方关联的上下文（如连接信息），则可以将其序列化并存储在此处。
    pub context: Option<Vec<u8>>,
}

/// The status of the snapshot.
///快照的状态。
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum SnapshotStatus {
    /// Represents that the snapshot is finished being created.
    ///表示快照创建完成。
    Finish,
    /// Indicates that the snapshot failed to build or is not ready.
    ///表示快照构建失败或未准备好。
    Failure,
}

fn is_local_msg(t: MessageType) -> bool {
    match t {
        MessageType::MsgHup
        | MessageType::MsgBeat
        | MessageType::MsgUnreachable
        | MessageType::MsgSnapStatus
        | MessageType::MsgCheckQuorum => true,
        _ => false,
    }
}

fn is_response_msg(t: MessageType) -> bool {
    match t {
        MessageType::MsgAppendResponse
        | MessageType::MsgRequestVoteResponse
        | MessageType::MsgHeartbeatResponse
        | MessageType::MsgUnreachable
        | MessageType::MsgRequestPreVoteResponse => true,
        _ => false,
    }
}

/// For a given snapshot, determine if it's empty or not.
///对于给定的快照，确定它是否为空。
pub fn is_empty_snap(s: &Snapshot) -> bool {
    s.get_metadata().get_index() == 0
}

/// Ready encapsulates the entries and messages that are ready to read,
/// be saved to stable storage, committed or sent to other peers.
/// All fields in Ready are read-only.
///Ready封装了可读取，已保存到稳定存储，已提交或发送给其他对等方的条目和消息。 “就绪”中的所有字段均为只读。
#[derive(Default, Debug, PartialEq)]
pub struct Ready {
    ss: Option<SoftState>,

    hs: Option<HardState>,

    read_states: Vec<ReadState>,

    entries: Vec<Entry>,

    snapshot: Snapshot,

    /// CommittedEntries specifies entries to be committed to a
    /// store/state-machine. These have previously been committed to stable
    /// store.
    /// CommittedEntries指定要提交到商店/状态机的条目。 这些以前已致力于稳定存储。
    pub committed_entries: Option<Vec<Entry>>,

    /// Messages specifies outbound messages to be sent AFTER Entries are
    /// committed to stable storage.
    /// If it contains a MsgSnap message, the application MUST report back to raft
    /// when the snapshot has been received or has failed by calling ReportSnapshot.
    ///消息指定将条目提交到稳定存储后要发送的出站消息。
    ///如果包含MsgSnap消息，则当收到快照或通过调用ReportSnapshot失败快照时，应用程序务必向筏报告。
    pub messages: Vec<Message>,

    must_sync: bool,
}

impl Ready {
    fn new<T: Storage>(
        raft: &mut Raft<T>,
        prev_ss: &SoftState,
        prev_hs: &HardState,
        since_idx: Option<u64>,
    ) -> Ready {
        let mut rd = Ready {
            entries: raft.raft_log.unstable_entries().unwrap_or(&[]).to_vec(),
            ..Default::default()
        };
        if !raft.msgs.is_empty() {
            mem::swap(&mut raft.msgs, &mut rd.messages);
        }
        rd.committed_entries = Some(
            (match since_idx {
                None => raft.raft_log.next_entries(),
                Some(idx) => raft.raft_log.next_entries_since(idx),
            })
            .unwrap_or_else(Vec::new),
        );
        let ss = raft.soft_state();
        if &ss != prev_ss {
            rd.ss = Some(ss);
        }
        let hs = raft.hard_state();
        if &hs != prev_hs {
            if hs.get_vote() != prev_hs.get_vote() || hs.get_term() != prev_hs.get_term() {
                rd.must_sync = true;
            }
            rd.hs = Some(hs);
        }
        if raft.raft_log.get_unstable().snapshot.is_some() {
            rd.snapshot = raft.raft_log.get_unstable().snapshot.clone().unwrap();
        }
        if !raft.read_states.is_empty() {
            rd.read_states = raft.read_states.clone();
        }
        rd
    }

    /// The current volatile state of a Node.
    /// SoftState will be nil if there is no update.
    /// It is not required to consume or store SoftState.
    ///节点的当前易失性状态。
     ///如果没有更新，SoftState将为nil。
     ///不需要使用或存储SoftState。
    #[inline]
    pub fn ss(&self) -> Option<&SoftState> {
        self.ss.as_ref()
    }

    /// The current state of a Node to be saved to stable storage BEFORE
    /// Messages are sent.
    /// HardState will be equal to empty state if there is no update.
    ///发送消息之前，要保存到稳定存储的节点的当前状态。
     ///如果没有更新，则HardState将等于空状态。
    #[inline]
    pub fn hs(&self) -> Option<&HardState> {
        self.hs.as_ref()
    }

    /// States can be used for node to serve linearizable read requests locally
    /// when its applied index is greater than the index in ReadState.
    /// Note that the read_state will be returned when raft receives MsgReadIndex.
    /// The returned is only valid for the request that requested to read.
    ///当节点的应用索引大于ReadState中的索引时，可以使用状态来使节点本地处理线性化的读取请求。
     ///请注意，当raft收到MsgReadIndex时，将返回read_state。
     ///返回的内容仅对请求读取的请求有效。
    #[inline]
    pub fn read_states(&self) -> &[ReadState] {
        &self.read_states
    }

    /// Entries specifies entries to be saved to stable storage BEFORE
    /// Messages are sent.
    ///条目指定发送消息之前要保存到稳定存储的条目。
    #[inline]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Snapshot specifies the snapshot to be saved to stable storage.
    /// Snapshot指定要保存到稳定存储的快照。
    #[inline]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// MustSync indicates whether the HardState and Entries must be synchronously
    /// written to disk or if an asynchronous write is permissible.
    /// MustSync指示是否必须将HardState和Entries同步写入磁盘，或者是否允许异步写入。
    #[inline]
    pub fn must_sync(&self) -> bool {
        self.must_sync
    }
}

/// RawNode is a thread-unsafe Node.
/// The methods of this struct correspond to the methods of Node and are described
/// more fully there.
/// RawNode是线程不安全的节点。
///此结构的方法与Node的方法相对应，并在此处进行了更全面的描述。
pub struct RawNode<T: Storage> {
    /// The internal raft state.
    ///内部raft状态。
    pub raft: Raft<T>,
    prev_ss: SoftState,
    prev_hs: HardState,
}

impl<T: Storage> RawNode<T> {
    #[allow(clippy::new_ret_no_self)]
    /// Create a new RawNode given some [`Config`](../struct.Config.html) and a list of [`Peer`](raw_node/struct.Peer.html)s.
    ///给定一些[`Config`]（../ struct.Config.html）和[`Peer`]（raw_node / struct.Peer.html）列表，创建一个新的RawNode。
    pub fn new(config: &Config, store: T, mut peers: Vec<Peer>) -> Result<RawNode<T>> {
        assert_ne!(config.id, 0, "config.id must not be zero");
        let r = Raft::new(config, store)?;
        let mut rn = RawNode {
            raft: r,
            prev_hs: Default::default(),
            prev_ss: Default::default(),
        };
        let last_index = rn.raft.get_store().last_index().expect("");
        if last_index == 0 {
            rn.raft.become_follower(1, INVALID_ID);
            let mut ents = Vec::with_capacity(peers.len());
            for (i, peer) in peers.iter_mut().enumerate() {
                let mut cc = ConfChange::new();
                cc.set_change_type(ConfChangeType::AddNode);
                cc.set_node_id(peer.id);
                if let Some(ctx) = peer.context.take() {
                    cc.set_context(ctx);
                }
                let data =
                    protobuf::Message::write_to_bytes(&cc).expect("unexpected marshal error");
                let mut e = Entry::new();
                e.set_entry_type(EntryType::EntryConfChange);
                e.set_term(1);
                e.set_index(i as u64 + 1);
                e.set_data(data);
                ents.push(e);
            }
            rn.raft.raft_log.append(&ents);
            rn.raft.raft_log.committed = ents.len() as u64;
            for peer in peers {
                rn.raft.add_node(peer.id)?;
            }
        }
        rn.prev_ss = rn.raft.soft_state();
        if last_index == 0 {
            rn.prev_hs = Default::default();
        } else {
            rn.prev_hs = rn.raft.hard_state();
        }
        Ok(rn)
    }

    fn commit_ready(&mut self, rd: Ready) {
        if rd.ss.is_some() {
            self.prev_ss = rd.ss.unwrap();
        }
        if let Some(e) = rd.hs {
            if e != HardState::new() {
                self.prev_hs = e;
            }
        }
        if !rd.entries.is_empty() {
            let e = rd.entries.last().unwrap();
            //持久化unstable中的entry数据
            self.raft.raft_log.stable_to(e.get_index(), e.get_term());
        }
        if rd.snapshot != Snapshot::new() {
            //持久化unstable中的snapshot数据
            self.raft
                .raft_log
                .stable_snap_to(rd.snapshot.get_metadata().get_index());
        }
        if !rd.read_states.is_empty() {
            self.raft.read_states.clear();
        }
    }

    fn commit_apply(&mut self, applied: u64) {
        self.raft.commit_apply(applied);
    }

    /// Tick advances the internal logical clock by a single tick.
    ///
    /// Returns true to indicate that there will probably be some readiness which
    /// needs to be handled.
    ///滴答使内部逻辑时钟前进一个滴答。
    ///返回true，表示可能需要处理一些准备情况。
    pub fn tick(&mut self) -> bool {
        self.raft.tick()
    }

    /// Campaign causes this RawNode to transition to candidate state.
    /// Campaign使该RawNode转换为候选状态。
    pub fn campaign(&mut self) -> Result<()> {
        let mut m = Message::new();
        m.set_msg_type(MessageType::MsgHup);
        self.raft.step(m)
    }

    /// Propose proposes data be appended to the raft log.
    ///提议将数据附加到raft日志中。
    pub fn propose(&mut self, context: Vec<u8>, data: Vec<u8>) -> Result<()> {
        let mut m = Message::new();
        m.set_msg_type(MessageType::MsgPropose);
        m.set_from(self.raft.id);
        let mut e = Entry::new();
        e.set_data(data);
        e.set_context(context);
        m.set_entries(RepeatedField::from_vec(vec![e]));
        self.raft.step(m)
    }

    /// ProposeConfChange proposes a config change.
    /// ProposeConfChange提出配置更改。
    #[cfg_attr(feature = "cargo-clippy", allow(clippy::needless_pass_by_value))]
    pub fn propose_conf_change(&mut self, context: Vec<u8>, cc: ConfChange) -> Result<()> {
        let data = protobuf::Message::write_to_bytes(&cc)?;
        let mut m = Message::new();
        m.set_msg_type(MessageType::MsgPropose);
        let mut e = Entry::new();
        e.set_entry_type(EntryType::EntryConfChange);
        e.set_data(data);
        e.set_context(context);
        m.set_entries(RepeatedField::from_vec(vec![e]));
        self.raft.step(m)
    }

    /// Takes the conf change and applies it.
    ///进行conf更改并应用它。
    ///
    /// # Panics
    ///
    /// In the case of `BeginMembershipChange` or `FinalizeConfChange` returning errors this will panic.
    ///如果出现`BeginMembershipChange`或`FinalizeConfChange`返回错误，这将引起恐慌。
    ///
    /// For a safe interface for these directly call `this.raft.begin_membership_change(entry)` or
    /// `this.raft.finalize_membership_change(entry)` respectively.
    ///为了获得安全的界面，请分别直接调用“ this.raft.begin_membership_change（entry）”或“ this.raft.finalize_membership_change（entry）”。
    pub fn apply_conf_change(&mut self, cc: &ConfChange) -> Result<ConfState> {
        if cc.get_node_id() == INVALID_ID
            && cc.get_change_type() != ConfChangeType::BeginMembershipChange
        {
            let mut cs = ConfState::new();
            cs.set_nodes(self.raft.prs().voter_ids().iter().cloned().collect());
            cs.set_learners(self.raft.prs().learner_ids().iter().cloned().collect());
            return Ok(cs);
        }
        let nid = cc.get_node_id();
        match cc.get_change_type() {
            ConfChangeType::AddNode => self.raft.add_node(nid)?,
            ConfChangeType::AddLearnerNode => self.raft.add_learner(nid)?,
            ConfChangeType::RemoveNode => self.raft.remove_node(nid)?,
            ConfChangeType::BeginMembershipChange => self.raft.begin_membership_change(cc)?,
            ConfChangeType::FinalizeMembershipChange => {
                self.raft.mut_prs().finalize_membership_change()?
            }
        };

        Ok(self.raft.prs().configuration().clone().into())
    }

    /// Step advances the state machine using the given message.
    ///步骤使用给定的消息推进状态机。
    pub fn step(&mut self, m: Message) -> Result<()> {
        // ignore unexpected local messages receiving over network
        //忽略通过网络接收的意外本地消息
        if is_local_msg(m.get_msg_type()) {
            return Err(Error::StepLocalMsg);
        }
        //如果外地发送者存在于processor中，或者不是response消息，则处理该消息
        if self.raft.prs().get(m.get_from()).is_some() || !is_response_msg(m.get_msg_type()) {
            return self.raft.step(m);
        }
        Err(Error::StepPeerNotFound)
    }

    /// Given an index, creates a new Ready value from that index.
    ///给定一个索引，从该索引创建一个新的Ready值。
    pub fn ready_since(&mut self, applied_idx: u64) -> Ready {
        Ready::new(
            &mut self.raft,
            &self.prev_ss,
            &self.prev_hs,
            Some(applied_idx),
        )
    }

    /// Ready returns the current point-in-time state of this RawNode.
    /// Ready返回此RawNode的当前时间点状态。
    pub fn ready(&mut self) -> Ready {
        Ready::new(&mut self.raft, &self.prev_ss, &self.prev_hs, None)
    }

    /// Given an index, can determine if there is a ready state from that time.
    ///给定一个索引，可以确定从那时起是否有就绪状态。
    pub fn has_ready_since(&self, applied_idx: Option<u64>) -> bool {
        let raft = &self.raft;
        //待发送的msgs不为空，或者unstable_entries不为空返回true
        if !raft.msgs.is_empty() || raft.raft_log.unstable_entries().is_some() {
            return true;
        }
        //如果raft的read_states不为空返回true
        if !raft.read_states.is_empty() {
            return true;
        }
        //snapshot为空则返回true
        if self.get_snap().map_or(false, |s| !is_empty_snap(s)) {
            return true;
        }

        let has_unapplied_entries = match applied_idx {
            //如果applied_idx没有指定则通过是否存在next entries判断
            None => raft.raft_log.has_next_entries(),
            //如果指定applied_idx，则判断从applied_idx开始是否有entries
            Some(idx) => raft.raft_log.has_next_entries_since(idx),
        };
        if has_unapplied_entries {
            return true;
        }
        //soft_state发生改变返回true
        if raft.soft_state() != self.prev_ss {
            return true;
        }
        //hs不为空，并且发生了改变则返回true
        let hs = raft.hard_state();
        if hs != HardState::new() && hs != self.prev_hs {
            return true;
        }
        false
    }

    /// HasReady called when RawNode user need to check if any Ready pending.
    /// Checking logic in this method should be consistent with Ready.containsUpdates().
    ///当RawNode用户需要检查是否有任何Ready待处理时，调用HasReady。 此方法中的检查逻辑应与Ready.containsUpdates（）一致。
    #[inline]
    pub fn has_ready(&self) -> bool {
        self.has_ready_since(None)
    }

    /// Grabs the snapshot from the raft if available.
    ///从raft中抓取快照（如果有）。
    #[inline]
    pub fn get_snap(&self) -> Option<&Snapshot> {
        self.raft.get_snap()
    }

    /// Advance notifies the RawNode that the application has applied and saved progress in the
    /// last Ready results.
    /// Advance通知RawNode该应用程序已应用并在最近的Ready结果中保存了进度。
    pub fn advance(&mut self, rd: Ready) {
        //更新raft内部的状态，整个执行流程为，raft接收到请求进行一系列操作，等到条件满足了生成msg存到msgs中，
        //等到ready轮序的时候会从中拿到msgs等的消息进行处理，处理的过程为更新外部存储或者持久化的过程，
        //过程结束后更新raft的状态，即这里的执行
        self.advance_append(rd);
        let commit_idx = self.prev_hs.get_commit();
        if commit_idx != 0 {
            // In most cases, prevHardSt and rd.HardState will be the same
            // because when there are new entries to apply we just sent a
            // HardState with an updated Commit value. However, on initial
            // startup the two are different because we don't send a HardState
            // until something changes, but we do send any un-applied but
            // committed entries (and previously-committed entries may be
            // incorporated into the snapshot, even if rd.CommittedEntries is
            // empty). Therefore we mark all committed entries as applied
            // whether they were included in rd.HardState or not.
            //在大多数情况下，prevHardSt和rd.HardState将是相同的，因为当有新条目要应用时，我们只是发送了具有更新的Commit值的HardState。
          //但是，在初次启动时，两者是不同的，因为在发生某些更改之前我们不会发送HardState，而是会发送任何未应用但已提交的条目（即使rd.CommittedEntries，先前提交的条目也可以合并到快照中） 是空的）。
          //因此，无论是否将它们提交到rd.HardState中，我们都将其标记为已应用。
            self.advance_apply(commit_idx);
        }
    }

    /// Appends and commits the ready value.
    ///追加并提交就绪值。
    #[inline]
    pub fn advance_append(&mut self, rd: Ready) {
        //提交ready ,更新本地raft的状态
        self.commit_ready(rd);
    }

    /// Advance apply to the passed index.
    ///将高级应用于传递的索引。
    #[inline]
    pub fn advance_apply(&mut self, applied: u64) {
        self.commit_apply(applied);
    }

    /// Status returns the current status of the given group.
    /// Status返回给定组的当前状态。
    #[inline]
    pub fn status(&self) -> Status {
        Status::new(&self.raft)
    }

    /// ReportUnreachable reports the given node is not reachable for the last send.
    /// ReportUnreachable报告给定节点在最后一次发送时不可达。
    pub fn report_unreachable(&mut self, id: u64) {
        let mut m = Message::new();
        m.set_msg_type(MessageType::MsgUnreachable);
        m.set_from(id);
        // we don't care if it is ok actually
        //实际上，我们不在乎是否可以
        self.raft.step(m).is_ok();
    }

    /// ReportSnapshot reports the status of the sent snapshot.
    /// ReportSnapshot报告已发送快照的状态。
    pub fn report_snapshot(&mut self, id: u64, status: SnapshotStatus) {
        let rej = status == SnapshotStatus::Failure;
        let mut m = Message::new();
        m.set_msg_type(MessageType::MsgSnapStatus);
        m.set_from(id);
        m.set_reject(rej);
        // we don't care if it is ok actually
        //实际上，我们不在乎是否可以
        self.raft.step(m).is_ok();
    }

    /// TransferLeader tries to transfer leadership to the given transferee.
    /// TransferLeader尝试将领导权转移给指定的受让人。
    pub fn transfer_leader(&mut self, transferee: u64) {
        let mut m = Message::new();
        m.set_msg_type(MessageType::MsgTransferLeader);
        m.set_from(transferee);
        self.raft.step(m).is_ok();
    }

    /// ReadIndex requests a read state. The read state will be set in ready.
    /// Read State has a read index. Once the application advances further than the read
    /// index, any linearizable read requests issued before the read request can be
    /// processed safely. The read state will have the same rctx attached.
    /// ReadIndex请求读取状态。 读取状态将设置为就绪。
     ///读取状态具有读取索引。
     ///一旦应用程序比读取索引前进，可以安全地处理在读取请求之前发出的任何可线性化的读取请求。
     ///读取状态将附加相同的rctx。
    pub fn read_index(&mut self, rctx: Vec<u8>) {
        let mut m = Message::new();
        m.set_msg_type(MessageType::MsgReadIndex);
        let mut e = Entry::new();
        e.set_data(rctx);
        m.set_entries(RepeatedField::from_vec(vec![e]));
        self.raft.step(m).is_ok();
    }

    /// Returns the store as an immutable reference.
    ///返回商店作为不可变的引用。
    #[inline]
    pub fn get_store(&self) -> &T {
        self.raft.get_store()
    }

    /// Returns the store as a mutable reference.
    ///返回存储作为可变引用。
    #[inline]
    pub fn mut_store(&mut self) -> &mut T {
        self.raft.mut_store()
    }

    /// Set whether skip broadcast empty commit messages at runtime.
    ///设置是否在运行时跳过广播的空提交消息。
    #[inline]
    pub fn skip_bcast_commit(&mut self, skip: bool) {
        self.raft.skip_bcast_commit(skip)
    }
}

#[cfg(test)]
mod test {
    use super::is_local_msg;
    use eraftpb::MessageType;
    use setup_for_test;

    #[test]
    fn test_is_local_msg() {
        setup_for_test();
        let tests = vec![
            (MessageType::MsgHup, true),
            (MessageType::MsgBeat, true),
            (MessageType::MsgUnreachable, true),
            (MessageType::MsgSnapStatus, true),
            (MessageType::MsgCheckQuorum, true),
            (MessageType::MsgPropose, false),
            (MessageType::MsgAppend, false),
            (MessageType::MsgAppendResponse, false),
            (MessageType::MsgRequestVote, false),
            (MessageType::MsgRequestVoteResponse, false),
            (MessageType::MsgSnapshot, false),
            (MessageType::MsgHeartbeat, false),
            (MessageType::MsgHeartbeatResponse, false),
            (MessageType::MsgTransferLeader, false),
            (MessageType::MsgTimeoutNow, false),
            (MessageType::MsgReadIndex, false),
            (MessageType::MsgReadIndexResp, false),
            (MessageType::MsgRequestPreVote, false),
            (MessageType::MsgRequestPreVoteResponse, false),
        ];
        for (msg_type, result) in tests {
            assert_eq!(is_local_msg(msg_type), result);
        }
    }
}
