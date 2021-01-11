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

use eraftpb::{ConfState, SnapshotMetadata};
use errors::{Error, Result};
use hashbrown::hash_map::DefaultHashBuilder;
use hashbrown::{HashMap, HashSet};
use std::cell::RefCell;
use std::cmp;

// Since it's an integer, it rounds for us.
//由于是整数，因此会四舍五入。
#[inline]
fn majority(total: usize) -> usize {
    (total / 2) + 1
}

/// The state of the progress.
///进度状态。
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ProgressState {
    /// Whether it's probing.
    ///是否在探测。
    Probe,
    /// Whether it's replicating.
    ///是否在复制。
    Replicate,
    /// Whethers it's a snapshot.
    ///是否是快照。
    Snapshot,
}

impl Default for ProgressState {
    fn default() -> ProgressState {
        ProgressState::Probe
    }
}

/// A Raft internal representation of a Configuration.
///
/// This is corollary to a ConfState, but optimized for `contains` calls.
/// config的Raft内部表示。
/// 这是ConfState的必然结果，但已针对“包含”调用进行了优化。
#[derive(Clone, Debug, Default, PartialEq, Getters)]
pub struct Configuration {
    /// The voter set.
    ///投票者集合。
    #[get = "pub"]
    voters: HashSet<u64>,
    /// The learner set.
    ///学习者集合。
    #[get = "pub"]
    learners: HashSet<u64>,
}

impl Configuration {
    /// Create a new configuration with the given configuration.
    ///使用给定的配置创建一个新的配置。
    pub fn new(
        voters: impl IntoIterator<Item = u64>,
        learners: impl IntoIterator<Item = u64>,
    ) -> Self {
        Self {
            voters: voters.into_iter().collect(),
            learners: learners.into_iter().collect(),
        }
    }
}

impl<Iter1, Iter2> From<(Iter1, Iter2)> for Configuration
where
    Iter1: IntoIterator<Item = u64>,
    Iter2: IntoIterator<Item = u64>,
{
    fn from((voters, learners): (Iter1, Iter2)) -> Self {
        Self {
            voters: voters.into_iter().collect(),
            learners: learners.into_iter().collect(),
        }
    }
}

impl From<ConfState> for Configuration {
    fn from(conf_state: ConfState) -> Self {
        Self {
            voters: conf_state.get_nodes().iter().cloned().collect(),
            learners: conf_state.get_learners().iter().cloned().collect(),
        }
    }
}

impl From<Configuration> for ConfState {
    fn from(conf: Configuration) -> Self {
        let mut state = ConfState::default();
        state.set_nodes(conf.voters.iter().cloned().collect());
        state.set_learners(conf.learners.iter().cloned().collect());
        state
    }
}

impl Configuration {
    fn with_capacity(voters: usize, learners: usize) -> Self {
        Self {
            voters: HashSet::with_capacity_and_hasher(voters, DefaultHashBuilder::default()),
            learners: HashSet::with_capacity_and_hasher(learners, DefaultHashBuilder::default()),
        }
    }

    /// Validates that the configuration is not problematic.
    ///
    /// Namely:
    /// * There can be no overlap of voters and learners.
    /// * There must be at least one voter.
    ///验证配置没有问题。
    ///即：
    /// *选民和学习者之间不能有重叠。
    /// *必须至少有一个选民。
    pub fn valid(&self) -> Result<()> {
        if let Some(id) = self.voters.intersection(&self.learners).next() {
            Err(Error::Exists(*id, "learners"))?;
        } else if self.voters.is_empty() {
            Err(Error::ConfigInvalid(
                "There must be at least one voter.".into(),
            ))?;
        }
        Ok(())
    }

    fn has_quorum(&self, potential_quorum: &HashSet<u64>) -> bool {
        self.voters.intersection(potential_quorum).count() >= majority(self.voters.len())
    }

    /// Returns whether or not the given `id` is a member of this configuration.
    ///返回给定的id是否是此配置的成员。
    pub fn contains(&self, id: u64) -> bool {
        self.voters.contains(&id) || self.learners.contains(&id)
    }
}

/// The status of an election according to a Candidate node.
///
/// This is returned by `progress_set.election_status(vote_map)`
///候选节点的选举状态。
///这是由`progress_set.election_status（vote_map）`返回的
#[derive(Clone, Copy, Debug)]
pub enum CandidacyStatus {
    /// The election has been won by this Raft.
    ///该raft赢得了选举。
    Elected,
    /// It is still possible to win the election.
    ///仍然有可能赢得选举。
    Eligible,
    /// It is no longer possible to win the election.
    ///不再可能赢得选举。
    Ineligible,
}

/// `ProgressSet` contains several `Progress`es,
/// which could be `Leader`, `Follower` and `Learner`.
///`ProgressSet`包含几个`Progress`es，可以是`Leader`，`Follower`和`Learner`。
#[derive(Default, Clone, Getters)]
pub struct ProgressSet {
    progress: HashMap<u64, Progress>,
    /// The current configuration state of the cluster.
    ///集群的当前配置状态。
    #[get = "pub"]
    configuration: Configuration,
    /// The pending configuration, which will be adopted after the Finalize entry is applied.
    ///待定配置，将在应用Finalize条目后采用。
    #[get = "pub"]
    next_configuration: Option<Configuration>,
    configuration_capacity: (usize, usize),
    // A preallocated buffer for sorting in the minimally_commited_index function.
    // You should not depend on these values unless you just set them.
    // We use a cell to avoid taking a `&mut self`.
    //用于在minimally_commited_index函数中排序的预分配缓冲区。
    //除非您仅设置它们，否则不应依赖这些值。 我们使用一个单元格来避免使用`＆mut self`。
    sort_buffer: RefCell<Vec<u64>>,
}

