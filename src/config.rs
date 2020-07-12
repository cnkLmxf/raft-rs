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

pub use super::read_only::{ReadOnlyOption, ReadState};
use super::{
    errors::{Error, Result},
    INVALID_ID,
};

/// Config contains the parameters to start a raft.
/// Config包含启动raft的参数。
pub struct Config {
    /// The identity of the local raft. It cannot be 0, and must be unique in the group.
    ///本地筏的标识。 它不能为0，并且在group中必须唯一。
    pub id: u64,

    /// The IDs of all nodes (including self) in
    /// the raft cluster. It should only be set when starting a new
    /// raft cluster.
    /// Restarting raft from previous configuration will panic if
    /// peers is set.
    /// peer is private and only used for testing right now.
    ///筏群集中所有节点（包括自身）的ID。 仅在启动新的raft群集时才应设置它。
    ///如果设置了peers，则从先前的配置重新启动raft会panic。
    // peer是私有的，并且仅用于测试。
    pub peers: Vec<u64>,

    /// The IDs of all learner nodes (maybe include self if
    /// the local node is a learner) in the raft cluster.
    /// learners only receives entries from the leader node. It does not vote
    /// or promote itself.
    ///raft集群中所有learner节点的ID（如果本地节点是learner，则可能包括self）。
    ///learner仅从领导者节点接收条目。 它不会投票或自我提升。
    pub learners: Vec<u64>,

    /// The number of node.tick invocations that must pass between
    /// elections. That is, if a follower does not receive any message from the
    /// leader of current term before ElectionTick has elapsed, it will become
    /// candidate and start an election. election_tick must be greater than
    /// HeartbeatTick. We suggest election_tick = 10 * HeartbeatTick to avoid
    /// unnecessary leader switching
    ///必须在两次选举之间传递的node.tick调用数。
    ///也就是说，如果在ElectionTick过去之前，follower没有收到当前任职leader的任何消息，它将成为候选人并开始选举。
    ///election_tick必须大于HeartbeatTick。 我们建议election_tick = 10 * HeartbeatTick，以避免不必要的leader切换
    pub election_tick: usize,

    /// HeartbeatTick is the number of node.tick invocations that must pass between
    /// heartbeats. That is, a leader sends heartbeat messages to maintain its
    /// leadership every heartbeat ticks.
    /// HeartbeatTick是必须在两次心跳之间传递的node.tick调用数。
    ///也就是说，领导者发送心跳消息，以在每一个心跳跳动时保持其领导地位。
    pub heartbeat_tick: usize,

    /// Applied is the last applied index. It should only be set when restarting
    /// raft. raft will not return entries to the application smaller or equal to Applied.
    /// If Applied is unset when restarting, raft might return previous applied entries.
    /// This is a very application dependent configuration.
    /// Applied是最后应用的索引。 仅应在重新启动raft时设置。 raft不会将entry返回到小于或等于Applied的应用程序。
    ///如果重新启动时未设置“apply”，则raft可能会返回以前的applied entry。 这是一个非常依赖于应用程序的配置。
    pub applied: u64,

    /// Limit the max size of each append message. Smaller value lowers
    /// the raft recovery cost(initial probing and message lost during normal operation).
    /// On the other side, it might affect the throughput during normal replication.
    /// Note: math.MaxUusize64 for unlimited, 0 for at most one entry per message.
    ///限制每个附加消息的最大大小。 较小的值可降低raft的恢复成本（正常操作期间的初始探测和消息丢失）。
    ///另一方面，它可能会影响正常复制期间的吞吐量。 注意：math.MaxUusize64为无限制，0为每条消息最多一个entry。
    pub max_size_per_msg: u64,

    /// Limit the max number of in-flight append messages during optimistic
    /// replication phase. The application transportation layer usually has its own sending
    /// buffer over TCP/UDP. Set to avoid overflowing that sending buffer.
    ///限制乐观复制阶段的运行中附加消息的最大数量。 应用程序传输层通常在TCP / UDP上具有自己的发送缓冲区。 设置以避免溢出发送缓冲区。
    /// TODO: feedback to application to limit the proposal rate?
    /// TODO：向应用程序反馈以限制提案率？
    pub max_inflight_msgs: usize,

    /// Specify if the leader should check quorum activity. Leader steps down when
    /// quorum is not active for an electionTimeout.
    ///指定领导者是否应检查仲裁活动。 当仲裁未激活竞选超时时，领导者下台。
    pub check_quorum: bool,

    /// Enables the Pre-Vote algorithm described in raft thesis section
    /// 9.6. This prevents disruption when a node that has been partitioned away
    /// rejoins the cluster.
    ///启用raft论文9.6节中描述的预投票算法。 这样可以防止在已分区的节点重新加入群集时造成中断。
    pub pre_vote: bool,

