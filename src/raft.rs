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

use std::cmp;

use eraftpb::{
    ConfChange, ConfChangeType, Entry, EntryType, HardState, Message, MessageType, Snapshot,
};
use hashbrown::{HashMap, HashSet};
use protobuf;
use protobuf::RepeatedField;
use rand::{self, Rng};

use super::errors::{Error, Result, StorageError};
use super::progress::{CandidacyStatus, Configuration, Progress, ProgressSet, ProgressState};
use super::raft_log::{self, RaftLog};
use super::read_only::{ReadOnly, ReadOnlyOption, ReadState};
use super::storage::Storage;
use super::Config;

// CAMPAIGN_PRE_ELECTION represents the first phase of a normal election when
// Config.pre_vote is true.
// CAMPAIGN_PRE_ELECTION表示Config.pre_vote为true时正常选举的第一阶段。
const CAMPAIGN_PRE_ELECTION: &[u8] = b"CampaignPreElection";
// CAMPAIGN_ELECTION represents a normal (time-based) election (the second phase
// of the election when Config.pre_vote is true).
// CAMPAIGN_ELECTION表示常规的（基于时间的）选举（当Config.pre_vote为true时，选举的第二阶段）。
const CAMPAIGN_ELECTION: &[u8] = b"CampaignElection";
// CAMPAIGN_TRANSFER represents the type of leader transfer.
// CAMPAIGN_TRANSFER表示领导者转移的类型。
const CAMPAIGN_TRANSFER: &[u8] = b"CampaignTransfer";

/// The role of the node.
///节点的角色。
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum StateRole {
    /// The node is a follower of the leader.
    ///节点是领导者的跟随者。
    Follower,
    /// The node could become a leader.
    ///该节点可以成为领导者。
    Candidate,
    /// The node is a leader.
    ///节点是领导者。
    Leader,
    /// The node could become a candidate, if `prevote` is enabled.
    ///如果启用了“ prevote”，则该节点可以成为候选节点。
    PreCandidate,
}

impl Default for StateRole {
    fn default() -> StateRole {
        StateRole::Follower
    }
}

/// A constant represents invalid id of raft.
///常数代表raft的无效ID。
pub const INVALID_ID: u64 = 0;
/// A constant represents invalid index of raft log.
///常量表示raft日志的无效索引。
pub const INVALID_INDEX: u64 = 0;

/// SoftState provides state that is useful for logging and debugging.
/// The state is volatile and does not need to be persisted to the WAL.
/// SoftState提供了对于日志记录和调试有用的状态。 状态是易变的，不需要持久保存到WAL。
#[derive(Default, PartialEq, Debug)]
pub struct SoftState {
    /// The potential leader of the cluster.
    ///集群的潜在领导者。
    pub leader_id: u64,
    /// The soft role this node may take.
    ///该节点可能扮演的软角色。
    pub raft_state: StateRole,
}

/// A struct that represents the raft consensus itself. Stores details concerning the current
/// and possible state the system can take.
///表示raft共识本身的结构。 存储有关系统当前和可能状态的详细信息。
#[derive(Default, Getters)]
pub struct Raft<T: Storage> {
    /// The current election term.
    ///当前的选举期限。
    pub term: u64,

    /// Which peer this raft is voting for.
    ///这个raft在投票给哪个peer。
    pub vote: u64,

    /// The ID of this node.
    ///此节点的ID。
    pub id: u64,

    /// The current read states.
    ///当前的读取状态。这里存放的是可以进行读取的，满足大半响应的
    pub read_states: Vec<ReadState>,

    /// The persistent log.
    ///持久日志。
    pub raft_log: RaftLog<T>,

    /// The maximum number of messages that can be inflight.
    ///可以发送的最大消息数。
    pub max_inflight: usize,

    /// The maximum length (in bytes) of all the entries.
    ///所有条目的最大长度（以字节为单位）。
    pub max_msg_size: u64,

    prs: Option<ProgressSet>,

    /// The current role of this node.
    ///此节点的当前角色。
    pub state: StateRole,

    /// Whether this is a learner node.
    ///
    /// Learners are not permitted to vote in elections, and are not counted for commit quorums.
    /// They do replicate data from the leader.
    ///这是否是学习者节点。
    ///不允许学习者在选举中投票，也不算达到法定人数。 他们可从领导者复制数据。
    pub is_learner: bool,

    /// The current votes for this node in an election.
    ///
    /// Reset when changing role.
    ///选举中该节点的当前投票。 更改角色时重置。
    pub votes: HashMap<u64, bool>,

    /// The list of messages.
    ///消息列表。
    pub msgs: Vec<Message>,

    /// The leader id
    ///领导者ID
    pub leader_id: u64,

    /// ID of the leader transfer target when its value is not None.
    ///
    /// If this is Some(id), we follow the procedure defined in raft thesis 3.10.
    ///当领导者转移目标的值不为None时，其ID。
     ///如果这是Some（id），我们将按照raft论文3.10中定义的过程进行操作。
    pub lead_transferee: Option<u64>,

    /// Only one conf change may be pending (in the log, but not yet
    /// applied) at a time. This is enforced via `pending_conf_index`, which
    /// is set to a value >= the log index of the latest pending
    /// configuration change (if any). Config changes are only allowed to
    /// be proposed if the leader's applied index is greater than this
    /// value.
    ///
    /// This value is conservatively set in cases where there may be a configuration change pending,
    /// but scanning the log is possibly expensive. This implies that the index stated here may not
    /// necessarily be a config change entry, and it may not be a `BeginMembershipChange` entry, even if
    /// we set this to one.
    /// 一次只能有一个conf更改挂起（在日志中，但尚未applied）。
    /// 这是通过`pending_conf_index`强制执行的，将其设置为> =最新的挂起配置更改（如果有）的日志索引的值。
    ///仅当领导者的应用索引大于此值时，才可以建议更改配置。
     ///
     ///在可能有挂起的配置更改的情况下，保守地设置此值，但是扫描日志可能会很昂贵。
     ///这意味着此处声明的索引不一定是配置更改项，即使我们将其设置为一个，也不一定是“ BeginMembershipChange”项。
    pub pending_conf_index: u64,

    /// The last `BeginMembershipChange` entry. Once we make this change we exit the joint state.
    /// 最后一个“ BeginMembershipChange”条目。 做出更改后，我们将退出联合状态。
    ///
    /// This is different than `pending_conf_index` since it is more specific, and also exact.
    /// While `pending_conf_index` is conservatively set at times to ensure safety in the
    /// one-by-one change method, in joint consensus based changes we track the state exactly. The
    /// index here **must** only be set when a `BeginMembershipChange` is present at that index.
    /// 这与`pending_conf_index`不同，因为它更具体，也更精确。
    /// 虽然有时会保守地设置`pending_conf_index`以确保采用一对一更改方法的安全性，但在基于联合共识的更改中，我们会精确跟踪状态。
    /// 只有在该索引上存在“ BeginMembershipChange”时，才必须在此设置索引。
    ///
    /// # Caveats
    ///
    /// It is important that whenever this is set that `pending_conf_index` is also set to the
    /// value if it is greater than the existing value.
    /// 重要的是，无论何时设置，只要`pending_conf_index`大于现有值，它也应设置为该值。
    ///
    /// **Use `Raft::set_pending_membership_change()` to change this value.**
    /// **使用`Raft :: set_pending_membership_change（）`更改此值。**
    #[get = "pub"]
    pending_membership_change: Option<ConfChange>,

    /// The queue of read-only requests.
    ///只读请求队列。
    pub read_only: ReadOnly,

    /// Ticks since it reached last electionTimeout when it is leader or candidate.
    /// Number of ticks since it reached last electionTimeout or received a
    /// valid message from current leader when it is a follower.
    ///当它成为领导者或候选人时达到最后一次选举超时的提示。
    ///自从它达到上次选举超时或从当前领导者作为跟随者收到有效消息以来的滴答数。
    pub election_elapsed: usize,

    /// Number of ticks since it reached last heartbeatTimeout.
    /// only leader keeps heartbeatElapsed.
    ///自达到上一个heartbeatTimeout以来的滴答数。 只有领导者保持心跳过去。
    heartbeat_elapsed: usize,

    /// Whether to check the quorum
    ///是否检查法定人数
    pub check_quorum: bool,

    /// Enable the prevote algorithm.
    ///
    /// This enables a pre-election vote round on Candidates prior to disrupting the cluster.
    ///
    /// Enable this if greater cluster stability is preferred over faster elections.
    ///启用prevote算法。
    ///这样可以在破坏群集之前对候选人进行预选投票.
    ///如果优先选择更高的群集稳定性而不是更快的选举，请启用此选项。
    pub pre_vote: bool,

    skip_bcast_commit: bool,

    heartbeat_timeout: usize,
    election_timeout: usize,

    // randomized_election_timeout is a random number between
    // [min_election_timeout, max_election_timeout - 1]. It gets reset
    // when raft changes its state to follower or candidate.
    // randomized_election_timeout是介于[min_election_timeout，max_election_timeout-1]之间的随机数。
    // 当raft更改为关注者或候选者时，它将重置。
    randomized_election_timeout: usize,
    min_election_timeout: usize,
    max_election_timeout: usize,

    /// Tag is only used for logging
    ///标签仅用于记录
    tag: String,
}

trait AssertSend: Send {}

impl<T: Storage + Send> AssertSend for Raft<T> {}

fn new_message(to: u64, field_type: MessageType, from: Option<u64>) -> Message {
    let mut m = Message::new();
    m.set_to(to);
    if let Some(id) = from {
        m.set_from(id);
    }
    m.set_msg_type(field_type);
    m
}

/// Maps vote and pre_vote message types to their correspond responses.
///将vote和pre_vote消息类型映射到其相应的响应。
pub fn vote_resp_msg_type(t: MessageType) -> MessageType {
    match t {
        MessageType::MsgRequestVote => MessageType::MsgRequestVoteResponse,
        MessageType::MsgRequestPreVote => MessageType::MsgRequestPreVoteResponse,
        _ => panic!("Not a vote message: {:?}", t),
    }
}

impl<T: Storage> Raft<T> {
    /// Creates a new raft for use on the node.
    ///创建一个新的筏，供在该节点上使用。
    #[allow(clippy::new_ret_no_self)]
    pub fn new(c: &Config, store: T) -> Result<Raft<T>> {
        c.validate()?;
        let raft_state = store.initial_state()?;
        let conf_state = &raft_state.conf_state;
        let raft_log = RaftLog::new(store, c.tag.clone());
        let mut peers: &[u64] = &c.peers;
        let mut learners: &[u64] = &c.learners;
        if !conf_state.get_nodes().is_empty() || !conf_state.get_learners().is_empty() {
            if !peers.is_empty() || !learners.is_empty() {
                // TODO: the peers argument is always nil except in
                // tests; the argument should be removed and these tests should be
                // updated to specify their nodes through a snap
                // TODO：除了测试外，peers参数始终为零； 应删除参数，并应更新这些测试以通过快照指定其节点
                panic!(
                    "{} cannot specify both new(peers/learners) and ConfState.(Nodes/Learners)",
                    c.tag
                )
            }
            peers = conf_state.get_nodes();
            learners = conf_state.get_learners();
        }
        let mut r = Raft {
            id: c.id,
            read_states: Default::default(),
            raft_log,
            max_inflight: c.max_inflight_msgs,
            max_msg_size: c.max_size_per_msg,
            prs: Some(ProgressSet::with_capacity(peers.len(), learners.len())),
            state: StateRole::Follower,
            is_learner: false,
            check_quorum: c.check_quorum,
            pre_vote: c.pre_vote,
            read_only: ReadOnly::new(c.read_only_option),
            heartbeat_timeout: c.heartbeat_tick,
            election_timeout: c.election_tick,
            votes: Default::default(),
            msgs: Default::default(),
            leader_id: Default::default(),
            lead_transferee: None,
            term: Default::default(),
            election_elapsed: Default::default(),
            pending_conf_index: Default::default(),
            pending_membership_change: Default::default(),
            vote: Default::default(),
            heartbeat_elapsed: Default::default(),
            randomized_election_timeout: 0,
            min_election_timeout: c.min_election_tick(),
            max_election_timeout: c.max_election_tick(),
            skip_bcast_commit: c.skip_bcast_commit,
            tag: c.tag.to_owned(),
        };
        for p in peers {
            let pr = Progress::new(1, r.max_inflight);
            if let Err(e) = r.mut_prs().insert_voter(*p, pr) {
                panic!("{}", e);
            }
        }
        for p in learners {
            let pr = Progress::new(1, r.max_inflight);
            if let Err(e) = r.mut_prs().insert_learner(*p, pr) {
                panic!("{}", e);
            };
            if *p == r.id {
                r.is_learner = true;
            }
        }

        if raft_state.hard_state != HardState::new() {
            r.load_state(&raft_state.hard_state);
        }
        if c.applied > 0 {
            r.commit_apply(c.applied);
        }
        let term = r.term;
        r.become_follower(term, INVALID_ID);

        // Used to resume Joint Consensus Changes
        let pending_conf_state = raft_state.pending_conf_state();
        let pending_conf_state_start_index = raft_state.pending_conf_state_start_index();
        match (pending_conf_state, pending_conf_state_start_index) {
            (Some(state), Some(idx)) => {
                r.begin_membership_change(&ConfChange::from((*idx, state.clone())))?;
            }
            (None, None) => (),
            _ => unreachable!("Should never find pending_conf_change without an index."),
        };

        info!(
            "{} newRaft [peers: {:?}, term: {:?}, commit: {}, applied: {}, last_index: {}, \
             last_term: {}, pending_membership_change: {:?}]",
            r.tag,
            r.prs().voters().collect::<Vec<_>>(),
            r.term,
            r.raft_log.committed,
            r.raft_log.get_applied(),
            r.raft_log.last_index(),
            r.raft_log.last_term(),
            r.pending_membership_change(),
        );
        Ok(r)
    }