impl ProgressSet {
    /// Creates a new ProgressSet.
    ///创建一个新的ProgressSet。
    pub fn new() -> Self {
        Self::with_capacity(0, 0)
    }

    /// Create a progress set with the specified sizes already reserved.
    ///创建一个已保留指定大小的进度集。
    pub fn with_capacity(voters: usize, learners: usize) -> Self {
        ProgressSet {
            progress: HashMap::with_capacity_and_hasher(
                voters + learners,
                DefaultHashBuilder::default(),
            ),
            sort_buffer: RefCell::from(Vec::with_capacity(voters)),
            configuration_capacity: (voters, learners),
            configuration: Configuration::with_capacity(voters, learners),
            next_configuration: Option::default(),
        }
    }

    pub(crate) fn restore_snapmeta(
        meta: &SnapshotMetadata,
        next_idx: u64,
        max_inflight: usize,
    ) -> Self {
        let mut prs = ProgressSet::new();
        //初始的时候统一设置为最新的的index
        let pr = Progress::new(next_idx, max_inflight);
        meta.get_conf_state().get_nodes().iter().for_each(|id| {
            //设置progress
            prs.progress.insert(*id, pr.clone());
            //设置当前configuration的voter
            prs.configuration.voters.insert(*id);
        });
        meta.get_conf_state().get_learners().iter().for_each(|id| {
            //设置progress
            prs.progress.insert(*id, pr.clone());
          //设置当前configuration的learner
            prs.configuration.learners.insert(*id);
        });
        //存在membership change
        if meta.pending_membership_change_index != 0 {
            let mut next_configuration = Configuration::with_capacity(0, 0);
            meta.get_pending_membership_change()
                .get_nodes()
                .iter()
                .for_each(|id| {
                    prs.progress.insert(*id, pr.clone());
                  //设置新的configuration中的voters
                    next_configuration.voters.insert(*id);
                });
            meta.get_pending_membership_change()
                .get_learners()
                .iter()
                .for_each(|id| {
                    prs.progress.insert(*id, pr.clone());
                    //设置新的configuration中的learner
                    next_configuration.learners.insert(*id);
                });
            //设置新的configuration
            prs.next_configuration = Some(next_configuration);
        }
        prs.assert_progress_and_configuration_consistent();
        prs
    }

    /// Returns the status of voters.
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    ///返回选民的状态。
    /// **注意：**请勿将其用于多数/定额计算。 Raft节点可能正在过渡到新配置并具有两个qourum。 使用`has_quorum`代替。
    #[inline]
    pub fn voters(&self) -> impl Iterator<Item = (&u64, &Progress)> {
        let set = self.voter_ids();
        self.progress.iter().filter(move |(&k, _)| set.contains(&k))
    }

    /// Returns the status of learners.
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    ///返回learner的状态。
     /// **注意：**请勿将其用于多数/定额计算。 Raft节点可能正在过渡到新配置并具有两个qourum。 使用`has_quorum`代替。
    #[inline]
    pub fn learners(&self) -> impl Iterator<Item = (&u64, &Progress)> {
        let set = self.learner_ids();
        self.progress.iter().filter(move |(&k, _)| set.contains(&k))
    }

    /// Returns the mutable status of voters.
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    ///返回投票者的可变状态。
    /// **注意：**请勿将其用于多数/定额计算。 Raft节点可能正在过渡到新配置并具有两个qourum。 使用`has_quorum`代替。
    #[inline]
    pub fn voters_mut(&mut self) -> impl Iterator<Item = (&u64, &mut Progress)> {
        let ids = self.voter_ids();
        self.progress
            .iter_mut()
            .filter(move |(k, _)| ids.contains(k))
    }

    /// Returns the mutable status of learners.
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    ///返回learner的可变状态。
    /// **注意：**请勿将其用于多数/定额计算。 Raft节点可能正在过渡到新配置并具有两个qourum。 使用`has_quorum`代替。
    #[inline]
    pub fn learners_mut(&mut self) -> impl Iterator<Item = (&u64, &mut Progress)> {
        let ids = self.learner_ids();
        self.progress
            .iter_mut()
            .filter(move |(k, _)| ids.contains(k))
    }

    /// Returns the ids of all known voters.
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    ///返回所有已知选民的ID。
    /// **注意：**请勿将其用于多数/定额计算。 Raft节点可能正在过渡到新配置并具有两个qourum。 使用`has_quorum`代替。
    #[inline]
    pub fn voter_ids(&self) -> HashSet<u64> {
        match self.next_configuration {
            Some(ref next) => self
                .configuration
                .voters
                .union(&next.voters)
                .cloned()
                .collect::<HashSet<u64>>(),
            None => self.configuration.voters.clone(),
        }
    }

    /// Returns the ids of all known learners.
    /// 返回所有已知learner的ID。
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    #[inline]
    pub fn learner_ids(&self) -> HashSet<u64> {
        match self.next_configuration {
            Some(ref next) => self
                .configuration
                .learners
                .union(&next.learners)
                .cloned()
                .collect::<HashSet<u64>>(),
            None => self.configuration.learners.clone(),
        }
    }

    /// Grabs a reference to the progress of a node.
    ///获取对节点进度的引用。
    #[inline]
    pub fn get(&self, id: u64) -> Option<&Progress> {
        self.progress.get(&id)
    }

    /// Grabs a mutable reference to the progress of a node.
    ///获取对节点进度的可变引用。
    #[inline]
    pub fn get_mut(&mut self, id: u64) -> Option<&mut Progress> {
        self.progress.get_mut(&id)
    }