    /// The range of election timeout. In some cases, we hope some nodes has less possibility
    /// to become leader. This configuration ensures that the randomized election_timeout
    /// will always be suit in [min_election_tick, max_election_tick).
    /// If it is 0, then election_tick will be chosen.
    ///选举超时的范围。 在某些情况下，我们希望某些节点成为领导者的可能性较小。
    ///此配置可确保随机化的election_timeout始终适合[min_election_tick，max_election_tick）。
    ///如果为0，则将选择eletement_tick。
    pub min_election_tick: usize,

    /// If it is 0, then 2 * election_tick will be chosen.
    ///如果为0，则将选择2 * election_tick。
    pub max_election_tick: usize,

    /// Choose the linearizability mode or the lease mode to read data. If you don’t care about the read consistency and want a higher read performance, you can use the lease mode.
    ///
    /// Setting this to `LeaseBased` requires `check_quorum = true`.
    ///选择线性化模式或lease模式以读取数据。 如果您不关心读取一致性并希望获得更高的读取性能，则可以使用lease(租借)模式。
    ///将此设置为`LeaseBased`需要`check_quorum = true`。
    pub read_only_option: ReadOnlyOption,

    /// Don't broadcast an empty raft entry to notify follower to commit an entry.
    /// This may make follower wait a longer time to apply an entry. This configuration
    /// May affect proposal forwarding and follower read.
    ///不要广播空的raft条目以通知关注者提交entry。
    ///这可能会使follower等待更长的时间才能apply entry。 此配置可能会影响提案转发和follower read。
    pub skip_bcast_commit: bool,

    /// A human-friendly tag used for logging.
    ///用于logging的人类友好tag。
    pub tag: String,
}

impl Default for Config {
    fn default() -> Self {
        const HEARTBEAT_TICK: usize = 2;
        Self {
            id: 0,
            peers: vec![],
            learners: vec![],
            election_tick: HEARTBEAT_TICK * 10,
            heartbeat_tick: HEARTBEAT_TICK,
            applied: 0,
            max_size_per_msg: 0,
            max_inflight_msgs: 256,
            check_quorum: false,
            pre_vote: false,
            min_election_tick: 0,
            max_election_tick: 0,
            read_only_option: ReadOnlyOption::Safe,
            skip_bcast_commit: false,
            tag: "".into(),
        }
    }
}

impl Config {
    /// Creates a new config.
    ///创建一个新的配置。
    pub fn new(id: u64) -> Self {
        Self {
            id,
            tag: format!("{}", id),
            ..Self::default()
        }
    }

    /// The minimum number of ticks before an election.
    ///选举前的最小刻度数。
    #[inline]
    pub fn min_election_tick(&self) -> usize {
        if self.min_election_tick == 0 {
            self.election_tick
        } else {
            self.min_election_tick
        }
    }

    /// The maximum number of ticks before an election.
    ///选举前的最大刻度数。
    #[inline]
    pub fn max_election_tick(&self) -> usize {
        if self.max_election_tick == 0 {
            2 * self.election_tick
        } else {
            self.max_election_tick
        }
    }

    /// Runs validations against the config.
    ///对配置运行验证。
    pub fn validate(&self) -> Result<()> {
        if self.id == INVALID_ID {
            return Err(Error::ConfigInvalid("invalid node id".to_owned()));
        }

        if self.heartbeat_tick == 0 {
            return Err(Error::ConfigInvalid(
                "heartbeat tick must greater than 0".to_owned(),
            ));
        }

        if self.election_tick <= self.heartbeat_tick {
            return Err(Error::ConfigInvalid(
                "election tick must be greater than heartbeat tick".to_owned(),
            ));
        }

        let min_timeout = self.min_election_tick();
        let max_timeout = self.max_election_tick();
        if min_timeout < self.election_tick {
            return Err(Error::ConfigInvalid(format!(
                "min election tick {} must not be less than election_tick {}",
                min_timeout, self.election_tick
            )));
        }

        if min_timeout >= max_timeout {
            return Err(Error::ConfigInvalid(format!(
                "min election tick {} should be less than max election tick {}",
                min_timeout, max_timeout
            )));
        }

        if self.max_inflight_msgs == 0 {
            return Err(Error::ConfigInvalid(
                "max inflight messages must be greater than 0".to_owned(),
            ));
        }

        if self.read_only_option == ReadOnlyOption::LeaseBased && !self.check_quorum {
            return Err(Error::ConfigInvalid(
                "read_only_option == LeaseBased requires check_quorum == true".into(),
            ));
        }

        Ok(())
    }
}