    /// Grabs an immutable reference to the store.
    ///获取对商店的不变引用。
    #[inline]
    pub fn get_store(&self) -> &T {
        self.raft_log.get_store()
    }

    /// Grabs a mutable reference to the store.
    ///获取对商店的可变引用。
    #[inline]
    pub fn mut_store(&mut self) -> &mut T {
        self.raft_log.mut_store()
    }

    /// Grabs a reference to the snapshot
    ///获取对快照的引用
    #[inline]
    pub fn get_snap(&self) -> Option<&Snapshot> {
        self.raft_log.get_unstable().snapshot.as_ref()
    }

    /// Returns the number of pending read-only messages.
    ///返回未决的只读消息数。
    #[inline]
    pub fn pending_read_count(&self) -> usize {
        self.read_only.pending_read_count()
    }

    /// Returns how many read states exist.
    ///返回存在多少个读取状态。
    #[inline]
    pub fn ready_read_count(&self) -> usize {
        self.read_states.len()
    }

    /// Returns a value representing the softstate at the time of calling.
    ///返回一个值，该值表示调用时的软状态。
    pub fn soft_state(&self) -> SoftState {
        SoftState {
            leader_id: self.leader_id,
            raft_state: self.state,
        }
    }

    /// Returns a value representing the hardstate at the time of calling.
    ///返回一个值，该值表示调用时的硬状态。
    pub fn hard_state(&self) -> HardState {
        let mut hs = HardState::new();
        hs.set_term(self.term);
        hs.set_vote(self.vote);
        hs.set_commit(self.raft_log.committed);
        hs
    }

    /// Returns whether the current raft is in lease.
    ///返回当前筏是否处于租赁状态。
    pub fn in_lease(&self) -> bool {
        self.state == StateRole::Leader && self.check_quorum
    }

    /// For testing leader lease
    ///用于测试领导者租赁
    #[doc(hidden)]
    pub fn set_randomized_election_timeout(&mut self, t: usize) {
        assert!(self.min_election_timeout <= t && t < self.max_election_timeout);
        self.randomized_election_timeout = t;
    }

    /// Fetch the length of the election timeout.
    ///获取选举超时时间。
    pub fn get_election_timeout(&self) -> usize {
        self.election_timeout
    }

    /// Fetch the length of the heartbeat timeout
    ///获取心跳超时的长度
    pub fn get_heartbeat_timeout(&self) -> usize {
        self.heartbeat_timeout
    }

    /// Fetch the number of ticks elapsed since last heartbeat.
    ///获取自上次心跳以来经过的滴答数。
    pub fn get_heartbeat_elapsed(&self) -> usize {
        self.heartbeat_elapsed
    }

    /// Return the length of the current randomized election timeout.
    ///返回当前随机选举超时的长度。
    pub fn get_randomized_election_timeout(&self) -> usize {
        self.randomized_election_timeout
    }

    /// Set whether skip broadcast empty commit messages at runtime.
    ///设置是否在运行时跳过广播的空提交消息。
    #[inline]
    pub fn skip_bcast_commit(&mut self, skip: bool) {
        self.skip_bcast_commit = skip;
    }

    /// Set when the peer began a joint consensus change.
    ///
    /// This will also set `pending_conf_index` if it is larger than the existing number.
    ///设置对等方何时开始联合共识更改。
    ///如果大于现有数字，还将设置`pending_conf_index`。
    #[inline]
    fn set_pending_membership_change(&mut self, maybe_change: impl Into<Option<ConfChange>>) {
        let maybe_change = maybe_change.into();
        if let Some(ref change) = maybe_change {
            let index = change.get_start_index();
            //确保不能同时有两个pending_membership_change
            assert!(self.pending_membership_change.is_none() || index == self.pending_conf_index);
            if index > self.pending_conf_index {
                //更新index
                self.pending_conf_index = index;
            }
        }
        //更新conf
        self.pending_membership_change = maybe_change.clone();
    }

    /// Get the index which the pending membership change started at.
    ///
    /// > **Note:** This is an experimental feature.
    ///获取待更改的成员资格更改开始的索引。
     ///> **注意：**这是一项实验功能。
    #[inline]
    pub fn began_membership_change_at(&self) -> Option<u64> {
        self.pending_membership_change
            .as_ref()
            .map(|v| v.get_start_index())
    }

    /// send persists state to stable storage and then sends to its mailbox.
    ///将持久状态发送到稳定存储，然后发送到其邮箱。
    fn send(&mut self, mut m: Message) {
        debug!("Sending from {} to {}: {:?}", self.id, m.get_to(), m);
        m.set_from(self.id);
        if m.get_msg_type() == MessageType::MsgRequestVote
            || m.get_msg_type() == MessageType::MsgRequestPreVote
            || m.get_msg_type() == MessageType::MsgRequestVoteResponse
            || m.get_msg_type() == MessageType::MsgRequestPreVoteResponse
        {
            //这四种消息都不能为0，因为MsgRequestVote需要竞选必须有term,MsgRequestPreVote需要带有将要竞选的下一个term值
            if m.get_term() == 0 {
                // All {pre-,}campaign messages need to have the term set when
                // sending.
                // - MsgVote: m.Term is the term the node is campaigning for,
                //   non-zero as we increment the term when campaigning.
                // - MsgVoteResp: m.Term is the new r.Term if the MsgVote was
                //   granted, non-zero for the same reason MsgVote is
                // - MsgPreVote: m.Term is the term the node will campaign,
                //   non-zero as we use m.Term to indicate the next term we'll be
                //   campaigning for
                // - MsgPreVoteResp: m.Term is the term received in the original
                //   MsgPreVote if the pre-vote was granted, non-zero for the
                //   same reasons MsgPreVote is
              //所有{pre-，}运行消息在发送时都需要设置term。
              //-MsgVote：m.Term是节点正在竞选的term，非零，因为我们在竞选时增加了该term。
              //-MsgVoteResp：m.Term是新的r.Term（如果已授予MsgVote，则非零，出于相同的原因，MsgVote是
              //-MsgPreVote：m.Term是节点将vote的term，非零，因为我们使用m.Term表示我们将为其vote的下一个term
              //-MsgPreVoteResp：m.Term是如果授予预投票权则在原始MsgPreVote中收到的term，出于相同的原因，该值非零。
                panic!(
                    "{} term should be set when sending {:?}",
                    self.tag,
                    m.get_msg_type()
                );
            }
        } else {
            //其他消息应该在最终send的时候设定，在这个出现非0是不正常的
            if m.get_term() != 0 {
                panic!(
                    "{} term should not be set when sending {:?} (was {})",
                    self.tag,
                    m.get_msg_type(),
                    m.get_term()
                );
            }
            // do not attach term to MsgPropose, MsgReadIndex
            // proposals are a way to forward to the leader and
            // should be treated as local message.
            // MsgReadIndex is also forwarded to leader.
            //不要在MsgPropose、MsgReadIndex上附加term，proposals是转发给领导者的一种方法，应被视为本地消息。 MsgReadIndex也转发给领导者。
            if m.get_msg_type() != MessageType::MsgPropose
                && m.get_msg_type() != MessageType::MsgReadIndex
            {
                m.set_term(self.term);
            }
        }
        self.msgs.push(m);
    }

    fn prepare_send_snapshot(&mut self, m: &mut Message, pr: &mut Progress, to: u64) -> bool {
        //pr最近不活跃则不发送快照
        if !pr.recent_active {
            debug!(
                "{} ignore sending snapshot to {} since it is not recently active",
                self.tag, to
            );
            return false;
        }

        m.set_msg_type(MessageType::MsgSnapshot);
        //如果获取snapshot错误，则不发送快照
        let snapshot_r = self.raft_log.snapshot();
        if let Err(e) = snapshot_r {
            if e == Error::Store(StorageError::SnapshotTemporarilyUnavailable) {
                debug!(
                    "{} failed to send snapshot to {} because snapshot is temporarily \
                     unavailable",
                    self.tag, to
                );
                return false;
            }
            panic!("{} unexpected error: {:?}", self.tag, e);
        }
        let snapshot = snapshot_r.unwrap();
        if snapshot.get_metadata().get_index() == 0 {
            panic!("{} need non-empty snapshot", self.tag);
        }
        let (sindex, sterm) = (
            snapshot.get_metadata().get_index(),
            snapshot.get_metadata().get_term(),
        );
        m.set_snapshot(snapshot);
        debug!(
            "{} [firstindex: {}, commit: {}] sent snapshot[index: {}, term: {}] to {} \
             [{:?}]",
            self.tag,
            self.raft_log.first_index(),
            self.raft_log.committed,
            sindex,
            sterm,
            to,
            pr
        );
        //将pr置为snapshot状态
        pr.become_snapshot(sindex);
        debug!(
            "{} paused sending replication messages to {} [{:?}]",
            self.tag, to, pr
        );
        true
    }

    fn prepare_send_entries(
        &mut self,
        m: &mut Message,
        pr: &mut Progress,
        term: u64,
        ents: Vec<Entry>,
    ) {
        m.set_msg_type(MessageType::MsgAppend);
        m.set_index(pr.next_idx - 1);

        m.set_log_term(term);
        m.set_entries(RepeatedField::from_vec(ents));
        m.set_commit(self.raft_log.committed);
        if !m.get_entries().is_empty() {
            match pr.state {
                ProgressState::Replicate => {
                    let last = m.get_entries().last().unwrap().get_index();
                    //推进process的next_idx
                    pr.optimistic_update(last);
                    //正在进行发送的请求
                    pr.ins.add(last);
                }
                ProgressState::Probe => pr.pause(),
                _ => panic!(
                    "{} is sending append in unhandled state {:?}",
                    self.tag, pr.state
                ),
            }
        }
    }

    /// Sends RPC, with entries to the given peer.
    ///发送RPC，并将条目发送到给定的对等方。
    pub fn send_append(&mut self, to: u64, pr: &mut Progress) {
        //处以pause状态则不进行发送
        if pr.is_paused() {
            trace!(
                "Skipping sending to {}, it's paused. Progress: {:?}",
                to,
                pr
            );
            return;
        }
        let term = self.raft_log.term(pr.next_idx - 1);
        //需要发送给远端的ents
        let ents = self.raft_log.entries(pr.next_idx, self.max_msg_size);
        let mut m = Message::new();
        m.set_to(to);
        if term.is_err() || ents.is_err() {
            // send snapshot if we failed to get term or entries
            //如果无法获得term或entries，则发送快照
            trace!(
                "{} Skipping sending to {}, term: {:?}, ents: {:?}",
                self.tag,
                to,
                term,
                ents,
            );
            //将snapshot 装入message
            if !self.prepare_send_snapshot(&mut m, pr, to) {//这里指定发送的消息为快照类型
                return;
            }
        } else {
            //这里指定的消息为MsgAppend类型，具体内容为装载message
            self.prepare_send_entries(&mut m, pr, term.unwrap(), ents.unwrap());
        }
        self.send(m);
    }