    /// Returns an iterator across all the nodes and their progress.
    /// 返回所有节点及其进度的迭代器。
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&u64, &Progress)> {
        self.progress.iter()
    }

    /// Returns a mutable iterator across all the nodes and their progress.
    /// 返回所有节点及其进度的可变迭代器。
    ///
    /// **Note:** Do not use this for majority/quorum calculation. The Raft node may be
    /// transitioning to a new configuration and have two qourums. Use `has_quorum` instead.
    #[inline]
    pub fn iter_mut(&mut self) -> impl ExactSizeIterator<Item = (&u64, &mut Progress)> {
        self.progress.iter_mut()
    }

    /// Adds a voter to the group.
    /// 将投票者添加到group中。
    ///
    /// # Errors
    ///
    /// * `id` is in the voter set.
    /// * `id` is in the learner set.
    /// * There is a pending membership change.
    /// *`id`在投票者集中。
    /// *`id`在学习者集中。
    /// *会员资格有待更改。
    pub fn insert_voter(&mut self, id: u64, pr: Progress) -> Result<()> {
        debug!("Inserting voter with id {}.", id);

        if self.learner_ids().contains(&id) {
            return Err(Error::Exists(id, "learners"));
        } else if self.voter_ids().contains(&id) {
            return Err(Error::Exists(id, "voters"));
        } else if self.is_in_membership_change() {
            return Err(Error::ViolatesContract(
                "There is a pending membership change.".into(),
            ));
        }

        self.configuration.voters.insert(id);
        self.progress.insert(id, pr);

        self.assert_progress_and_configuration_consistent();
        Ok(())
    }

    /// Adds a learner to the group.
    ///将学习者添加到group中。
    ///
    /// # Errors
    ///
    /// * `id` is in the voter set.
    /// * `id` is in the learner set.
    /// * There is a pending membership change.
    pub fn insert_learner(&mut self, id: u64, pr: Progress) -> Result<()> {
        debug!("Inserting learner with id {}.", id);

        if self.learner_ids().contains(&id) {
            return Err(Error::Exists(id, "learners"));
        } else if self.voter_ids().contains(&id) {
            return Err(Error::Exists(id, "voters"));
        } else if self.is_in_membership_change() {
            return Err(Error::ViolatesContract(
                "There is a pending membership change".into(),
            ));
        }

        self.configuration.learners.insert(id);
        self.progress.insert(id, pr);

        self.assert_progress_and_configuration_consistent();
        Ok(())
    }

    /// Removes the peer from the set of voters or learners.
    /// 从voter或learner中删除peer。
    ///
    /// # Errors
    ///
    /// * There is a pending membership change.
    pub fn remove(&mut self, id: u64) -> Result<Option<Progress>> {
        debug!("Removing peer with id {}.", id);

        if self.is_in_membership_change() {
            return Err(Error::ViolatesContract(
                "There is a pending membership change.".into(),
            ));
        }

        self.configuration.learners.remove(&id);
        self.configuration.voters.remove(&id);
        let removed = self.progress.remove(&id);

        self.assert_progress_and_configuration_consistent();
        Ok(removed)
    }

    /// Promote a learner to a peer.
    ///将learner提升为peer。
    pub fn promote_learner(&mut self, id: u64) -> Result<()> {
        debug!("Promote learner with id {}.", id);

        if self.is_in_membership_change() {
            return Err(Error::ViolatesContract(
                "There is a pending membership change.".into(),
            ));
        }

        if !self.configuration.learners.remove(&id) {
            // Wasn't already a learner. We can't promote what doesn't exist.
            //还不是学习者。 我们不能宣传不存在的东西。
            return Err(Error::NotExists(id, "learners"));
        }
        if !self.configuration.voters.insert(id) {
            // Already existed, the caller should know this was a noop.
            //已经存在，调用者应该知道这是noop。
            return Err(Error::Exists(id, "voters"));
        }

        self.assert_progress_and_configuration_consistent();
        Ok(())
    }

    #[inline(always)]
    fn assert_progress_and_configuration_consistent(&self) {
        debug_assert!(self
            .configuration
            .voters
            .union(&self.configuration.learners)
            .all(|v| self.progress.contains_key(v)));
        debug_assert!(self
            .progress
            .keys()
            .all(|v| self.configuration.learners.contains(v)
                || self.configuration.voters.contains(v)
                || self
                    .next_configuration
                    .as_ref()
                    .map_or(false, |c| c.learners.contains(v))
                || self
                    .next_configuration
                    .as_ref()
                    .map_or(false, |c| c.voters.contains(v))));
        assert_eq!(
            self.voter_ids().len() + self.learner_ids().len(),
            self.progress.len()
        );
    }

    /// Returns the maximal committed index for the cluster.
    /// 返回集群的最大提交索引。
    /// match过半的index都可作为已提交的index
    /// Eg. If the matched indexes are [2,2,2,4,5], it will return 2.
    pub fn maximal_committed_index(&self) -> u64 {
        let mut matched = self.sort_buffer.borrow_mut();
        matched.clear();
        self.configuration.voters().iter().for_each(|id| {
            let peer = &self.progress[id];
            matched.push(peer.matched);
        });
        // Reverse sort.
        matched.sort_by(|a, b| b.cmp(a));
        let mut mci = matched[matched.len() / 2];

        if let Some(next) = &self.next_configuration {
            matched.clear();
            next.voters().iter().for_each(|id| {
                let peer = &self.progress[id];
                matched.push(peer.matched);
            });
            // Reverse sort.
            matched.sort_by(|a, b| b.cmp(a));
            let next_mci = matched[matched.len() / 2];
            if next_mci < mci {
                mci = next_mci;
            }
        }
        mci
    }

    /// Returns the Candidate's eligibility in the current election.
    ///
    /// If it is still eligible, it should continue polling nodes and checking.
    /// Eventually, the election will result in this returning either `Elected`
    /// or `Ineligible`, meaning the election can be concluded.
    ///返回当前选举中候选人的资格。
    ///如果仍然符合条件，则应继续轮询节点并进行检查。 最终，选举将导致返回“当选”或“不合格”，这意味着可以结束选举。
    pub fn candidacy_status<'a>(
        &self,
        votes: impl IntoIterator<Item = (&'a u64, &'a bool)>,
    ) -> CandidacyStatus {
        //获取支持和返回的票数
        let (accepts, rejects) = votes.into_iter().fold(
            (HashSet::default(), HashSet::default()),
            |(mut accepts, mut rejects), (&id, &accepted)| {
                if accepted {
                    accepts.insert(id);
                } else {
                    rejects.insert(id);
                }
                (accepts, rejects)
            },
        );
      //如果支持过半，则返回elected
        match self.next_configuration {
            Some(ref next) => {
                if next.has_quorum(&accepts) && self.configuration.has_quorum(&accepts) {
                    return CandidacyStatus::Elected;
                } else if next.has_quorum(&rejects) || self.configuration.has_quorum(&rejects) {
                    return CandidacyStatus::Ineligible;
                }
            }
            None => {
                if self.configuration.has_quorum(&accepts) {
                    return CandidacyStatus::Elected;
                } else if self.configuration.has_quorum(&rejects) {
                    return CandidacyStatus::Ineligible;
                }
            }
        };

        CandidacyStatus::Eligible
    }

    /// Determines if the current quorum is active according to the this raft node.
    /// Doing this will set the `recent_active` of each peer to false.
    ///
    /// This should only be called by the leader.
    ///根据此raft节点确定当前仲裁是否处于活动状态。 这样做会将每个对等体的“recent_active”设置为false。
    ///这只能由领导者调用。
    pub fn quorum_recently_active(&mut self, perspective_of: u64) -> bool {
        let mut active = HashSet::default();
        for (&id, pr) in self.voters_mut() {
            if id == perspective_of {
                active.insert(id);
                continue;
            }
            if pr.recent_active {
                active.insert(id);
            }
            pr.recent_active = false;
        }
        for (&_id, pr) in self.learners_mut() {
            pr.recent_active = false;
        }
        //如果活跃的成员超过一半，并且如果有新configuration，并且活跃成员超过新配置的一半，则返回true
        self.configuration.has_quorum(&active) &&
            // If `next` is `None` we don't consider it, so just `true` it.
            self.next_configuration.as_ref().map(|next| next.has_quorum(&active)).unwrap_or(true)
    }

    /// Determine if a quorum is formed from the given set of nodes.
    ///
    /// This is the only correct way to verify you have reached a quorum for the whole group.
    ///确定是否由给定的节点集形成仲裁。
     ///
     ///这是验证您是否已达到整个组法定人数的唯一正确方法。
    #[inline]
    pub fn has_quorum(&self, potential_quorum: &HashSet<u64>) -> bool {
        self.configuration.has_quorum(potential_quorum)
            && self
                .next_configuration
                .as_ref()
                .map(|next| next.has_quorum(potential_quorum))
                // If `next` is `None` we don't consider it, so just `true` it.
                .unwrap_or(true)
    }

    /// Determine if the ProgressSet is represented by a transition state under Joint Consensus.
    ///确定ProgressSet是否由“联合共识”下的过渡状态表示。
    #[inline]
    pub fn is_in_membership_change(&self) -> bool {
        self.next_configuration.is_some()
    }

    /// Enter a joint consensus state to transition to the specified configuration.
    /// ///输入联合共识状态以过渡到指定的配置。
    ///
    /// The `next` provided should be derived from the `ConfChange` message. `progress` is used as
    /// a basis for created peer `Progress` values. You are only expected to set `ins` from the
    /// `raft.max_inflights` value.
    ///提供的`next`应从`ConfChange`消息中派生。 `progress`用作创建对等`Progress`值的基础。 只需要从raft.max_inflights值设置`ins`。
    ///
    /// Once this state is entered the leader should replicate the `ConfChange` message. After the
    /// majority of nodes, in both the current and the `next`, have committed the union state. At
    /// this point the leader can call `finalize_config_transition` and replicate a message
    /// commiting the change.
    ///一旦进入此状态，领导者应复制ConfChange消息。 在当前节点和下一个节点中的大多数节点提交联合状态之后。
    ///此时，领导者可以调用“ finalize_config_transition”并复制提交更改的消息。
    ///
    /// Valid transitions:
    /// 有效转换：
    /// * Non-existing -> Learner
    /// * Non-existing -> Voter
    /// * Learner -> Voter
    /// * Learner -> Non-existing
    /// * Voter -> Non-existing
    ///
    /// Errors:
    /// * Voter -> Learner
    /// * Member as voter and learner. 成员为选民和学习者。
    /// * Empty voter set. 空选民集
    pub(crate) fn begin_membership_change(
        &mut self,
        next: impl Into<Configuration>,
        mut progress: Progress,
    ) -> Result<()> {
        let next = next.into();
        next.valid()?;
        // Demotion check.
        //降级检查。
        if let Some(&demoted) = self
            .configuration
            .voters
            .intersection(&next.learners)
            .next()
        {
            return Err(Error::Exists(demoted, "learners"));
        }
        debug!(
            "Beginning membership change. End configuration will be {:?}",
            next
        );

        // When a peer is first added/promoted, we should mark it as recently active.
        // Otherwise, check_quorum may cause us to step down if it is invoked
        // before the added peer has a chance to communicate with us.
        //首次添加/升级peer时，我们应将其标记为最近活动状态。
        //否则，如果在添加的对等方有机会与我们通信之前调用check_quorum，可能会导致我们退出。
        progress.recent_active = true;
        progress.paused = false;
        for id in next.voters.iter().chain(&next.learners) {
            // Now we create progresses for any that do not exist.
            //现在，我们为所有不存在的进度创建进度。
            self.progress.entry(*id).or_insert_with(|| progress.clone());
        }
        self.next_configuration = Some(next);
        Ok(())
    }

    /// Finalizes the joint consensus state and transitions solely to the new state.
    ///
    /// This must be called only after calling `begin_membership_change` and after the majority
    /// of peers in both the `current` and the `next` state have commited the changes.
    ///最终确定联合共识状态，并仅过渡到新状态。
    ///只有在调用`begin_membership_change`之后，并且在`current`和`next`状态下的大多数对等体都提交了更改之后，才能调用此方法。
    pub fn finalize_membership_change(&mut self) -> Result<()> {
        let next = self.next_configuration.take();
        match next {
            None => Err(Error::NoPendingMembershipChange)?,
            Some(next) => {
                {
                    let pending = self
                        .configuration
                        .voters()
                        .difference(next.voters())
                        .chain(self.configuration.learners().difference(next.learners()))
                        .cloned();
                    for id in pending {
                        self.progress.remove(&id);
                    }
                }
                self.configuration = next;
                debug!(
                    "Finalizing membership change. Config is {:?}",
                    self.configuration
                );
            }
        }
        Ok(())
    }
}

