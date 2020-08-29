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

// Copyright 2016 The etcd Authors
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

use std::collections::VecDeque;

use eraftpb::Message;

use hashbrown::{HashMap, HashSet};

/// Determines the relative safety of and consistency of read only requests.
///确定只读请求的相对安全性和一致性。
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ReadOnlyOption {
    /// Safe guarantees the linearizability of the read only request by
    /// communicating with the quorum. It is the default and suggested option.
    /// /// Safe通过与仲裁(quorum法定人数)进行通信来保证只读请求的线性化。 这是默认的建议选项。
    Safe,
    /// LeaseBased ensures linearizability of the read only request by
    /// relying on the leader lease. It can be affected by clock drift.
    /// If the clock drift is unbounded, leader might keep the lease longer than it
    /// should (clock can move backward/pause without any bound). ReadIndex is not safe
    /// in that case.
    /// LeaseBased通过依赖于领导者租约来确保只读请求的线性化。
    ///它可能会受到时钟漂移的影响。
    ///如果时钟漂移不受限制，则领导者可能会将租约保留的时间延长（时钟可以向后/暂停无限制）。
    ///在这种情况下，ReadIndex是不安全的。
    LeaseBased,
}

impl Default for ReadOnlyOption {
    fn default() -> ReadOnlyOption {
        ReadOnlyOption::Safe
    }
}

/// ReadState provides state for read only query.
/// It's caller's responsibility to send MsgReadIndex first before getting
/// this state from ready. It's also caller's duty to differentiate if this
/// state is what it requests through request_ctx, e.g. given a unique id as
/// request_ctx.
/// ReadState提供只读查询的状态。
///在准备好此状态之前，呼叫者有责任先发送MsgReadIndex。
///呼叫者还有责任区分此状态是否是通过request_ctx请求的状态，例如 给定一个唯一的ID作为request_ctx。
#[derive(Default, Debug, PartialEq, Clone)]
pub struct ReadState {
    /// The index of the read state.
    ///读取状态的索引。
    pub index: u64,
    /// A datagram consisting of context about the request.
    ///由请求上下文组成的数据报。
    pub request_ctx: Vec<u8>,
}

//此结构用于记录执行读取请求前接收到其他节点的心跳的响应个数等内容
#[derive(Default, Debug, Clone)]
pub struct ReadIndexStatus {
    pub req: Message,
    pub index: u64,
    pub acks: HashSet<u64>,
}

#[derive(Default, Debug, Clone)]
pub struct ReadOnly {
    pub option: ReadOnlyOption,
    pub pending_read_index: HashMap<Vec<u8>, ReadIndexStatus>,
    pub read_index_queue: VecDeque<Vec<u8>>,
}

impl ReadOnly {
    pub fn new(option: ReadOnlyOption) -> ReadOnly {
        ReadOnly {
            option,
            pending_read_index: HashMap::default(),
            read_index_queue: VecDeque::new(),
        }
    }

    /// Adds a read only request into readonly struct.
    ///
    /// `index` is the commit index of the raft state machine when it received
    /// the read only request.
    ///
    /// `m` is the original read only request message from the local or remote node.
    ///向只读结构添加一个只读请求。
     ///`index`是raft状态机收到只读请求时的提交索引。
     ///`m'是来自本地或远程节点的原始只读请求消息。
    pub fn add_request(&mut self, index: u64, m: Message) {
        let ctx = {
            let key = m.get_entries()[0].get_data();
            //如果正在处理该key则直接返回
            if self.pending_read_index.contains_key(key) {
                return;
            }
            key.to_vec()
        };
        let status = ReadIndexStatus {
            req: m,//请求内容
            index,//已提交的index
            acks: HashSet::default(),//请求的ack类型
        };
        //将key及其请求状态建立映射
        self.pending_read_index.insert(ctx.clone(), status);
        //将key加入到队列
        self.read_index_queue.push_back(ctx);
    }

    /// Notifies the ReadOnly struct that the raft state machine received
    /// an acknowledgment of the heartbeat that attached with the read only request
    /// context.
    ///通知ReadOnly结构，raft状态机收到了附带只读请求上下文的心跳确认。
    pub fn recv_ack(&mut self, m: &Message) -> HashSet<u64> {
        match self.pending_read_index.get_mut(m.get_context()) {
            None => Default::default(),
            Some(rs) => {
                rs.acks.insert(m.get_from());
                // add one to include an ack from local node
                let mut set_with_self = HashSet::default();
                set_with_self.insert(m.get_to());
                rs.acks.union(&set_with_self).cloned().collect()
            }
        }
    }

    /// Advances the read only request queue kept by the ReadOnly struct.
    /// It dequeues the requests until it finds the read only request that has
    /// the same context as the given `m`.
    ///推进由ReadOnly结构保留的只读请求队列。 它使请求出队，直到找到与给定的m具有相同上下文的只读请求。
    pub fn advance(&mut self, m: &Message) -> Vec<ReadIndexStatus> {
        let mut rss = vec![];
        if let Some(i) = self.read_index_queue.iter().position(|x| {
            if !self.pending_read_index.contains_key(x) {
                panic!("cannot find correspond read state from pending map");
            }
            *x == m.get_context()
        }) {
            for _ in 0..=i {
                let rs = self.read_index_queue.pop_front().unwrap();
                let status = self.pending_read_index.remove(&rs).unwrap();
                rss.push(status);
            }
        }
        rss
    }

    /// Returns the context of the last pending read only request in ReadOnly struct.
    ///返回ReadOnly结构中最后一个挂起的只读请求的上下文。
    pub fn last_pending_request_ctx(&self) -> Option<Vec<u8>> {
        self.read_index_queue.back().cloned()
    }

    #[inline]
    pub fn pending_read_count(&self) -> usize {
        self.read_index_queue.len()
    }
}