    // send_heartbeat sends an empty MsgAppend
    // send_heartbeat发送一个空的MsgAppend
    fn send_heartbeat(&mut self, to: u64, pr: &Progress, ctx: Option<Vec<u8>>) {
        // Attach the commit as min(to.matched, self.raft_log.committed).
        // When the leader sends out heartbeat message,
        // the receiver(follower) might not be matched with the leader
        // or it might not have all the committed entries.
        // The leader MUST NOT forward the follower's commit to
        // an unmatched index.
        //将提交附加为min（to.matched，self.raft_log.committed）。
        //当领导者发出心跳消息时，接收者（追随者）可能与领导者不匹配，或者可能没有所有提交的条目。
        //领导者绝不能将跟随者的提交转发给无法匹配的索引。
        let mut m = Message::new();
        m.set_to(to);
        m.set_msg_type(MessageType::MsgHeartbeat);
        let commit = cmp::min(pr.matched, self.raft_log.committed);
        m.set_commit(commit);
        //如果ctx不为空，则说明是个readindex请求发送的心跳
        if let Some(context) = ctx {
            m.set_context(context);
        }
        self.send(m);
    }

    /// Sends RPC, with entries to all peers that are not up-to-date
    /// according to the progress recorded in r.prs().
    ///发送RPC，并根据r.prs（）中记录的进度向所有未更新的对等项发送条目。
    pub fn bcast_append(&mut self) {
        let self_id = self.id;
        let mut prs = self.take_prs();
        prs.iter_mut()
            .filter(|&(id, _)| *id != self_id)
            .for_each(|(id, pr)| self.send_append(*id, pr));
        self.set_prs(prs);
    }

    /// Sends RPC, without entries to all the peers.
    ///发送RPC，向所有peer不发送entries。
    pub fn bcast_heartbeat(&mut self) {
        let ctx = self.read_only.last_pending_request_ctx();
        self.bcast_heartbeat_with_ctx(ctx)
    }
    //带着读请求中的key进行广播
    #[cfg_attr(feature = "cargo-clippy", allow(clippy::needless_pass_by_value))]
    fn bcast_heartbeat_with_ctx(&mut self, ctx: Option<Vec<u8>>) {
        let self_id = self.id;
        let mut prs = self.take_prs();
        prs.iter_mut()
            .filter(|&(id, _)| *id != self_id)
            .for_each(|(id, pr)| self.send_heartbeat(*id, pr, ctx.clone()));
        self.set_prs(prs);
    }

    /// Attempts to advance the commit index. Returns true if the commit index
    /// changed (in which case the caller should call `r.bcast_append`).
    ///尝试提升commit索引。 如果提交索引已更改，则返回true（在这种情况下，调用方应调用`r.bcast_append`）。
    pub fn maybe_commit(&mut self) -> bool {
        //根据progress获取已经同步大半的最大index
        let mci = self.prs().maximal_committed_index();
        //比较是否需要更新commit
        self.raft_log.maybe_commit(mci, self.term)
    }

    /// Commit that the Raft peer has applied up to the given index.
    /// 确认Raft对等体已应用到给定索引。
    ///
    /// Registers the new applied index to the Raft log.
    /// 将新的应用索引注册到Raft日志中。
    ///
    /// # Hooks
    ///
    /// * Post: Checks to see if it's time to finalize a Joint Consensus state.
    /// Post：检查是否是时候确定“共同共识”状态。
    pub fn commit_apply(&mut self, applied: u64) {
        #[allow(deprecated)]
          //更新apply_index的值
        self.raft_log.applied_to(applied);

        // Check to see if we need to finalize a Joint Consensus state now.
        //检查是否需要立即完成“联合共识”状态。
        let start_index = self
            .pending_membership_change
            .as_ref()
            .map(|v| Some(v.get_start_index()))
            .unwrap_or(None);
        //如果membership发生改变的记录已经applied，则发送需要通知所有的node，只有leader需要做这个步骤
        if let Some(index) = start_index {
            // Invariant: We know that if we have commited past some index, we can also commit that index.
            //不变量：我们知道，如果我们提交的内容超过某个索引，那么我们也可以提交该索引。
            if applied >= index && self.state == StateRole::Leader {
                // We must replicate the commit entry.
                //我们必须复制提交条目。
                self.append_finalize_conf_change_entry();
            }
        }
    }
    //添加最后的conf change的entry
    fn append_finalize_conf_change_entry(&mut self) {
        let mut conf_change = ConfChange::new();
        conf_change.set_change_type(ConfChangeType::FinalizeMembershipChange);
        let data = protobuf::Message::write_to_bytes(&conf_change).unwrap();
        let mut entry = Entry::new();
        entry.set_entry_type(EntryType::EntryConfChange);
        entry.set_data(data);
        // Index/Term set here.
        self.append_entry(&mut [entry]);//添加entry之后可能会更新commit
        //根据每个节点的progress发送新添加的entry
        self.bcast_append();
    }

    /// Resets the current node to a given term.
    /// ///将当前节点重置为给定term。
    pub fn reset(&mut self, term: u64) {
        if self.term != term {
            self.term = term;
            //换了新leader则vote值为空,代表没选举leader
            self.vote = INVALID_ID;
        }
        //没有leader
        self.leader_id = INVALID_ID;
        self.reset_randomized_election_timeout();
        //elapsed都置0
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        //leader transfer任务丢弃
        self.abort_leader_transfer();
        //votes成员清空
        self.votes.clear();
        //conf_index置为0代表没有
        self.pending_conf_index = 0;
        //read内容也为空
        self.read_only = ReadOnly::new(self.read_only.option);

        let last_index = self.raft_log.last_index();
        let self_id = self.id;
        for (&id, pr) in self.mut_prs().iter_mut() {
            //将所有的pr都设置为自己的last_index+1
            pr.reset(last_index + 1);
            if id == self_id {
                //并将自己的progress matched设置为last_index
                pr.matched = last_index;
            }
        }
    }

    /// Appends a slice of entries to the log. The entries are updated to match
    /// the current index and term.
    ///在日志中追加条目的一部分。 条目将更新以匹配当前索引和术语。
    pub fn append_entry(&mut self, es: &mut [Entry]) {
        let mut li = self.raft_log.last_index();
        for (i, e) in es.iter_mut().enumerate() {
            e.set_term(self.term);
            e.set_index(li + 1 + i as u64);
        }
        // use latest "last" index after truncate/append
        //截断/附加后使用最新的“last”索引
        li = self.raft_log.append(es);

        let self_id = self.id;
      //这里可能更新progress的matched和next_idx大小,更新了match的大小就有可能更新commit的大小，所以接下来调用maybe_commit用于更新commit
        self.mut_prs().get_mut(self_id).unwrap().maybe_update(li);

        // Regardless of maybe_commit's return, our caller will call bcastAppend.
        //无论maybe_commit返回什么，我们的调用者都会调用bcastAppend。
        self.maybe_commit();
    }

    /// Returns true to indicate that there will probably be some readiness need to be handled.
    ///返回true，表示可能需要处理一些准备情况。
    pub fn tick(&mut self) -> bool {
        match self.state {
            StateRole::Follower | StateRole::PreCandidate | StateRole::Candidate => {
                self.tick_election()
            }
            StateRole::Leader => self.tick_heartbeat(),
        }
    }

    // TODO: revoke pub when there is a better way to test.
    /// Run by followers and candidates after self.election_timeout.
    ///
    /// Returns true to indicate that there will probably be some readiness need to be handled.
    // TODO：在有更好的测试方法时撤消pub。
    ///在self.election_timeout之后由关注者和候选人运行。
    ///返回true，表示可能需要处理一些准备情况。
    pub fn tick_election(&mut self) -> bool {
        self.election_elapsed += 1;
        //没有超过了选举超时时间，或者选民中不包含自己，则返回false
        if !self.pass_election_timeout() || !self.promotable() {
            return false;
        }
        //走到这里说明既超过了选举超时时间，选民中又包含自己
        self.election_elapsed = 0;
        //构建选举的raftmessage
        let m = new_message(INVALID_ID, MessageType::MsgHup, Some(self.id));
        //处理新消息
        self.step(m).is_ok();
        true
    }

    // tick_heartbeat is run by leaders to send a MsgBeat after self.heartbeat_timeout.
    // Returns true to indicate that there will probably be some readiness need to be handled.
    // tick_heartbeat由领导者运行，以在self.heartbeat_timeout之后发送MsgBeat。
    //返回true表示可能需要处理一些准备情况。
    fn tick_heartbeat(&mut self) -> bool {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;

        let mut has_ready = false;
        if self.election_elapsed >= self.election_timeout {
            self.election_elapsed = 0;
            //检查法定人数
            if self.check_quorum {
                //构建检查法定人数的raft message
                let m = new_message(INVALID_ID, MessageType::MsgCheckQuorum, Some(self.id));
                has_ready = true;
                //检查recent active member，如果不通过则变为follower
                self.step(m).is_ok();
            }
            //如果当前raft为leader，并且存在leader迁移，则放弃该迁移
            if self.state == StateRole::Leader && self.lead_transferee.is_some() {
                self.abort_leader_transfer()
            }
        }
        //如果当前状态不是leader则返回是否ready了
        if self.state != StateRole::Leader {
            return has_ready;
        }
        //如果headerbeat_elapsed超过心跳超时，则准备发送心跳
        if self.heartbeat_elapsed >= self.heartbeat_timeout {
            self.heartbeat_elapsed = 0;
            has_ready = true;
            let m = new_message(INVALID_ID, MessageType::MsgBeat, Some(self.id));
            //处理发送心跳信息
            self.step(m).is_ok();
        }
        has_ready
    }

    /// Converts this node to a follower.
    ///将此节点转换为关注者。
    pub fn become_follower(&mut self, term: u64, leader_id: u64) {
        self.reset(term);
        self.leader_id = leader_id;
        self.state = StateRole::Follower;
        info!("{} became follower at term {}", self.tag, self.term);
    }

    // TODO: revoke pub when there is a better way to test.
    /// Converts this node to a candidate
    /// 将此节点转换为候选
    ///
    /// # Panics
    ///
    /// Panics if a leader already exists.
    ///如果领导者已经存在，则感到恐慌。
    pub fn become_candidate(&mut self) {
        assert_ne!(
            self.state,
            StateRole::Leader,
            "invalid transition [leader -> candidate]"
        );
        let term = self.term + 1;
        //注意这里对vote ,term，votes，progress等内容的变化
        self.reset(term);
        let id = self.id;
        //投票给自己
        self.vote = id;
        //更新状态
        self.state = StateRole::Candidate;
        info!("{} became candidate at term {}", self.tag, self.term);
    }

    /// Converts this node to a pre-candidate
    /// 将此节点转换为候选对象
    ///
    /// # Panics
    ///
    /// Panics if a leader already exists.
    ///如果领导者已经存在，则感到恐慌。
    pub fn become_pre_candidate(&mut self) {
        assert_ne!(
            self.state,
            StateRole::Leader,
            "invalid transition [leader -> pre-candidate]"
        );
        // Becoming a pre-candidate changes our state.
        // but doesn't change anything else. In particular it does not increase
        // self.term or change self.vote.
        //成为候选人可以改变我们的状态。 但不会改变其他任何东西。 特别是它不会增加self.term或更改self.vote。
        self.state = StateRole::PreCandidate;
        //清空votes
        self.votes = HashMap::default();
        // If a network partition happens, and leader is in minority partition,
        // it will step down, and become follower without notifying others.
        //如果发生了网络分区，并且领导者在少数群体的分区中，它将退出并成为关注者，而不会通知其他人。
        //清空leader
        self.leader_id = INVALID_ID;
        info!("{} became pre-candidate at term {}", self.tag, self.term);
    }

