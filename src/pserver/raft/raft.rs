use crate::pserver::raft::state_machine::*;
use crate::util::entity::Partition;
use crate::util::{coding::*, config, entity::*, error::*};
use jimraft::{
    error::RResult, raft::LogReader, CmdResult, ConfigChange, NodeResolver, NodeResolverCallback,
    Peer, PeerType, Raft, RaftOptions, RaftServer, RaftServerOptions, Snapshot, StateMachine,
    StateMachineCallback,
};

use crate::client::meta_client::MetaClient;
use std::boxed::Box;
use std::collections::HashMap;
use std::mem;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::runtime::Builder;

pub struct JimRaftServer {
    conf: Arc<config::Config>,
    pub node_id: u64,
    raft_server: RaftServer,
}

impl JimRaftServer {
    pub fn get_instance(conf: Arc<config::Config>, node_id: u64) -> Arc<JimRaftServer> {
        static mut SERVER: Option<Arc<JimRaftServer>> = None;

        unsafe {
            // use mut static variable in Rust is unsafe
            SERVER
                .get_or_insert_with(|| {
                    // instance singleton object
                    Arc::new(Self::create_raft_server(conf, node_id))
                })
                .clone()
        }
    }

    fn create_raft_server(conf: Arc<config::Config>, node_id: u64) -> Self {
        let server_ops: RaftServerOptions = RaftServerOptions::new();
        let nr_callback: NodeResolverCallback = NodeResolverCallback {
            target: Box::new(SimpleNodeResolver::new(conf.clone())),
        };
        server_ops.set_node_resolver(nr_callback);
        server_ops.set_node_id(node_id);
        server_ops.set_tick_interval(conf.ps.rs.tick_interval);
        server_ops.set_election_tick(conf.ps.rs.election_tick);
        server_ops.set_transport_inprocess_use(conf.ps.rs.transport_inprocess_use);
        Self {
            conf: conf,
            node_id: node_id,
            raft_server: RaftServer::new(server_ops),
        }
    }

    pub fn create_raft(&self, partition: Arc<Partition>) -> ASResult<Raft> {
        let (collection_id, partition_id) = (partition.collection_id, partition.id);
        let options = RaftOptions::new();
        let (current_peer_id, peers) = self.create_peers(partition);
        let callback: StateMachineCallback = StateMachineCallback {
            target: Box::new(SimpleStateMachine {
                persisted: 0,
                peer_id: current_peer_id,
            }),
        };
        options.set_id(merge_u32(partition.collection_id, partition.id));
        options.set_peers(peers);
        options.set_state_machine(callback);
        options.set_use_memoray_storage(true);
        convert(self.raft_server.create_raft(&options))
    }

    fn create_peers(&self, partition: Arc<Partition>) -> (u64, Vec<Peer>) {
        let mut current_peer_id = 0;
        let mut peers: Vec<Peer> = vec![];
        for replica in partition.replicas.iter() {
            if replica.node == self.node_id as u32 {
                current_peer_id = replica.peer;
            }
            let mut peer_type;
            match replica.replica_type {
                ReplicaType::LEARNER => peer_type = PeerType::LEARNER,
                ReplicaType::NORMAL => peer_type = PeerType::NORMAL,
            }
            let peer: Peer = Peer {
                type_: peer_type,
                node_id: replica.node as u64,
                id: replica.peer,
            };
            peers.push(peer);
        }
        (current_peer_id, peers)
    }
}

pub struct SimpleNodeResolver {
    pub meta_client: Arc<MetaClient>,
    pub cache: RwLock<HashMap<u64, String>>,
}

impl SimpleNodeResolver {
    pub fn new(conf: Arc<config::Config>) -> Self {
        Self {
            meta_client: Arc::new(MetaClient::new(conf)),
            cache: RwLock::new(HashMap::new()),
        }
    }
}
impl NodeResolver for SimpleNodeResolver {
    fn get_node_address(&self, node_id: u64) -> RResult<String> {
        if let Some(addr) = self.cache.read().unwrap().get(&node_id) {
            return Ok(addr.to_string());
        }

        let v = self
            .cache
            .write()
            .unwrap()
            .entry(node_id)
            .or_insert(|| -> String {
                let mut rt = Builder::new()
                    .basic_scheduler()
                    .enable_all()
                    .build()
                    .unwrap();
                match rt.block_on(self.meta_client.get_server_addr_by_id(node_id)) {
                    Ok(addr) => {
                        let parts: Vec<&str> = addr.split("_").collect();
                        parts[0].to_string()
                    }
                    Err(_) => Err(jimraft::error::err_str(
                        &format!("get node address error,node id[{}] ", node_id).as_str(),
                    )),
                }
            });
        return Ok(v.to_string());
    }
}

pub struct RaftEngine {
    pub partition: Arc<Partition>,
    pub raft: Raft,
}

impl RaftEngine {
    pub fn new(partition: Arc<Partition>, raft: Raft) -> Self {
        Self {
            partition: partition,
            raft: raft,
        }
    }

    pub fn append<'a, T: 'static>(&self, event: Event, callback: T) -> ASResult<()>
    where
        T: AppendCallback + 'a,
    {
        let faced = AppendCallbackFaced {
            target: Box::new(callback),
        };
        unsafe {
            self.raft.propose(
                &EventCodec::encode(event),
                1,
                mem::transmute(Box::new(faced)),
            );
        }
        Ok(())
    }

    pub fn begin_read_log(&self, start_index: u64) -> ASResult<LogReader> {
        match self.raft.begin_read_log(start_index) {
            Ok(logger) => return Ok(logger),
            Err(e) => {
                return Err(err_box(format!(
                    "get raft logger failure. error:[{}]",
                    e.to_string()
                )))
            }
        }
    }
}