/// The progress of catching up from a restart.
///从重启开始的进度。
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// How much state is matched.
    ///匹配多少状态。
    pub matched: u64,
    /// The next index to apply
    ///下一个要应用的索引
    pub next_idx: u64,
    /// When in ProgressStateProbe, leader sends at most one replication message
    /// per heartbeat interval. It also probes actual progress of the follower.
    ///
    /// When in ProgressStateReplicate, leader optimistically increases next
    /// to the latest entry sent after sending replication message. This is
    /// an optimized state for fast replicating log entries to the follower.
    ///
    /// When in ProgressStateSnapshot, leader should have sent out snapshot
    /// before and stop sending any replication message.
    ///在ProgressStateProbe中时，领导者每个心跳间隔最多发送一条复制消息。 它还探讨了关注者的实际进度。
    ///在处于ProgressStateReplicate时，领导者乐观地增加发送复制消息后发送的最新条目的旁边。 这是用于将日志条目快速复制到关注者的优化状态。
    ///在ProgressStateSnapshot中时，领导者应该之前已发送快照并停止发送任何复制消息。
    pub state: ProgressState,
    /// Paused is used in ProgressStateProbe.
    /// When Paused is true, raft should pause sending replication message to this peer.
    ///Paused 在ProgressStateProbe中使用。
     ///如果Paused为true，则raft应暂停向该对等方发送复制消息。
    pub paused: bool,
    /// This field is used in ProgressStateSnapshot.
    /// If there is a pending snapshot, the pendingSnapshot will be set to the
    /// index of the snapshot. If pendingSnapshot is set, the replication process of
    /// this Progress will be paused. raft will not resend snapshot until the pending one
    /// is reported to be failed.
    ///此字段在ProgressStateSnapshot中使用。
    ///如果存在一个pending快照，则pendingSnapshot将设置为快照的索引。
    ///如果设置了pendingsnapshot，则此进度的复制过程将被暂停。 在报告pending的快照失败之前，raft不会重新发送快照。
    pub pending_snapshot: u64,

    /// This is true if the progress is recently active. Receiving any messages
    /// from the corresponding follower indicates the progress is active.
    /// RecentActive can be reset to false after an election timeout.
    ///如果progress最近处于活动状态，则为true。 接收到来自相应关注者的任何消息表示进度处于活动状态。
    ///选举超时后，RecentActive可以重置为false。
    pub recent_active: bool,

    /// Inflights is a sliding window for the inflight messages.
    /// When inflights is full, no more message should be sent.
    /// When a leader sends out a message, the index of the last
    /// entry should be added to inflights. The index MUST be added
    /// into inflights in order.
    /// When a leader receives a reply, the previous inflights should
    /// be freed by calling inflights.freeTo.
    ///Inflights是Inflight消息的滑动窗口。
     ///当Inflights已满时，不应再发送任何消息。
     ///当领导者发送消息时，最后一个entry的索引应添加到Inflights中。 索引必须按顺序添加到Inflights中。
     ///当领导者收到回复时，应通过调用inflights.freeTo释放之前的Inflights。
    pub ins: Inflights,
}