    // TODO: revoke pub when there is a better way to test.
    /// Makes this raft the leader.
    ///使这个Raft成为领导者。
    ///
    /// # Panics
    ///
    /// Panics if this is a follower node.
    ///如果这是关注者节点，则表示恐慌。
    pub fn become_leader(&mut self) {
        trace!("ENTER become_leader");
        assert_ne!(
            self.state,
            StateRole::Follower,
            "invalid transition [follower -> leader]"
        );
        let term = self.term;
        self.reset(term);
        self.leader_id = self.id;
        self.state = StateRole::Leader;

        // Followers enter replicate mode when they've been successfully probed
        // (perhaps after having received a snapshot as a result). The leader is
        // trivially in this state. Note that r.reset() has initialized this
        // progress with the last index already.
        //追踪者成功探测后（可能是在收到快照后）进入复制模式。 领导者处于这种状态。
        //请注意，r.reset（）已使用最后一个索引初始化了此进度。
        let id = self.id;
        self.mut_prs().get_mut(id).unwrap().become_replicate();

        // Conservatively set the pending_conf_index to the last index in the
        // log. There may or may not be a pending config change, but it's
        // safe to delay any future proposals until we commit all our
        // pending log entries, and scanning the entire tail of the log
        // could be expensive.
        //保守地将未决_conf_index设置为日志中的最后一个索引。
      //可能有也可能没有挂起的配置更改，但是可以放心地推迟任何以后的提议，直到我们提交所有挂起的日志条目为止，并且扫描整个日志尾部可能会很昂贵。
        self.pending_conf_index = self.raft_log.last_index();
        //变为leader的时候增加一条空数据，然后广播，为的是提交之前term的数据
        self.append_entry(&mut [Entry::new()]);

        // In most cases, we append only a new entry marked with an index and term.
        // In the specific case of a node recovering while in the middle of a membership change,
        // and the finalization entry may have been lost, we must also append that, since it
        // would be overwritten by the term change.
        //在大多数情况下，我们只会附加一个标有索引和术语的新条目。
      //在特定的情况下，如果节点在成员资格更改过程中恢复，并且终结条目可能已丢失，则我们还必须附加该内容，因为它将被术语更改覆盖。
        let change_start_index = self
            .pending_membership_change
            .as_ref()
            .map(|v| Some(v.get_start_index()))
            .unwrap_or(None);
        if let Some(index) = change_start_index {
            trace!(
                "Checking if we need to finalize again..., began: {}, applied: {}, committed: {}",
                index,
                self.raft_log.applied,
                self.raft_log.committed
            );
            if index <= self.raft_log.committed {
                self.append_finalize_conf_change_entry();
            }
        }

        info!("{} became leader at term {}", self.tag, self.term);
        trace!("EXIT become_leader");
    }

    fn num_pending_conf(&self, ents: &[Entry]) -> usize {
        ents.iter()
            .filter(|e| e.get_entry_type() == EntryType::EntryConfChange)
            .count()
    }

    /// Campaign to attempt to become a leader.
    ///
    /// If prevote is enabled, this is handled as well.
    ///试图成为领导者的运动。
     ///如果启用了prevote，则也会处理。
    pub fn campaign(&mut self, campaign_type: &[u8]) {
        let (vote_msg, term) = if campaign_type == CAMPAIGN_PRE_ELECTION {
            //注意这里become_pre_candidate和become_candidate的区别，become_pre_candidate并没有reset term,而become_candidate reset了term,将term进行了+1,所以有了下边的self.term+1 以及become_candidate后的self.term
            self.become_pre_candidate();
            // Pre-vote RPCs are sent for next term before we've incremented self.term.
            //在增加self.term之前，先发送下一个投票的RPC。
            (MessageType::MsgRequestPreVote, self.term + 1)
        } else {
            self.become_candidate();
            (MessageType::MsgRequestVote, self.term)
        };
        let self_id = self.id;
        let acceptance = true;
        info!(
            "{} received {:?} from {} at term {}",
            self.id, vote_msg, self_id, self.term
        );
        //自己给自己投票，其中self.vote已经在become_candidate中设定为自己
        self.register_vote(self_id, acceptance);
        //如果获得了大多数的投票则进入下一状态，如果是leader transfer ,则直接变成leader  (此判断只有一个节点的时候会成功)
        if let CandidacyStatus::Elected = self.prs().candidacy_status(&self.votes) {
            // We won the election after voting for ourselves (which must mean that
            // this is a single-node cluster). Advance to the next state.
            //我们在为自己投票后赢得了选举（这必须表示这是一个单节点集群）。 前进到下一个状态。
            if campaign_type == CAMPAIGN_PRE_ELECTION {
                self.campaign(CAMPAIGN_ELECTION);
            } else {
                self.become_leader();
            }
            return;
        }

        // Only send vote request to voters.
        //仅向选民发送投票请求 。
        let prs = self.take_prs();
        prs.voter_ids()
            .iter()
            .filter(|&id| *id != self_id)
            .for_each(|&id| {
                info!(
                    "{} [logterm: {}, index: {}] sent {:?} request to {} at term {}",
                    self.tag,
                    self.raft_log.last_term(),
                    self.raft_log.last_index(),
                    vote_msg,
                    id,
                    self.term
                );
                //如果竞争类型是CAMPAIGN_PRE_ELECTION，则发送MsgRequestPreVote类型的请求，CAMPAIGN_TRANSFER和CAMPAIGN_ELECTION的竞争发送MsgRequestVote请求
                let mut m = new_message(id, vote_msg, None);
                m.set_term(term);
                m.set_index(self.raft_log.last_index());
                m.set_log_term(self.raft_log.last_term());
                //如果受leader邀请让自己变为leader，则自己的竞争context为CAMPAIGN_TRANSFER
                if campaign_type == CAMPAIGN_TRANSFER {
                    m.set_context(campaign_type.to_vec());
                }
                self.send(m);
            });
        self.set_prs(prs);
    }

    /// Sets the vote of `id` to `vote`.
    ///将id的投票设置为vote。
    fn register_vote(&mut self, id: u64, vote: bool) {
        self.votes.entry(id).or_insert(vote);
    }

    /// Steps the raft along via a message. This should be called everytime your raft receives a
    /// message from a peer.
    ///通过一条消息沿着raft 前进。 每当您的raft收到peer发送的消息时，都应调用此名称。
    pub fn step(&mut self, m: Message) -> Result<()> {
        // Handle the message term, which may result in our stepping down to a follower.
        //处理消息项，这可能会导致我们下台至关注者。

        if m.get_term() == 0 {
            // local message
            //本地消息
        } else if m.get_term() > self.term {//消息的term大于当前节点的term
            //如果是vote请求，则有3种可能CAMPAIGN_TRANSFER、CAMPAIGN_ELECTION、CAMPAIGN_PRE_ELECTION
            if m.get_msg_type() == MessageType::MsgRequestVote
                || m.get_msg_type() == MessageType::MsgRequestPreVote
            {
                //强制leader转换
                let force = m.get_context() == CAMPAIGN_TRANSFER;
                //设定检查quorum并且leader不为null，并且没有选举超时认为是在lease中
                let in_lease = self.check_quorum
                    && self.leader_id != INVALID_ID
                    && self.election_elapsed < self.election_timeout;
                //不是强制leader转换，并且在lease内 则忽略该请求(在lease内发起选举则可能是被踢掉的node因为接受不到leader的心跳导致的竞争【leader在移除指定节点后不会给他发心跳】)。意思:force为true说明是leader主动让位，可以统一他的竞选，如果不在任期内同样可以任选
                if !force && in_lease {
                    // if a server receives RequestVote request within the minimum election
                    // timeout of hearing from a current leader, it does not update its term
                    // or grant its vote
                    //
                    // This is included in the 3rd concern for Joint Consensus, where if another
                    // peer is removed from the cluster it may try to hold elections and disrupt
                    // stability.
                  //如果服务器在当前领导者的听证会的最小选举超时时间内收到RequestVote请求，则它不会更新其任期或授予其投票权
                  //
                  //这是“联合共识”的第三个关注点，其中，如果从集群中删除另一个对等方，则可能会尝试举行选举并破坏稳定性。
                    info!(
                        "{} [logterm: {}, index: {}, vote: {}] ignored {:?} vote from \
                         {} [logterm: {}, index: {}] at term {}: lease is not expired \
                         (remaining ticks: {})",
                        self.tag,
                        self.raft_log.last_term(),
                        self.raft_log.last_index(),
                        self.vote,
                        m.get_msg_type(),
                        m.get_from(),
                        m.get_log_term(),
                        m.get_index(),
                        self.term,
                        self.election_timeout - self.election_elapsed
                    );

                    return Ok(());
                }
            }
            //如果 m.term> self.term，并且是MsgRequestPreVote或则未拒绝的MsgRequestPreVoteResponse请求，则目前不做任何操作，在下边的match中会做处理
            if m.get_msg_type() == MessageType::MsgRequestPreVote
                || (m.get_msg_type() == MessageType::MsgRequestPreVoteResponse && !m.get_reject())
            {
                // For a pre-vote request:
                // Never change our term in response to a pre-vote request.
                //
                // For a pre-vote response with pre-vote granted:
                // We send pre-vote requests with a term in our future. If the
                // pre-vote is granted, we will increment our term when we get a
                // quorum. If it is not, the term comes from the node that
                // rejected our vote so we should become a follower at the new
                // term.
              //对于预投票请求：
              //切勿更改我们的term以响应预投票请求。
              //
              //对于已授予预投票的预投票响应：
              //我们会在未来发送带有期限的投票前请求。 如果预投票获得批准，我们将在达到法定人数时增加任期。
              //如果不是，则该term来自拒绝我们投票的节点，因此我们应该成为新term的追随者。
            } else {
                //否则
                info!(
                    "{} [term: {}] received a {:?} message with higher term from {} [term: {}]",
                    self.tag,
                    self.term,
                    m.get_msg_type(),
                    m.get_from(),
                    m.get_term()
                );
                //在m.term>self.term，并且是以下请求的情况下变为follower
                if m.get_msg_type() == MessageType::MsgAppend
                    || m.get_msg_type() == MessageType::MsgHeartbeat
                    || m.get_msg_type() == MessageType::MsgSnapshot
                {
                    self.become_follower(m.get_term(), m.get_from());
                } else {
                    self.become_follower(m.get_term(), INVALID_ID);
                }
            }
        } else if m.get_term() < self.term {
            //如果m.term<self.term 并且 开启check_quorum或者pre_vote，并且是MsgHeartbeat或者MsgAppend请求时,直接发送返回消息，目的是为了防止term的无线增加
            if (self.check_quorum || self.pre_vote)
                && (m.get_msg_type() == MessageType::MsgHeartbeat
                    || m.get_msg_type() == MessageType::MsgAppend)
            {
                // We have received messages from a leader at a lower term. It is possible
                // that these messages were simply delayed in the network, but this could
                // also mean that this node has advanced its term number during a network
                // partition, and it is now unable to either win an election or to rejoin
                // the majority on the old term（当前节点无法加入之前的majority，并且无法赢得选举）. If checkQuorum is false, this will be
                // handled by incrementing term numbers in response to MsgVote with a higher
                // term, but if checkQuorum is true we may not advance the term on MsgVote and
                // must generate other messages to advance the term. The net result of these
                // two features is to minimize the disruption caused by nodes that have been
                // removed from the cluster's configuration: a removed node will send MsgVotes
                // which will be ignored, but it will not receive MsgApp or MsgHeartbeat, so it
                // will not create disruptive term increases, by notifying leader of this node's
                // activeness.
              //我们收到了来自低term leader的消息。
              //这些消息可能只是在网络中被延迟了，但是这也可能意味着当前节点在网络分区期间已经提高了其任期编号，现在它无法赢得选举或重新加入旧任期的多数席位。 。
              //如果checkQuorum为false，这将通过增加具有较高词条的MsgVote的术语编号来解决，
              //但是如果checkQuorum为true，我们可能不会在MsgVote上推进该term，并且必须生成其他消息以使该术语超前。
              //这两个功能的最终结果是最大程度地减少了从集群配置中删除的节点所造成的破坏：一个删除的节点将发送MsgVotes，该消息将被忽略，但不会接收MsgApp或MsgHeartbeat，因此不会造成破坏性的影响。
              //通过通知领导者该节点的活动性来增加期限。
                // The above comments also true for Pre-Vote
                //以上评论对预投票也适用
                //
                // When follower gets isolated, it soon starts an election ending
                // up with a higher term than leader, although it won't receive enough
                // votes to win the election. When it regains connectivity, this response
                // with "pb.MsgAppResp" of higher term would force leader to step down.
                // However, this disruption is inevitable to free this stuck node with
                // fresh election. This can be prevented with Pre-Vote phase.
                //当追随者被孤立时，它将很快开始选举，其任期比领导者的任期长，尽管它不会获得足够的选票来赢得选举。
              //当恢复连接时，带有较高期限的“ pb.MsgAppResp”的响应将迫使领导者下台。
              //但是，这种中断是不可避免的，以释放带有新选举的卡住节点。 这可以通过预投票阶段来防止。
                let to_send = new_message(m.get_from(), MessageType::MsgAppendResponse, None);
                self.send(to_send);
            } else if m.get_msg_type() == MessageType::MsgRequestPreVote {//m.term < self.term 并且是MsgRequestPreVote，直接reject
                // Before pre_vote enable, there may be a recieving candidate with higher term,
                // but less log. After update to pre_vote, the cluster may deadlock if
                // we drop messages with a lower term.
                //在启用pre_vote之前，可能会有一个接收期限更高，但日志较少的候选者。
                //更新到pre_vote之后，如果我们丢弃期限较低的消息，集群可能会死锁。
                info!(
                    "{} [log_term: {}, index: {}, vote: {}] rejected {:?} from {} [log_term: {}, index: {}] at term {}",
                    self.id,
                    self.raft_log.last_term(),
                    self.raft_log.last_index(),
                    self.vote,
                    m.get_msg_type(),
                    m.get_from(),
                    m.get_log_term(),
                    m.get_index(),
                    self.term,
                );

                let mut to_send =
                    new_message(m.get_from(), MessageType::MsgRequestPreVoteResponse, None);
                to_send.set_term(self.term);
                to_send.set_reject(true);
                self.send(to_send);
            } else {
                // ignore other cases
                //忽略其他情况
                info!(
                    "{} [term: {}] ignored a {:?} message with lower term from {} [term: {}]",
                    self.tag,
                    self.term,
                    m.get_msg_type(),
                    m.get_from(),
                    m.get_term()
                );
            }
            return Ok(());
        }

        #[cfg(feature = "failpoint")]
        fail_point!("before_step");

        match m.get_msg_type() {
            MessageType::MsgHup => {
                if self.state != StateRole::Leader {
                    let ents = self
                        .raft_log
                        .slice(
                            self.raft_log.applied + 1,
                            self.raft_log.committed + 1,
                            raft_log::NO_LIMIT,
                        )
                        .expect("unexpected error getting unapplied entries");
                    //已提交但是没有 apply的confChange entry个数
                    let n = self.num_pending_conf(&ents);
                    //如果存在需要apply的confchange,则不能竞争leader
                    if n != 0 && self.raft_log.committed > self.raft_log.applied {
                        warn!(
                            "{} cannot campaign at term {} since there are still {} pending \
                             configuration changes to apply",
                            self.tag, self.term, n
                        );
                        return Ok(());
                    }
                    info!(
                        "{} is starting a new election at term {}",
                        self.tag, self.term
                    );
                    if self.pre_vote {
                        self.campaign(CAMPAIGN_PRE_ELECTION);
                    } else {
                        self.campaign(CAMPAIGN_ELECTION);
                    }
                } else {
                    debug!("{} ignoring MsgHup because already leader", self.tag);
                }
            }
            MessageType::MsgRequestVote | MessageType::MsgRequestPreVote => {
                // We can vote if this is a repeat of a vote we've already cast...
                //如果这是我们已经投票的重复，我们可以投票...
                let can_vote = (self.vote == m.get_from()) ||
                    // ...we haven't voted and we don't think there's a leader yet in this term...
                    // ...我们还没有投票，而且我们认为这个学期还没有领导人...
                    (self.vote == INVALID_ID && self.leader_id == INVALID_ID) ||
                    // ...or this is a PreVote for a future term...
                    // ...或者这是未来学期的PreVote ...
                    (m.msg_type == MessageType::MsgRequestPreVote && m.get_term() > self.term);
                // ...and we believe the candidate is up to date.
                // ...而且我们认为候选人是最新的。
                if can_vote && self.raft_log.is_up_to_date(m.get_index(), m.get_log_term()) {
                    // When responding to Msg{Pre,}Vote messages we include the term
                    // from the message, not the local term. To see why consider the
                    // case where a single node was previously partitioned away and
                    // it's local term is now of date. If we include the local term
                    // (recall that for pre-votes we don't update the local term), the
                    // (pre-)campaigning node on the other end will proceed to ignore
                    // the message (it ignores all out of date messages).
                    // The term in the original message and current local term are the
                    // same in the case of regular votes, but different for pre-votes.
                    //当回复Msg {Pre，} Vote消息时，我们会包含消息中的term，而不是本地term。
                  //要了解为什么要考虑以前将单个节点分割开并且其本地术语为最新的情况。
                  //如果我们包含本地term（请记住，对于预投票，我们不会更新本地term），另一端的（预）campaigning节点将继续忽略该消息（它将忽略所有过时的消息）。
                  //在常规投票中，原始消息中的term和当前本地term相同，但预先投票中的term不同。
                    self.log_vote_approve(&m);
                    let mut to_send =
                        new_message(m.get_from(), vote_resp_msg_type(m.get_msg_type()), None);
                    to_send.set_reject(false);
                    to_send.set_term(m.get_term());
                    self.send(to_send);
                    if m.get_msg_type() == MessageType::MsgRequestVote {
                        // Only record real votes.
                        //仅记录实际投票。
                        self.election_elapsed = 0;
                        self.vote = m.get_from();
                    }
                } else {
                    self.log_vote_reject(&m);
                    let mut to_send =
                        new_message(m.get_from(), vote_resp_msg_type(m.get_msg_type()), None);
                    to_send.set_reject(true);
                    to_send.set_term(self.term);
                    self.send(to_send);
                }
            }
            _ => match self.state {
                StateRole::PreCandidate | StateRole::Candidate => self.step_candidate(m)?,
                StateRole::Follower => self.step_follower(m)?,
                StateRole::Leader => self.step_leader(m)?,
            },
        }
        Ok(())
    }

    /// Apply a `BeginMembershipChange` variant `ConfChange`.
    ///应用`BeginMembershipChange`变体`ConfChange`。
    ///
    /// > **Note:** This is an experimental feature.
    ///> **注意：**这是一项实验功能。
    ///
    /// When a Raft node applies this variant of a configuration change it will adopt a joint
    /// configuration state until the membership change is finalized.
    ///当Raft节点应用此配置更改的变体时，它将采用联合配置状态，直到完成成员资格更改为止。
    ///
    /// During this time the `Raft` will have two, possibly overlapping, cooperating quorums for
    /// both elections and log replication.
    ///在此期间，“筏”将有两个可能重叠的协作仲裁，用于选举和日志复制。
    ///
    /// # Errors
    ///
    /// * `ConfChange.change_type` is not `BeginMembershipChange`
    /// * `ConfChange.configuration` does not exist.
    /// * `ConfChange.start_index` does not exist. It **must** equal the index of the
    ///   corresponding entry.
    /// *`ConfChange.change_type`不是`BeginMembershipChange`
    /// *`ConfChange.configuration`不存在。
    /// *`ConfChange.start_index`不存在。 **必须**等于相应条目的索引。
    #[inline(always)]
    pub fn begin_membership_change(&mut self, conf_change: &ConfChange) -> Result<()> {
        if conf_change.get_change_type() != ConfChangeType::BeginMembershipChange {
            return Err(Error::ViolatesContract(format!(
                "{:?} != BeginMembershipChange",
                conf_change.get_change_type()
            )));
        }
        let configuration = if conf_change.has_configuration() {
            conf_change.get_configuration().clone()
        } else {
            return Err(Error::ViolatesContract(
                "!ConfChange::has_configuration()".into(),
            ));
        };
        if conf_change.get_start_index() == 0 {
            return Err(Error::ViolatesContract(
                "!ConfChange::has_start_index()".into(),
            ));
        };

        self.set_pending_membership_change(conf_change.clone());
        let max_inflights = self.max_inflight;
        self.mut_prs()
            .begin_membership_change(configuration, Progress::new(1, max_inflights))?;
        Ok(())
    }

    /// Apply a `FinalizeMembershipChange` variant `ConfChange`.
    ///应用`FinalizeMembershipChange`变体`ConfChange`。
    ///
    /// > **Note:** This is an experimental feature.
    ///
    /// When a Raft node applies this variant of a configuration change it will finalize the
    /// transition begun by [`begin_membership_change`].
    ///当Raft节点应用此配置更改变体时，它将最终确定由[`begin_membership_change`]开始的转换。
    ///
    /// Once this is called the Raft will no longer have two, possibly overlapping, cooperating
    /// qourums.
    ///一旦被称为“raft”，将不再有两个可能重叠的协作qourum。
    ///
    /// # Errors
    ///
    /// * This Raft is not in a configuration change via `begin_membership_change`.
    /// * `ConfChange.change_type` is not a `FinalizeMembershipChange`.
    /// * `ConfChange.configuration` value should not exist.
    /// * `ConfChange.start_index` value should not exist.
    /// 该筏没有通过`begin_membership_change`进行配置更改。
    /// *`ConfChange.change_type`不是`FinalizeMembershipChange`。
     /// *`ConfChange.configuration`值不应存在。
     /// *`ConfChange.start_index`值不应存在。
    #[inline(always)]
    pub fn finalize_membership_change(&mut self, conf_change: &ConfChange) -> Result<()> {
        if conf_change.get_change_type() != ConfChangeType::FinalizeMembershipChange {
            return Err(Error::ViolatesContract(format!(
                "{:?} != BeginMembershipChange",
                conf_change.get_change_type()
            )));
        }
        if conf_change.has_configuration() {
            return Err(Error::ViolatesContract(
                "ConfChange::has_configuration()".into(),
            ));
        };
        let leader_in_new_set = self
            .prs()
            .next_configuration()
            .as_ref()
            .map(|config| config.contains(self.leader_id))
            .ok_or_else(|| Error::NoPendingMembershipChange)?;

        // Joint Consensus, in the Raft paper, states the leader should step down and become a
        // follower if it is removed during a transition.
        //共同意见书（Raft Paper）中指出，如果领导在过渡期间被撤职，领导者应下台并成为追随者。
        if !leader_in_new_set {
            let last_term = self.raft_log.last_term();
            if self.state == StateRole::Leader {
                self.become_follower(last_term, INVALID_ID);
            } else {
                // It's no longer safe to lookup the ID in the ProgressSet, remove it.
                //在ProgressSet中查找ID不再是安全的，移除他。
                self.leader_id = INVALID_ID;
            }
        }

        self.mut_prs().finalize_membership_change()?;
        // Ensure we reset this on *any* node, since the leader might have failed
        // and we don't want to finalize twice.
        //确保我们在* any *节点上重置此设置，因为领导者可能已失败，并且我们不想两次完成。
        self.set_pending_membership_change(None);
        Ok(())
    }

    fn log_vote_approve(&self, m: &Message) {
        info!(
            "{} [logterm: {}, index: {}, vote: {}] cast {:?} for {} [logterm: {}, index: {}] \
             at term {}",
            self.tag,
            self.raft_log.last_term(),
            self.raft_log.last_index(),
            self.vote,
            m.get_msg_type(),
            m.get_from(),
            m.get_log_term(),
            m.get_index(),
            self.term
        );
    }

    fn log_vote_reject(&self, m: &Message) {
        info!(
            "{} [logterm: {}, index: {}, vote: {}] rejected {:?} from {} [logterm: {}, index: \
             {}] at term {}",
            self.tag,
            self.raft_log.last_term(),
            self.raft_log.last_index(),
            self.vote,
            m.get_msg_type(),
            m.get_from(),
            m.get_log_term(),
            m.get_index(),
            self.term
        );
    }

    fn handle_append_response(
        &mut self,
        m: &Message,
        prs: &mut ProgressSet,
        old_paused: &mut bool,
        send_append: &mut bool,
        maybe_commit: &mut bool,
    ) {
        let pr = prs.get_mut(m.get_from()).unwrap();
        pr.recent_active = true;

        if m.get_reject() {
            debug!(
                "{} received msgAppend rejection(lastindex: {}) from {} for index {}",
                self.tag,
                m.get_reject_hint(),
                m.get_from(),
                m.get_index()
            );
            //发送被拒绝了则有可能需要调整match和next_index的大小
            if pr.maybe_decr_to(m.get_index(), m.get_reject_hint()) {
                debug!(
                    "{} decreased progress of {} to [{:?}]",
                    self.tag,
                    m.get_from(),
                    pr
                );
                //如果重调了nextindex，则将自己状态置为probe，下次再被拒绝的时候走maybe_desc_to中的非replica状态逻辑
                if pr.state == ProgressState::Replicate {
                    //重设自己的状态和nextindex
                    pr.become_probe();
                }
                //被拒绝并调整了match和next_index的大小，说明需要发送数据重新试探
                *send_append = true;
            }
            return;
        }
        //走到这里说明没有拒绝
        *old_paused = pr.is_paused();
        //更新自己的progress 的 match字段
        if !pr.maybe_update(m.get_index()) {
            return;
        }

        // Transfer leadership is in progress.
        //有leader转移命令
        if let Some(lead_transferee) = self.lead_transferee {
            let last_index = self.raft_log.last_index();
            //这个follwer就是我要转移的对象，并且pr的matched已经达到leader的last_index，则发送MsgTimeoutNow命令
            if m.get_from() == lead_transferee && pr.matched == last_index {
                info!(
                    "{} sent MsgTimeoutNow to {} after received MsgAppResp",
                    self.tag,
                    m.get_from()
                );
                self.send_timeout_now(m.get_from());
            }
        }
        //这里代表没有rejct
        match pr.state {
            //如果死probe则变为replicate可进行复制
            ProgressState::Probe => pr.become_replicate(),
            ProgressState::Snapshot => {
                if !pr.maybe_snapshot_abort() {
                    return;
                }
                //走到这里说明丢弃了snapshot，这里需要probe 待发送的数据
                debug!(
                    "{} snapshot aborted, resumed sending replication messages to {} \
                     [{:?}]",
                    self.tag,
                    m.get_from(),
                    pr
                );
                pr.become_probe();
            }
            ProgressState::Replicate => pr.ins.free_to(m.get_index()),
        }
        //可能需要执行commit
        *maybe_commit = true;
    }