impl Progress {
    /// Creates a new progress with the given settings.
    ///使用给定的设置创建新progresss。
    pub fn new(next_idx: u64, ins_size: usize) -> Self {
        Progress {
            matched: 0,
            next_idx,
            state: ProgressState::default(),
            paused: false,
            pending_snapshot: 0,
            recent_active: false,
            ins: Inflights::new(ins_size),
        }
    }

    fn reset_state(&mut self, state: ProgressState) {
        self.paused = false;
        self.pending_snapshot = 0;
        self.state = state;
        self.ins.reset();
    }

    pub(crate) fn reset(&mut self, next_idx: u64) {
        self.matched = 0;
        self.next_idx = next_idx;
        self.state = ProgressState::default();
        self.paused = false;
        self.pending_snapshot = 0;
        self.recent_active = false;
        debug_assert!(self.ins.cap() != 0);
        self.ins.reset();
    }

    /// Changes the progress to a probe.
    ///将进度更改为probe。
    pub fn become_probe(&mut self) {
        // If the original state is ProgressStateSnapshot, progress knows that
        // the pending snapshot has been sent to this peer successfully, then
        // probes from pendingSnapshot + 1.
        //如果原始状态为ProgressStateSnapshot，则progress知道已将挂起的快照成功发送到此对等方，然后从endingSnapshot + 1进行探测。
        if self.state == ProgressState::Snapshot {
            let pending_snapshot = self.pending_snapshot;
            self.reset_state(ProgressState::Probe);
            self.next_idx = cmp::max(self.matched + 1, pending_snapshot + 1);
        } else {
            self.reset_state(ProgressState::Probe);
            self.next_idx = self.matched + 1;
        }
    }

    /// Changes the progress to a Replicate.
    ///将progress更改为 Replicate
    pub fn become_replicate(&mut self) {
        self.reset_state(ProgressState::Replicate);
        self.next_idx = self.matched + 1;
    }

    /// Changes the progress to a snapshot.
    ///将progress更改为快照。
    pub fn become_snapshot(&mut self, snapshot_idx: u64) {
        self.reset_state(ProgressState::Snapshot);
        self.pending_snapshot = snapshot_idx;
    }

    /// Sets the snapshot to failure.
    ///将快照设置为失败。
    pub fn snapshot_failure(&mut self) {
        self.pending_snapshot = 0;
    }

    /// Unsets pendingSnapshot if Match is equal or higher than
    /// the pendingSnapshot
    ///如果Match等于或高于endingSnapshot，则取消设置pending快照
    pub fn maybe_snapshot_abort(&self) -> bool {
        self.state == ProgressState::Snapshot && self.matched >= self.pending_snapshot
    }

    /// Returns false if the given n index comes from an outdated message.
    /// Otherwise it updates the progress and returns true.
    ///如果给定的n索引来自过期消息，则返回false。 否则，它将更新progress并返回true。
    /// 更新progress的匹配index大小，后续可能会更新commit_idx
    pub fn maybe_update(&mut self, n: u64) -> bool {
        let need_update = self.matched < n;
        if need_update {
            self.matched = n;
            self.resume();
        };

        if self.next_idx < n + 1 {
            self.next_idx = n + 1
        }

        need_update
    }

    /// Optimistically advance the index
    ///乐观地提高索引
    pub fn optimistic_update(&mut self, n: u64) {
        self.next_idx = n + 1;
    }

    /// Returns false if the given index comes from an out of order message.
    /// Otherwise it decreases the progress next index to min(rejected, last)
    /// and returns true.
    ///如果给定索引来自乱序消息，则返回false。 否则，它将进度下一个索引减少为min（rejected，last）并返回true。
    pub fn maybe_decr_to(&mut self, rejected: u64, last: u64) -> bool {
        if self.state == ProgressState::Replicate {
            // the rejection must be stale if the progress has matched and "rejected"
            // is smaller than "match".
            //如果进度已匹配并且“rejected”小于“matched”，则rejection必须是过期的。
            if rejected <= self.matched {
                return false;
            }
            self.next_idx = self.matched + 1;
            return true;
        }

        // the rejection must be stale if "rejected" does not match next - 1
        //如果下一个与“ rejected”不匹配，则拒绝必须是过期的-1
        if self.next_idx == 0 || self.next_idx - 1 != rejected {
            return false;
        }

        self.next_idx = cmp::min(rejected, last + 1);
        if self.next_idx < 1 {
            self.next_idx = 1;
        }
        self.resume();
        true
    }

    /// Determine whether progress is paused.
    ///确定进度是否暂停。
    pub fn is_paused(&self) -> bool {
        match self.state {
            ProgressState::Probe => self.paused,
            ProgressState::Replicate => self.ins.full(),
            ProgressState::Snapshot => true,
        }
    }

    /// Resume progress
    ///恢复进度
    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Pause progress.
    ///暂停进度。
    pub fn pause(&mut self) {
        self.paused = true;
    }
}

/// A buffer of inflight messages.
///inflight消息的缓冲区。
#[derive(Debug, PartialEq)]
pub struct Inflights {
    // the starting index in the buffer
    //缓冲区中的起始索引
    start: usize,
    // number of inflights in the buffer
    //缓冲区中的inflights个数
    count: usize,

    // ring buffer
    //环形缓冲区
    buffer: Vec<u64>,
}

// The `buffer` must have it's capacity set correctly on clone, normally it does not.
//缓冲区必须在克隆上正确设置其容量，通常没有。
impl Clone for Inflights {
    fn clone(&self) -> Self {
        let mut buffer = self.buffer.clone();
        buffer.reserve(self.buffer.capacity() - self.buffer.len());
        Inflights {
            start: self.start,
            count: self.count,
            buffer,
        }
    }
}

impl Inflights {
    /// Creates a new buffer for inflight messages.
    ///为inflight消息创建一个新的缓冲区。
    pub fn new(cap: usize) -> Inflights {
        Inflights {
            buffer: Vec::with_capacity(cap),
            start: 0,
            count: 0,
        }
    }

    /// Returns true if the inflights is full.
    ///如果机舱已满，则返回true。
    pub fn full(&self) -> bool {
        self.count == self.cap()
    }

    /// The buffer capacity.
    ///缓冲区容量。
    pub fn cap(&self) -> usize {
        self.buffer.capacity()
    }