    fn handle_heartbeat_response(
        &mut self,
        m: &Message,
        prs: &mut ProgressSet,
        send_append: &mut bool,
        more_to_send: &mut Option<Message>,
    ) {
        // Update the node. Drop the value explicitly since we'll check the qourum after.
        //更新节点。 显式删除该值，因为我们将在之后检查仲裁。
        {
            let pr = prs.get_mut(m.get_from()).unwrap();
            pr.recent_active = true;
            pr.resume();

            // free one slot for the full inflights window to allow progress.
            //在整个飞行窗口中释放一个插槽以允许progress。
            if pr.state == ProgressState::Replicate && pr.ins.full() {
                pr.ins.free_first_one();
            }
            if pr.matched < self.raft_log.last_index() {
                *send_append = true;
            }
            //如果read_only设定的不是safe或者contxt为空说明不是read_index发出的响应，到这就直接发挥了
            if self.read_only.option != ReadOnlyOption::Safe || m.get_context().is_empty() {
                return;
            }
        }
        //接收到heartbeat的ack不满足半数以上
        if !prs.has_quorum(&self.read_only.recv_ack(m)) {
            return;
        }
        //走到这一步说明readIndex请求已经接收到半数以上的节点的回应
        let rss = self.read_only.advance(m);
        //遍历所有可读的请求
        for rs in rss {
            let mut req = rs.req;
            if req.get_from() == INVALID_ID || req.get_from() == self.id {
                // from local member,本节点的raw_node发起的readIndexReq
                let rs = ReadState {
                    index: rs.index,
                    request_ctx: req.take_entries()[0].take_data(),
                };
                //是自己发起的请求放在自己的read_states中
                self.read_states.push(rs);
            } else {
                //如果是从其他节点发送的readIndexReq转发到leader这的请求，需要将该请求最后转发给原始请求节点
                let mut to_send = Message::new();
                to_send.set_to(req.get_from());
                to_send.set_msg_type(MessageType::MsgReadIndexResp);
                to_send.set_index(rs.index);
                to_send.set_entries(req.take_entries());
                //需要将msgReadIndexResp发送给其接收方
                *more_to_send = Some(to_send);
            }
        }
    }

    fn handle_transfer_leader(&mut self, m: &Message, prs: &mut ProgressSet) {
        let from = m.get_from();
        if prs.learner_ids().contains(&from) {
            debug!("{} is learner. Ignored transferring leadership", from);
            return;
        }
        let lead_transferee = from;
        let last_lead_transferee = self.lead_transferee;
        //之前存在未执行的leader stransferee
        if last_lead_transferee.is_some() {
            if last_lead_transferee.unwrap() == lead_transferee {
                info!(
                    "{} [term {}] transfer leadership to {} is in progress, ignores request \
                     to same node {}",
                    self.tag, self.term, lead_transferee, lead_transferee
                );
                return;
            }
            //如果上一次处理的leadertransfer还没有处理完，这次由发送了一个leader transfer，则停止上一个leader transfer
            self.abort_leader_transfer();
            info!(
                "{} [term {}] abort previous transferring leadership to {}",
                self.tag,
                self.term,
                last_lead_transferee.unwrap()
            );
        }
        if lead_transferee == self.id {
            debug!(
                "{} is already leader. Ignored transferring leadership to self",
                self.tag
            );
            return;
        }
        // Transfer leadership to third party.
        //将领导权移交给第三方。
        info!(
            "{} [term {}] starts to transfer leadership to {}",
            self.tag, self.term, lead_transferee
        );
        // Transfer leadership should be finished in one electionTimeout
        // so reset r.electionElapsed.
        //转移领导权应在一个选举超时时间内完成，因此请重置r.electionElapsed。
        self.election_elapsed = 0;
        self.lead_transferee = Some(lead_transferee);
        let pr = prs.get_mut(from).unwrap();
        //如果所有的消息都已经同步给未来的leader，则直接发送timeout消息就可以，代表transfer leader请求，不需要发送消息
        if pr.matched == self.raft_log.last_index() {
            self.send_timeout_now(lead_transferee);
            info!(
                "{} sends MsgTimeoutNow to {} immediately as {} already has up-to-date log",
                self.tag, lead_transferee, lead_transferee
            );
        } else {//否则需要连带发送消息到未来的leader
            self.send_append(lead_transferee, pr);
        }
    }

    fn handle_snapshot_status(&mut self, m: &Message, pr: &mut Progress) {
        if m.get_reject() {
            pr.snapshot_failure();
            //变为探测状态开始尝试发送entry
            pr.become_probe();
            debug!(
                "{} snapshot failed, resumed sending replication messages to {} [{:?}]",
                self.tag,
                m.get_from(),
                pr
            );
        } else {
            //没有拒绝则snapshot发送过成功了，直接变为探测状态
            pr.become_probe();
            debug!(
                "{} snapshot succeeded, resumed sending replication messages to {} [{:?}]",
                self.tag,
                m.get_from(),
                pr
            );
        }
        // If snapshot finish, wait for the msgAppResp from the remote node before sending
        // out the next msgAppend.
        // If snapshot failure, wait for a heartbeat interval before next try
        //如果快照完成，在发送下一个msgAppend之前等待远程节点发送msgAppResp。
        //如果快照失败，请等待心跳间隔时间，然后再尝试
        pr.pause();
    }

    /// Check message's progress to decide which action should be taken.
    ///检查消息的进度，以确定应采取的措施。
    fn check_message_with_progress(
        &mut self,
        m: &mut Message,
        send_append: &mut bool,
        old_paused: &mut bool,
        maybe_commit: &mut bool,
        more_to_send: &mut Option<Message>,
    ) {
        if self.prs().get(m.get_from()).is_none() {
            debug!("{} no progress available for {}", self.tag, m.get_from());
            return;
        }

        let mut prs = self.take_prs();
        match m.get_msg_type() {
            MessageType::MsgAppendResponse => {
                self.handle_append_response(m, &mut prs, old_paused, send_append, maybe_commit);
            }
            MessageType::MsgHeartbeatResponse => {
                self.handle_heartbeat_response(m, &mut prs, send_append, more_to_send);
            }
            MessageType::MsgSnapStatus => {
                let pr = prs.get_mut(m.get_from()).unwrap();
                if pr.state == ProgressState::Snapshot {
                    self.handle_snapshot_status(m, pr);
                }
            }
            MessageType::MsgUnreachable => {
                let pr = prs.get_mut(m.get_from()).unwrap();
                // During optimistic replication, if the remote becomes unreachable,
                // there is huge probability that a MsgAppend is lost.
                //在乐观复制期间，如果远程无法访问，则MsgAppend丢失的可能性很大。
                if pr.state == ProgressState::Replicate {
                    pr.become_probe();
                }
                debug!(
                    "{} failed to send message to {} because it is unreachable [{:?}]",
                    self.tag,
                    m.get_from(),
                    pr
                );
            }
            MessageType::MsgTransferLeader => {
                self.handle_transfer_leader(m, &mut prs);
            }
            _ => {}
        }
        self.set_prs(prs);
    }

    fn step_leader(&mut self, mut m: Message) -> Result<()> {
        // These message types do not require any progress for m.From.
        //这些消息类型不需要m.From的任何progress。
        match m.get_msg_type() {
            MessageType::MsgBeat => {
                self.bcast_heartbeat();
                return Ok(());
            }
            MessageType::MsgCheckQuorum => {
                //检查获取的follower是否超过半数
                if !self.check_quorum_active() {
                    warn!(
                        "{} stepped down to follower since quorum is not active",
                        self.tag
                    );
                    let term = self.term;
                    //没有超过半数则变为follower
                    self.become_follower(term, INVALID_ID);
                }
                return Ok(());
            }
            MessageType::MsgPropose => {
                if m.get_entries().is_empty() {
                    panic!("{} stepped empty MsgProp", self.tag);
                }
                if !self.prs().voter_ids().contains(&self.id) {
                    // If we are not currently a member of the range (i.e. this node
                    // was removed from the configuration while serving as leader),
                    // drop any new proposals.
                    //如果我们当前还不是该范围的成员（即该节点在担任领导者时已从配置中删除），请删除所有新提案。
                    return Err(Error::ProposalDropped);
                }
                if self.lead_transferee.is_some() {
                    debug!(
                        "{} [term {}] transfer leadership to {} is in progress; dropping \
                         proposal",
                        self.tag,
                        self.term,
                        self.lead_transferee.unwrap()
                    );
                    return Err(Error::ProposalDropped);
                }

                for (i, e) in m.mut_entries().iter_mut().enumerate() {
                    if e.get_entry_type() == EntryType::EntryConfChange {
                        //已经存在则忽略
                        if self.has_pending_conf() {
                            info!(
                                "propose conf {:?} ignored since pending unapplied \
                                 configuration [index {}, applied {}]",
                                e, self.pending_conf_index, self.raft_log.applied
                            );
                            *e = Entry::new();
                            e.set_entry_type(EntryType::EntryNormal);
                        } else {
                            self.pending_conf_index = self.raft_log.last_index() + i as u64 + 1;
                        }
                    }
                }
                self.append_entry(&mut m.mut_entries());
                self.bcast_append();
                return Ok(());
            }
            MessageType::MsgReadIndex => {
                if self.raft_log.term(self.raft_log.committed).unwrap_or(0) != self.term {
                    // Reject read only request when this leader has not committed any log entry
                    // in its term.
                    //当此领导者在其任期内未提交任何日志条目时，拒绝只读请求。
                    //为什么拒绝：因为不知道真实的commitid在哪个位置,leader在刚上任的时候会发送一个空的entry并提交，如果这个流程没做完不会处理任何读请求
                    return Ok(());
                }

                let mut self_set = HashSet::default();
                self_set.insert(self.id);
                //如果自己加入active quorum 不满足大于一半节点条件，则分条件处理
                if !self.prs().has_quorum(&self_set) {
                    // thinking: use an interally defined context instead of the user given context.
                    // We can express this in terms of the term and index instead of
                    // a user-supplied value.
                    // This would allow multiple reads to piggyback on the same message.
                  //思考：使用内部定义的上下文而不是用户给定的上下文。 我们可以用术语和索引而不是用户提供的值来表示。
                  //这将允许多次读取附带在同一条消息上。
                    match self.read_only.option {
                        //走readindex流程
                        ReadOnlyOption::Safe => {
                            //获取要请求的key
                            let ctx = m.get_entries()[0].get_data().to_vec();
                            //添加readindex请求到read_only，其中包含pending_read_index和read_index_queue两个字段
                            self.read_only.add_request(self.raft_log.committed, m);
                            self.bcast_heartbeat_with_ctx(Some(ctx));
                        }
                        ReadOnlyOption::LeaseBased => {
                            let mut read_index = self.raft_log.committed;
                            if m.get_from() == INVALID_ID || m.get_from() == self.id {
                                // from local member
                                let rs = ReadState {
                                    index: read_index,//当前已提交的Index
                                    request_ctx: m.take_entries()[0].take_data(),
                                };
                                self.read_states.push(rs);
                            } else {
                                //从其他节点发起MsgReadIndex请求，转发到leader节点上来的
                                let mut to_send = Message::new();
                                to_send.set_to(m.get_from());
                                to_send.set_msg_type(MessageType::MsgReadIndexResp);
                                to_send.set_index(read_index);
                                to_send.set_entries(m.take_entries());
                                self.send(to_send);
                            }
                        }
                    }
                } else {
                    //否则直接将读取请求放入当read_states，后续加以处理（有单独的线程读取该内容，然后处理）和msgs字段的地位相等
                    let rs = ReadState {
                        index: self.raft_log.committed,
                        request_ctx: m.take_entries()[0].take_data(),
                    };
                    self.read_states.push(rs);
                }
                return Ok(());
            }
            _ => {}
        }
        //执行到这里说明是响应消息
        let mut send_append = false;
        let mut maybe_commit = false;
        let mut old_paused = false;
        let mut more_to_send = None;
        //检查消息的发送情况
        self.check_message_with_progress(
            &mut m,
            &mut send_append,
            &mut old_paused,
            &mut maybe_commit,
            &mut more_to_send,
        );
      //处理需要更新commit字段的要求，比如，如果check_message_with_progress处理的是appendresponse消息，则可能超过半数进行了数据返回，此时可以提交，则进行数据commit操作
        if maybe_commit {
            if self.maybe_commit() {
                //判断是否能够bcase_commit
                if self.should_bcast_commit() {
                    //这里是将更新commitid绑定在了append 给其他节点数据的请求上
                    self.bcast_append();
                }
            } else if old_paused {
                // update() reset the wait state on this node. If we had delayed sending
                // an update before, send it now.
                // update（）重置此节点上的等待状态。 如果我们之前延迟发送更新，请立即发送。
                //如果old_paused为true则说明以前不能往这个节点上发数据，调整为可以发送数据
                send_append = true;
            }
        }
        //处理需要发送给其他节点的数据
        if send_append {
            let from = m.get_from();
            let mut prs = self.take_prs();
            self.send_append(from, prs.get_mut(from).unwrap());
            self.set_prs(prs);
        }
        //处理需要转发的请求
        if let Some(to_send) = more_to_send {
            self.send(to_send)
        }

        Ok(())
    }