    /// Adds an inflight into inflights
    ///在infights添加inflight
    pub fn add(&mut self, inflight: u64) {
        if self.full() {
            panic!("cannot add into a full inflights")
        }

        let mut next = self.start + self.count;
        if next >= self.cap() {
            next -= self.cap();
        }
        assert!(next <= self.buffer.len());
        if next == self.buffer.len() {
            self.buffer.push(inflight);
        } else {
            self.buffer[next] = inflight;
        }
        self.count += 1;
    }

    /// Frees the inflights smaller or equal to the given `to` flight.
    ///释放小于或等于给定的“ to”航班的机翼。
    pub fn free_to(&mut self, to: u64) {
        if self.count == 0 || to < self.buffer[self.start] {
            // out of the left side of the window
            //在窗口的左侧
            return;
        }

        let mut i = 0usize;
        let mut idx = self.start;
        while i < self.count {
            if to < self.buffer[idx] {
                // found the first large inflight
                //找到第一个大inflight
                break;
            }

            // increase index and maybe rotate
            //增加索引并可能旋转
            idx += 1;
            if idx >= self.cap() {
                idx -= self.cap();
            }

            i += 1;
        }

        // free i inflights and set new start index
        //释放i inflights并设置新的开始索引
        self.count -= i;
        self.start = idx;
    }

    /// Frees the first buffer entry.
    ///释放第一个缓冲区entry。
    pub fn free_first_one(&mut self) {
        let start = self.buffer[self.start];
        self.free_to(start);
    }

    /// Frees all inflights.
    ///释放所有inflights。
    pub fn reset(&mut self) {
        self.count = 0;
        self.start = 0;
    }
}

#[cfg(test)]
mod test {
    use progress::Inflights;
    use setup_for_test;

    #[test]
    fn test_inflight_add() {
        setup_for_test();
        let mut inflight = Inflights::new(10);
        for i in 0..5 {
            inflight.add(i);
        }

        let wantin = Inflights {
            start: 0,
            count: 5,
            buffer: vec![0, 1, 2, 3, 4],
        };

        assert_eq!(inflight, wantin);

        for i in 5..10 {
            inflight.add(i);
        }

        let wantin2 = Inflights {
            start: 0,
            count: 10,
            buffer: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        };

        assert_eq!(inflight, wantin2);

        let mut inflight2 = Inflights::new(10);
        inflight2.start = 5;
        inflight2.buffer.extend_from_slice(&[0, 0, 0, 0, 0]);

        for i in 0..5 {
            inflight2.add(i);
        }

        let wantin21 = Inflights {
            start: 5,
            count: 5,
            buffer: vec![0, 0, 0, 0, 0, 0, 1, 2, 3, 4],
        };

        assert_eq!(inflight2, wantin21);

        for i in 5..10 {
            inflight2.add(i);
        }

        let wantin22 = Inflights {
            start: 5,
            count: 10,
            buffer: vec![5, 6, 7, 8, 9, 0, 1, 2, 3, 4],
        };

        assert_eq!(inflight2, wantin22);
    }

    #[test]
    fn test_inflight_free_to() {
        setup_for_test();
        let mut inflight = Inflights::new(10);
        for i in 0..10 {
            inflight.add(i);
        }

        inflight.free_to(4);

        let wantin = Inflights {
            start: 5,
            count: 5,
            buffer: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        };

        assert_eq!(inflight, wantin);

        inflight.free_to(8);

        let wantin2 = Inflights {
            start: 9,
            count: 1,
            buffer: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        };

        assert_eq!(inflight, wantin2);

        for i in 10..15 {
            inflight.add(i);
        }

        inflight.free_to(12);

        let wantin3 = Inflights {
            start: 3,
            count: 2,
            buffer: vec![10, 11, 12, 13, 14, 5, 6, 7, 8, 9],
        };

        assert_eq!(inflight, wantin3);

        inflight.free_to(14);

        let wantin4 = Inflights {
            start: 5,
            count: 0,
            buffer: vec![10, 11, 12, 13, 14, 5, 6, 7, 8, 9],
        };

        assert_eq!(inflight, wantin4);
    }

    #[test]
    fn test_inflight_free_first_one() {
        setup_for_test();
        let mut inflight = Inflights::new(10);
        for i in 0..10 {
            inflight.add(i);
        }

        inflight.free_first_one();

        let wantin = Inflights {
            start: 1,
            count: 9,
            buffer: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        };

        assert_eq!(inflight, wantin);
    }
}

// TODO: Reorganize this whole file into separate files.
// See https://github.com/pingcap/raft-rs/issues/125
#[cfg(test)]
mod test_progress_set {
    use hashbrown::HashSet;

    use {progress::Configuration, Progress, ProgressSet, Result};

    const CANARY: u64 = 123;

    #[test]
    fn test_insert_redundant_voter() -> Result<()> {
        let mut set = ProgressSet::default();
        let default_progress = Progress::new(0, 256);
        let mut canary_progress = Progress::new(0, 256);
        canary_progress.matched = CANARY;
        set.insert_voter(1, default_progress.clone())?;
        assert!(
            set.insert_voter(1, canary_progress).is_err(),
            "Should return an error on redundant insert."
        );
        assert_eq!(
            *set.get(1).expect("Should be inserted."),
            default_progress,
            "The ProgressSet was mutated in a `insert_voter` that returned error."
        );
        Ok(())
    }

    #[test]
    fn test_insert_redundant_learner() -> Result<()> {
        let mut set = ProgressSet::default();
        let default_progress = Progress::new(0, 256);
        let mut canary_progress = Progress::new(0, 256);
        canary_progress.matched = CANARY;
        set.insert_learner(1, default_progress.clone())?;
        assert!(
            set.insert_learner(1, canary_progress).is_err(),
            "Should return an error on redundant insert."
        );
        assert_eq!(
            *set.get(1).expect("Should be inserted."),
            default_progress,
            "The ProgressSet was mutated in a `insert_learner` that returned error."
        );
        Ok(())
    }

    #[test]
    fn test_insert_learner_that_is_voter() -> Result<()> {
        let mut set = ProgressSet::default();
        let default_progress = Progress::new(0, 256);
        let mut canary_progress = Progress::new(0, 256);
        canary_progress.matched = CANARY;
        set.insert_voter(1, default_progress.clone())?;
        assert!(
            set.insert_learner(1, canary_progress).is_err(),
            "Should return an error on invalid learner insert."
        );
        assert_eq!(
            *set.get(1).expect("Should be inserted."),
            default_progress,
            "The ProgressSet was mutated in a `insert_learner` that returned error."
        );
        Ok(())
    }

    #[test]
    fn test_insert_voter_that_is_learner() -> Result<()> {
        let mut set = ProgressSet::default();
        let default_progress = Progress::new(0, 256);
        let mut canary_progress = Progress::new(0, 256);
        canary_progress.matched = CANARY;
        set.insert_learner(1, default_progress.clone())?;
        assert!(
            set.insert_voter(1, canary_progress).is_err(),
            "Should return an error on invalid voter insert."
        );
        assert_eq!(
            *set.get(1).expect("Should be inserted."),
            default_progress,
            "The ProgressSet was mutated in a `insert_voter` that returned error."
        );
        Ok(())
    }

    #[test]
    fn test_promote_learner() -> Result<()> {
        let mut set = ProgressSet::default();
        let default_progress = Progress::new(0, 256);
        set.insert_voter(1, default_progress)?;
        let pre = set.get(1).expect("Should have been inserted").clone();
        assert!(
            set.promote_learner(1).is_err(),
            "Should return an error on invalid promote_learner."
        );
        assert!(
            set.promote_learner(2).is_err(),
            "Should return an error on invalid promote_learner."
        );
        assert_eq!(pre, *set.get(1).expect("Peer should not have been deleted"));
        Ok(())
    }

    #[test]
    fn test_membership_change_configuration_remove_voter() -> Result<()> {
        check_membership_change_configuration((vec![1, 2], vec![]), (vec![1], vec![]))
    }

    #[test]
    fn test_membership_change_configuration_remove_learner() -> Result<()> {
        check_membership_change_configuration((vec![1], vec![2]), (vec![1], vec![]))
    }

    #[test]
    fn test_membership_change_configuration_conflicting_sets() {
        assert!(
            check_membership_change_configuration((vec![1], vec![]), (vec![1], vec![1]),).is_err()
        )
    }

    #[test]
    fn test_membership_change_configuration_empty_sets() {
        assert!(check_membership_change_configuration((vec![], vec![]), (vec![], vec![])).is_err())
    }

    #[test]
    fn test_membership_change_configuration_empty_voters() {
        assert!(
            check_membership_change_configuration((vec![1], vec![]), (vec![], vec![]),).is_err()
        )
    }

    #[test]
    fn test_membership_change_configuration_add_voter() -> Result<()> {
        check_membership_change_configuration((vec![1], vec![]), (vec![1, 2], vec![]))
    }

    #[test]
    fn test_membership_change_configuration_add_learner() -> Result<()> {
        check_membership_change_configuration((vec![1], vec![]), (vec![1], vec![2]))
    }

    #[test]
    fn test_membership_change_configuration_promote_learner() -> Result<()> {
        check_membership_change_configuration((vec![1], vec![2]), (vec![1, 2], vec![]))
    }

    fn check_membership_change_configuration(
        start: (impl IntoIterator<Item = u64>, impl IntoIterator<Item = u64>),
        end: (impl IntoIterator<Item = u64>, impl IntoIterator<Item = u64>),
    ) -> Result<()> {
        let start_voters = start.0.into_iter().collect::<HashSet<u64>>();
        let start_learners = start.1.into_iter().collect::<HashSet<u64>>();
        let end_voters = end.0.into_iter().collect::<HashSet<u64>>();
        let end_learners = end.1.into_iter().collect::<HashSet<u64>>();
        let transition_voters = start_voters
            .union(&end_voters)
            .cloned()
            .collect::<HashSet<u64>>();
        let transition_learners = start_learners
            .union(&end_learners)
            .cloned()
            .collect::<HashSet<u64>>();

        let mut set = ProgressSet::default();
        let default_progress = Progress::new(0, 10);

        for starter in start_voters {
            set.insert_voter(starter, default_progress.clone())?;
        }
        for starter in start_learners {
            set.insert_learner(starter, default_progress.clone())?;
        }
        set.begin_membership_change(
            Configuration::new(end_voters.clone(), end_learners.clone()),
            default_progress,
        )?;
        assert!(set.is_in_membership_change());
        assert_eq!(
            set.voter_ids(),
            transition_voters,
            "Transition state voters inaccurate"
        );
        assert_eq!(
            set.learner_ids(),
            transition_learners,
            "Transition state learners inaccurate."
        );

        set.finalize_membership_change()?;
        assert!(!set.is_in_membership_change());
        assert_eq!(set.voter_ids(), end_voters, "End state voters inaccurate");
        assert_eq!(
            set.learner_ids(),
            end_learners,
            "End state learners inaccurate"
        );
        Ok(())
    }
}