    // step_candidate is shared by state Candidate and PreCandidate; the difference is
    // whether they respond to MsgRequestVote or MsgRequestPreVote.
    // step_candidate由状态Candidate和PreCandidate共享； 区别在于他们是响应MsgRequestVote还是MsgRequestPreVote。
    fn step_candidate(&mut self, m: Message) -> Result<()> {
        match m.get_msg_type() {
            MessageType::MsgPropose => {
                info!(
                    "{} no leader at term {}; dropping proposal",
                    self.tag, self.term
                );
                return Err(Error::ProposalDropped);
            }
            MessageType::MsgAppend => {
                debug_assert_eq!(self.term, m.get_term());
                self.become_follower(m.get_term(), m.get_from());
                self.handle_append_entries(&m);
            }
            MessageType::MsgHeartbeat => {
                debug_assert_eq!(self.term, m.get_term());
                self.become_follower(m.get_term(), m.get_from());
                self.handle_heartbeat(m);
            }
            MessageType::MsgSnapshot => {
                debug_assert_eq!(self.term, m.get_term());
                self.become_follower(m.get_term(), m.get_from());
                self.handle_snapshot(m);
            }
            MessageType::MsgRequestPreVoteResponse | MessageType::MsgRequestVoteResponse => {
                // Only handle vote responses corresponding to our candidacy (while in
                // state Candidate, we may get stale MsgPreVoteResp messages in this term from
                // our pre-candidate state).
                //仅处理与我们的候选资格相对应的投票响应（在候选状态下，此术语可能会从我们的候选前状态中获得陈旧的MsgPreVoteResp消息）。
                if (self.state == StateRole::PreCandidate
                    && m.get_msg_type() != MessageType::MsgRequestPreVoteResponse)
                    || (self.state == StateRole::Candidate
                        && m.get_msg_type() != MessageType::MsgRequestVoteResponse)
                {
                    return Ok(());
                }

                let acceptance = !m.get_reject();
                let msg_type = m.get_msg_type();
                let from_id = m.get_from();
                info!(
                    "{} received {:?}{} from {} at term {}",
                    self.id,
                    msg_type,
                    if !acceptance { " rejection" } else { "" },
                    from_id,
                    self.term
                );
                //设置自己的vote结果到votes中
                self.register_vote(from_id, acceptance);
                //判断是否超过半数了
                match self.prs().candidacy_status(&self.votes) {
                    CandidacyStatus::Elected => {
                        //超过半数了则更换状态继续竞选
                        if self.state == StateRole::PreCandidate {
                            self.campaign(CAMPAIGN_ELECTION);
                        } else {
                            //直接变为leader并广播数据，在成为leader的时候发送了一条空数据到raft，为的是提交之前term的数据，详情见raft论文
                            self.become_leader();
                            self.bcast_append();
                        }
                    }
                    //超过一半不同意，则变为follower
                    CandidacyStatus::Ineligible => {
                        // pb.MsgPreVoteResp contains future term of pre-candidate
                        // m.term > self.term; reuse self.term
                      // pb.MsgPreVoteResp包含预候选的future term m.term> self.term; 重用self.term
                        let term = self.term;
                        self.become_follower(term, INVALID_ID);
                    }
                    CandidacyStatus::Eligible => (),
                };
            }
            MessageType::MsgTimeoutNow => debug!(
                "{} [term {} state {:?}] ignored MsgTimeoutNow from {}",
                self.tag,
                self.term,
                self.state,
                m.get_from()
            ),
            _ => {}
        }
        Ok(())
    }

    fn step_follower(&mut self, mut m: Message) -> Result<()> {
        match m.get_msg_type() {
            MessageType::MsgPropose => {
                if self.leader_id == INVALID_ID {
                    info!(
                        "{} no leader at term {}; dropping proposal",
                        self.tag, self.term
                    );
                    return Err(Error::ProposalDropped);
                }
                m.set_to(self.leader_id);
                self.send(m);
            }
            MessageType::MsgAppend => {
                //接收到append消息就重置election_elapsed
                self.election_elapsed = 0;
                self.leader_id = m.get_from();
                self.handle_append_entries(&m);
            }
            MessageType::MsgHeartbeat => {
                self.election_elapsed = 0;
                self.leader_id = m.get_from();
                self.handle_heartbeat(m);
            }
            MessageType::MsgSnapshot => {
                self.election_elapsed = 0;
                self.leader_id = m.get_from();
                self.handle_snapshot(m);
            }
            MessageType::MsgTransferLeader => {
                if self.leader_id == INVALID_ID {
                    info!(
                        "{} no leader at term {}; dropping leader transfer msg",
                        self.tag, self.term
                    );
                    return Ok(());
                }
                m.set_to(self.leader_id);
                self.send(m);
            }
            MessageType::MsgTimeoutNow => {
                if self.promotable() {
                    info!(
                        "{} [term {}] received MsgTimeoutNow from {} and starts an election to \
                         get leadership.",
                        self.tag,
                        self.term,
                        m.get_from()
                    );
                    // Leadership transfers never use pre-vote even if self.pre_vote is true; we
                    // know we are not recovering from a partition so there is no need for the
                    // extra round trip.
                    //即使self.pre_vote为true，领导权转移也不会使用pre-vote； 我们知道我们不是从分区中恢复，因此不需要额外的往返。
                    self.campaign(CAMPAIGN_TRANSFER);
                } else {
                    info!(
                        "{} received MsgTimeoutNow from {} but is not promotable",
                        self.tag,
                        m.get_from()
                    );
                }
            }
            MessageType::MsgReadIndex => {
                if self.leader_id == INVALID_ID {
                    info!(
                        "{} no leader at term {}; dropping index reading msg",
                        self.tag, self.term
                    );
                    return Ok(());
                }
                m.set_to(self.leader_id);
                self.send(m);
            }
            MessageType::MsgReadIndexResp => {
                if m.get_entries().len() != 1 {
                    error!(
                        "{} invalid format of MsgReadIndexResp from {}, entries count: {}",
                        self.tag,
                        m.get_from(),
                        m.get_entries().len()
                    );
                    return Ok(());
                }
                let rs = ReadState {
                    index: m.get_index(),
                    request_ctx: m.take_entries()[0].take_data(),
                };
                self.read_states.push(rs);
            }
            _ => {}
        }
        Ok(())
    }

    // TODO: revoke pub when there is a better way to test.
    /// For a given message, append the entries to the log.
    ///对于给定的消息，将条目附加到日志中。
    pub fn handle_append_entries(&mut self, m: &Message) {
        if m.get_index() < self.raft_log.committed {
            debug!("{} Got message with lower index than committed.", self.tag);
            let mut to_send = Message::new();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.committed);
            self.send(to_send);
            return;
        }
        let mut to_send = Message::new();
        to_send.set_to(m.get_from());
        to_send.set_msg_type(MessageType::MsgAppendResponse);
        //这里follower进行添加entry
        match self.raft_log.maybe_append(
            m.get_index(),
            m.get_log_term(),
            m.get_commit(),
            m.get_entries(),
        ) {
            Some(mlast_index) => {
                to_send.set_index(mlast_index);
                self.send(to_send);
            }
            None => {
                debug!(
                    "{} [logterm: {:?}, index: {}] rejected msgApp [logterm: {}, index: {}] \
                     from {}",
                    self.tag,
                    self.raft_log.term(m.get_index()),
                    m.get_index(),
                    m.get_log_term(),
                    m.get_index(),
                    m.get_from()
                );
                to_send.set_index(m.get_index());
                to_send.set_reject(true);
                to_send.set_reject_hint(self.raft_log.last_index());
                self.send(to_send);
            }
        }
    }

    // TODO: revoke pub when there is a better way to test.
    /// For a message, commit and send out heartbeat.
    ///要获取消息，请提交并发送心跳。
    pub fn handle_heartbeat(&mut self, mut m: Message) {
        //leader发送过来的commitid取的是pr.matched和leader.commiteid的较小值，所以根据match限定，当前commitid一定比last_index小
        self.raft_log.commit_to(m.get_commit());
        let mut to_send = Message::new();
        to_send.set_to(m.get_from());
        to_send.set_msg_type(MessageType::MsgHeartbeatResponse);
        to_send.set_context(m.take_context());
        self.send(to_send);
    }

    fn handle_snapshot(&mut self, mut m: Message) {
        //snapshot last index, snapshot last term
        let (sindex, sterm) = (
            m.get_snapshot().get_metadata().get_index(),
            m.get_snapshot().get_metadata().get_term(),
        );
        if self.restore(m.take_snapshot()) {
            info!(
                "{} [commit: {}, term: {}] restored snapshot [index: {}, term: {}]",
                self.tag, self.term, self.raft_log.committed, sindex, sterm
            );
            let mut to_send = Message::new();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.last_index());
            self.send(to_send);
        } else {
            info!(
                "{} [commit: {}] ignored snapshot [index: {}, term: {}]",
                self.tag, self.raft_log.committed, sindex, sterm
            );
            let mut to_send = Message::new();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.committed);
            self.send(to_send);
        }
    }
    //恢复raft本身的状态机，不是apply的那个状态机
    fn restore_raft(&mut self, snap: &Snapshot) -> Option<bool> {
        let meta = snap.get_metadata();
        if self.raft_log.match_term(meta.get_index(), meta.get_term()) {
            info!(
                "{} [commit: {}, lastindex: {}, lastterm: {}] fast-forwarded commit to \
                 snapshot [index: {}, term: {}]",
                self.tag,
                self.raft_log.committed,
                self.raft_log.last_index(),
                self.raft_log.last_term(),
                meta.get_index(),
                meta.get_term()
            );
            self.raft_log.commit_to(meta.get_index());
            return Some(false);
        }

        // Both of learners and voters are empty means the peer is created by ConfChange.
        //学习者和投票者都是空的，这意味着同伴是由ConfChange创建的。
        if self.prs().iter().len() != 0 && !self.is_learner {
            for &id in meta.get_conf_state().get_learners() {
                if id == self.id {
                    error!(
                        "{} can't become learner when restores snapshot [index: {}, term: {}]",
                        self.tag,
                        meta.get_index(),
                        meta.get_term(),
                    );
                    return Some(false);
                }
            }
        }

        info!(
            "{} [commit: {}, lastindex: {}, lastterm: {}] starts to restore snapshot \
             [index: {}, term: {}]",
            self.tag,
            self.raft_log.committed,
            self.raft_log.last_index(),
            self.raft_log.last_term(),
            meta.get_index(),
            meta.get_term()
        );

        let next_idx = self.raft_log.last_index() + 1;
        let mut prs = ProgressSet::restore_snapmeta(meta, next_idx, self.max_inflight);
        prs.get_mut(self.id).unwrap().matched = next_idx - 1;
        //自己是个learner ,但是voter中包含自己，则将自己置为非learner
        if self.is_learner && prs.configuration().voters().contains(&self.id) {
            self.is_learner = false;
        }
        self.prs = Some(prs);
        if meta.get_pending_membership_change_index() > 0 {
            let cs = meta.get_pending_membership_change().clone();
            //存在membership change 则构建ConfChange对象并存储在pending_membership_change
            let mut conf_change = ConfChange::new();
            conf_change.set_change_type(ConfChangeType::BeginMembershipChange);
            conf_change.set_configuration(cs);
            //设置confchange的index
            conf_change.set_start_index(meta.get_pending_membership_change_index());
            self.pending_membership_change = Some(conf_change);
        }
        None
    }

    /// Recovers the state machine from a snapshot. It restores the log and the
    /// configuration of state machine.
    ///从快照中恢复状态机。 它还原日志和状态机的配置。
    pub fn restore(&mut self, snap: Snapshot) -> bool {
        if snap.get_metadata().get_index() < self.raft_log.committed {
            return false;
        }
        if let Some(b) = self.restore_raft(&snap) {
            return b;
        }
        //存储snapshot的数据到unstable
        self.raft_log.restore(snap);
        true
    }

    /// Check if there is any pending confchange.
    ///
    /// This method can be false positive.
    ///检查是否有任何pending的confchange。
     ///
     ///此方法可能为假positive。
    #[inline]
    pub fn has_pending_conf(&self) -> bool {
        self.pending_conf_index > self.raft_log.applied || self.pending_membership_change.is_some()
    }

    /// Specifies if the commit should be broadcast.
    ///指定是否应广播提交。
    pub fn should_bcast_commit(&self) -> bool {
        !self.skip_bcast_commit || self.has_pending_conf()
    }

    /// Indicates whether state machine can be promoted to leader,
    /// which is true when its own id is in progress list.
    ///指示是否可以将状态机提升为领导者，当其自身的ID处于进度列表中时为true。
    pub fn promotable(&self) -> bool {
        self.prs().voter_ids().contains(&self.id)
    }

    /// Propose that the peer group change its active set to a new set.
    ///提议对等组将其active set更改为new set。
    ///
    /// > **Note:** This is an experimental feature.
    ///
    /// ```rust
    /// use raft::{Raft, Config, storage::MemStorage, eraftpb::ConfState};
    /// let config = Config {
    ///     id: 1,
    ///     peers: vec![1],
    ///     ..Default::default()
    /// };
    /// let mut raft = Raft::new(&config, MemStorage::default()).unwrap();
    /// raft.become_candidate();
    /// raft.become_leader(); // It must be a leader!
    ///
    /// let mut conf = ConfState::default();
    /// conf.set_nodes(vec![1,2,3]);
    /// conf.set_learners(vec![4]);
    /// if let Err(e) = raft.propose_membership_change(conf) {
    ///     panic!("{}", e);
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// * This Peer is not leader.
    /// * `voters` and `learners` are not mutually exclusive.
    /// * `voters` is empty.
    /// *此对等方不是领导者。
     /// *`voters`和`learners`不是互斥的。
     /// *`voters`是空的。
    pub fn propose_membership_change(&mut self, config: impl Into<Configuration>) -> Result<()> {
        if self.state != StateRole::Leader {
            return Err(Error::InvalidState(self.state));
        }
        let config = config.into();
        config.valid()?;
        debug!(
            "Replicating SetNodes with voters ({:?}), learners ({:?}).",
            config.voters(),
            config.learners()
        );
        let destination_index = self.raft_log.last_index() + 1;
        // Prep a configuration change to append.
        //准备要进行的配置更改。
        let mut conf_change = ConfChange::new();
        conf_change.set_change_type(ConfChangeType::BeginMembershipChange);
        conf_change.set_configuration(config.into());
        conf_change.set_start_index(destination_index);
        let data = protobuf::Message::write_to_bytes(&conf_change)?;
        let mut entry = Entry::new();
        entry.set_entry_type(EntryType::EntryConfChange);
        entry.set_data(data);
        let mut message = Message::new();
        message.set_msg_type(MessageType::MsgPropose);
        message.set_from(self.id);
        message.set_index(destination_index);
        message.set_entries(RepeatedField::from_vec(vec![entry]));
        // `append_entry` sets term, index for us.
        //`append_entry`为我们设置条件，索引。
        self.step(message)?;
        Ok(())
    }

    /// # Errors
    ///
    /// * `id` is already a voter.
    /// * `id` is already a learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    /// *`id`已经是投票者。
     /// *`id`已经是一个学习者。
     /// *会员资格有待更改。 （参见`is_in_membership_change（）`）
    fn add_voter_or_learner(&mut self, id: u64, learner: bool) -> Result<()> {
        debug!(
            "Adding node (learner: {}) with ID {} to peers.",
            learner, id
        );

        let result = if learner {
            //如果是个learner则构建一个leader 添加到progressSet中的leaders中
            let progress = Progress::new(self.raft_log.last_index() + 1, self.max_inflight);
            self.mut_prs().insert_learner(id, progress)
        } else if self.prs().learner_ids().contains(&id) {
            //如果learner已经包含该id则提升leader地位到voter
            self.mut_prs().promote_learner(id)
        } else {
            //初次添加非learner则添加到voter中
            let progress = Progress::new(self.raft_log.last_index() + 1, self.max_inflight);
            self.mut_prs().insert_voter(id, progress)
        };

        if let Err(e) = result {
            error!("{}", e);
            return Err(e);
        }
        //如果添加的是自己，则修改自己的learner状态
        if self.id == id {
            self.is_learner = learner
        };
        // When a node is first added/promoted, we should mark it as recently active.
        // Otherwise, check_quorum may cause us to step down if it is invoked
        // before the added node has a chance to commuicate with us.
        //首次添加/升级节点时，我们应将其标记为最近处于活动状态。
        //否则，如果在添加的节点有机会与我们通信之前调用check_quorum，可能会导致我们退出。
        self.mut_prs().get_mut(id).unwrap().recent_active = true;
        result
    }

    /// Adds a new node to the cluster.
    ///向集群添加一个新节点。
    ///
    /// # Errors
    ///
    /// * `id` is already a voter.
    /// * `id` is already a learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    /// *`id`已经是投票者。
     /// *`id`已经是一个学习者。
     /// *会员资格有待更改。 （参见`is_in_membership_change（）`）
    pub fn add_node(&mut self, id: u64) -> Result<()> {
        self.add_voter_or_learner(id, false)
    }

    /// Adds a learner node.
    ///添加一个学习者节点。
    ///
    /// # Errors
    ///
    /// * `id` is already a voter.
    /// * `id` is already a learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    /// *`id`已经是投票者。
     /// *`id`已经是一个学习者。
     /// *会员资格有待更改。 （参见`is_in_membership_change（）`）
    pub fn add_learner(&mut self, id: u64) -> Result<()> {
        self.add_voter_or_learner(id, true)
    }

    /// Removes a node from the raft.
    ///从木筏中删除一个节点。
    ///
    /// # Errors
    ///
    /// * `id` is not a voter or learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    /// *`id`不是投票者或学习者。
     /// *会员资格有待更改。 （参见`is_in_membership_change（）`）
    pub fn remove_node(&mut self, id: u64) -> Result<()> {
        self.mut_prs().remove(id)?;

        // do not try to commit or abort transferring if there are no nodes in the cluster.
        //如果集群中没有节点，请勿尝试提交或中止传输。
        if self.prs().voter_ids().is_empty() && self.prs().learner_ids().is_empty() {
            return Ok(());
        }

        // The quorum size is now smaller, so see if any pending entries can
        // be committed.
        //现在，仲裁大小较小，因此请查看是否可以提交任何pending条目。
        if self.maybe_commit() {
            //广播commitid
            self.bcast_append();
        }
        // If the removed node is the lead_transferee, then abort the leadership transferring.
        //如果删除的节点是lead_transferee，则中止领导转移。
        if self.state == StateRole::Leader && self.lead_transferee == Some(id) {
            self.abort_leader_transfer();
        }

        Ok(())
    }

    /// Updates the progress of the learner or voter.
    ///更新学习者或投票者的进度。
    pub fn set_progress(&mut self, id: u64, matched: u64, next_idx: u64, is_learner: bool) {
        let mut p = Progress::new(next_idx, self.max_inflight);
        p.matched = matched;
        if is_learner {
            if let Err(e) = self.mut_prs().insert_learner(id, p) {
                panic!("{}", e);
            }
        } else if let Err(e) = self.mut_prs().insert_voter(id, p) {
            panic!("{}", e);
        }
    }

    /// Takes the progress set (destructively turns to `None`).
    ///获取进度集（破坏性地变为“ None”）。
    pub fn take_prs(&mut self) -> ProgressSet {
        self.prs.take().unwrap()
    }

    /// Sets the progress set.
    ///设置进度集。
    pub fn set_prs(&mut self, prs: ProgressSet) {
        self.prs = Some(prs);
    }

    /// Returns a read-only reference to the progress set.
    ///返回对进度集的只读引用。
    pub fn prs(&self) -> &ProgressSet {
        self.prs.as_ref().unwrap()
    }

    /// Returns a mutable reference to the progress set.
    ///返回对进度集的可变引用。
    pub fn mut_prs(&mut self) -> &mut ProgressSet {
        self.prs.as_mut().unwrap()
    }

    // TODO: revoke pub when there is a better way to test.
    /// For a given hardstate, load the state into self.
    ///对于给定的硬状态，将状态加载到self中。
    pub fn load_state(&mut self, hs: &HardState) {
        if hs.get_commit() < self.raft_log.committed || hs.get_commit() > self.raft_log.last_index()
        {
            panic!(
                "{} hs.commit {} is out of range [{}, {}]",
                self.tag,
                hs.get_commit(),
                self.raft_log.committed,
                self.raft_log.last_index()
            )
        }
        self.raft_log.committed = hs.get_commit();
        self.term = hs.get_term();
        self.vote = hs.get_vote();
    }

    /// `pass_election_timeout` returns true iff `election_elapsed` is greater
    /// than or equal to the randomized election timeout in
    /// [`election_timeout`, 2 * `election_timeout` - 1].
    ///如果`election_elapsed`大于或等于[`election_timeout`，2 *`election_timeout`-1]中的随机选举超时，则`pass_election_timeout`返回true。
    pub fn pass_election_timeout(&self) -> bool {
        self.election_elapsed >= self.randomized_election_timeout
    }

    /// Regenerates and stores the election timeout.
    ///重新 生成 并 存储 选举超时。
    pub fn reset_randomized_election_timeout(&mut self) {
        let prev_timeout = self.randomized_election_timeout;
        //新的election_timeout
        let timeout =
            rand::thread_rng().gen_range(self.min_election_timeout, self.max_election_timeout);
        debug!(
            "{} reset election timeout {} -> {} at {}",
            self.tag, prev_timeout, timeout, self.election_elapsed
        );
        self.randomized_election_timeout = timeout;
    }

    // check_quorum_active returns true if the quorum is active from
    // the view of the local raft state machine. Otherwise, it returns
    // false.
    // check_quorum_active also resets all recent_active to false.
    // check_quorum_active can only called by leader.
    //如果从本地筏状态机的视图来看仲裁是活动的，则check_quorum_active返回true。 否则，它返回false。
    //check_quorum_active还将所有last_active重置为false。
    //check_quorum_active只能由领导者调用。
    fn check_quorum_active(&mut self) -> bool {
        let self_id = self.id;
        //recent active的member 是否超过半数，
        self.mut_prs().quorum_recently_active(self_id)
    }

    /// Issues a message to timeout immediately.
    ///立即发出超时消息。
    pub fn send_timeout_now(&mut self, to: u64) {
        let msg = new_message(to, MessageType::MsgTimeoutNow, None);
        self.send(msg);
    }

    /// Stops the tranfer of a leader.
    ///停止转移领导者。
    pub fn abort_leader_transfer(&mut self) {
        self.lead_transferee = None;
    }

    /// Determine if the Raft is in a transition state under Joint Consensus.
    ///确定“筏”是否在“联合共识”下处于过渡状态。
    pub fn is_in_membership_change(&self) -> bool {
        self.prs().is_in_membership_change()
    }
}
