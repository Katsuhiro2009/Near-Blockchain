use near_primitives::time::Clock;
use std::collections::{hash_map::Entry, HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use actix::dev::{MessageResponse, ResponseChannel};
use actix::{Actor, Message};
use borsh::{BorshDeserialize, BorshSerialize};
use cached::{Cached, SizedCache};
use conqueue::{QueueReceiver, QueueSender};
use near_crypto::{KeyType, SecretKey, Signature};
#[cfg(feature = "test_features")]
use serde::{Deserialize, Serialize};
use tracing::warn;

use near_primitives::hash::CryptoHash;
use near_primitives::network::{AnnounceAccount, PeerId};
use near_primitives::types::AccountId;
use near_store::{ColAccountAnnouncements, Store};

use crate::routing::route_back_cache::RouteBackCache;
use crate::PeerInfo;
use crate::{
    types::{PeerIdOrHash, Ping, Pong},
    utils::cache_to_hashmap,
};

const ANNOUNCE_ACCOUNT_CACHE_SIZE: usize = 10_000;
const ROUTE_BACK_CACHE_SIZE: u64 = 100_000;
const ROUTE_BACK_CACHE_EVICT_TIMEOUT: Duration = Duration::from_millis(120_000);
const ROUTE_BACK_CACHE_REMOVE_BATCH: u64 = 100;
const PING_PONG_CACHE_SIZE: usize = 1_000;
const ROUND_ROBIN_MAX_NONCE_DIFFERENCE_ALLOWED: usize = 10;
const ROUND_ROBIN_NONCE_CACHE_SIZE: usize = 10_000;
/// Routing table will clean edges if there is at least one node that is not reachable
/// since `SAVE_PEERS_MAX_TIME` seconds. All peers disconnected since `SAVE_PEERS_AFTER_TIME`
/// seconds will be removed from cache and persisted in disk.
pub const SAVE_PEERS_MAX_TIME: Duration = Duration::from_secs(7_200);
pub const DELETE_PEERS_AFTER_TIME: Duration = Duration::from_secs(3_600);
/// Graph implementation supports up to 128 peers.
pub const MAX_NUM_PEERS: usize = 128;

/// Information that will be ultimately used to create a new edge.
/// It contains nonce proposed for the edge with signature from peer.
#[derive(Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq, Debug, Default)]
pub struct EdgeInfo {
    pub nonce: u64,
    pub signature: Signature,
}

impl EdgeInfo {
    pub fn new(peer0: &PeerId, peer1: &PeerId, nonce: u64, secret_key: &SecretKey) -> Self {
        let data = if peer0 < peer1 {
            EdgeInner::build_hash(&peer0, &peer1, nonce)
        } else {
            EdgeInner::build_hash(&peer1, &peer0, nonce)
        };

        let signature = secret_key.sign(data.as_ref());
        Self { nonce, signature }
    }
}

/// Status of the edge
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug, Hash)]
pub enum EdgeType {
    Added,
    Removed,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "test_features", derive(Serialize, Deserialize))]
pub struct Edge(pub Arc<EdgeInner>);

impl Edge {
    /// Create an addition edge.
    pub fn new(
        peer0: PeerId,
        peer1: PeerId,
        nonce: u64,
        signature0: Signature,
        signature1: Signature,
    ) -> Self {
        Edge(Arc::new(EdgeInner::new(peer0, peer1, nonce, signature0, signature1)))
    }

    pub fn make_fake_edge(peer0: PeerId, peer1: PeerId, nonce: u64) -> Self {
        Self(Arc::new(EdgeInner {
            key: (peer0, peer1),
            nonce,
            signature0: Signature::empty(KeyType::ED25519),
            signature1: Signature::empty(KeyType::ED25519),
            removal_info: None,
        }))
    }

    /// Build a new edge with given information from the other party.
    pub fn build_with_secret_key(
        peer0: PeerId,
        peer1: PeerId,
        nonce: u64,
        secret_key: &SecretKey,
        signature1: Signature,
    ) -> Self {
        let hash = if peer0 < peer1 {
            Self::build_hash(&peer0, &peer1, nonce)
        } else {
            Self::build_hash(&peer1, &peer0, nonce)
        };
        let signature0 = secret_key.sign(hash.as_ref());
        Self::new(peer0, peer1, nonce, signature0, signature1)
    }

    /// Build the hash of the edge given its content.
    /// It is important that peer0 < peer1 at this point.
    pub fn build_hash(peer0: &PeerId, peer1: &PeerId, nonce: u64) -> CryptoHash {
        CryptoHash::hash_borsh(&(peer0, peer1, nonce))
    }

    pub fn make_key(peer0: PeerId, peer1: PeerId) -> (PeerId, PeerId) {
        if peer0 < peer1 {
            (peer0, peer1)
        } else {
            (peer1, peer0)
        }
    }

    /// Helper function when adding a new edge and we receive information from new potential peer
    /// to verify the signature.
    pub fn partial_verify(peer0: PeerId, peer1: PeerId, edge_info: &EdgeInfo) -> bool {
        let pk = peer1.public_key();
        let data = if peer0 < peer1 {
            Edge::build_hash(&peer0, &peer1, edge_info.nonce)
        } else {
            Edge::build_hash(&peer1, &peer0, edge_info.nonce)
        };
        edge_info.signature.verify(data.as_ref(), &pk)
    }

    /// Next nonce of valid addition edge.
    pub fn next_nonce(nonce: u64) -> u64 {
        if nonce % 2 == 1 {
            nonce + 2
        } else {
            nonce + 1
        }
    }
}

impl std::ops::Deref for Edge {
    type Target = EdgeInner;

    fn deref(&self) -> &EdgeInner {
        &self.0
    }
}

/// Edge object. Contains information relative to a new edge that is being added or removed
/// from the network. This is the information that is required.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "test_features", derive(Serialize, Deserialize))]
pub struct EdgeInner {
    /// Since edges are not directed `key.0 < peer1` should hold.
    pub key: (PeerId, PeerId),
    /// Nonce to keep tracking of the last update on this edge.
    /// It must be even
    pub nonce: u64,
    /// Signature from parties validating the edge. These are signature of the added edge.
    pub signature0: Signature,
    pub signature1: Signature,
    /// Info necessary to declare an edge as removed.
    /// The bool says which party is removing the edge: false for Peer0, true for Peer1
    /// The signature from the party removing the edge.
    pub removal_info: Option<(bool, Signature)>,
}

impl EdgeInner {
    /// Create an addition edge.
    pub fn new(
        peer0: PeerId,
        peer1: PeerId,
        nonce: u64,
        signature0: Signature,
        signature1: Signature,
    ) -> Self {
        let (peer0, signature0, peer1, signature1) = if peer0 < peer1 {
            (peer0, signature0, peer1, signature1)
        } else {
            (peer1, signature1, peer0, signature0)
        };

        Self { key: (peer0, peer1), nonce, signature0, signature1, removal_info: None }
    }

    pub fn key(&self) -> &(PeerId, PeerId) {
        &self.key
    }

    pub fn to_simple_edge(&self) -> SimpleEdge {
        SimpleEdge::new(self.key.0.clone(), self.key.1.clone(), self.nonce)
    }

    /// Create the remove edge change from an added edge change.
    pub fn remove_edge(&self, my_peer_id: PeerId, sk: &SecretKey) -> Edge {
        assert_eq!(self.edge_type(), EdgeType::Added);
        let mut edge = self.clone();
        edge.nonce += 1;
        let me = edge.key.0 == my_peer_id;
        let hash = edge.hash();
        let signature = sk.sign(hash.as_ref());
        edge.removal_info = Some((me, signature));
        Edge(Arc::new(edge))
    }

    /// Build the hash of the edge given its content.
    /// It is important that peer0 < peer1 at this point.
    pub fn build_hash(peer0: &PeerId, peer1: &PeerId, nonce: u64) -> CryptoHash {
        debug_assert!(peer0 < peer1);
        CryptoHash::hash_borsh(&(peer0, peer1, &nonce))
    }

    fn hash(&self) -> CryptoHash {
        Edge::build_hash(&self.key.0, &self.key.1, self.nonce)
    }

    fn prev_hash(&self) -> CryptoHash {
        Edge::build_hash(&self.key.0, &self.key.1, self.nonce - 1)
    }

    pub fn verify(&self) -> bool {
        if self.key.0 > self.key.1 {
            return false;
        }

        match self.edge_type() {
            EdgeType::Added => {
                let data = self.hash();

                self.removal_info.is_none()
                    && self.signature0.verify(data.as_ref(), &self.key.0.public_key())
                    && self.signature1.verify(data.as_ref(), &self.key.1.public_key())
            }
            EdgeType::Removed => {
                // nonce should be an even positive number
                if self.nonce == 0 {
                    return false;
                }

                // Check referring added edge is valid.
                let add_hash = self.prev_hash();
                if !self.signature0.verify(add_hash.as_ref(), &self.key.0.public_key())
                    || !self.signature1.verify(add_hash.as_ref(), &self.key.1.public_key())
                {
                    return false;
                }

                if let Some((party, signature)) = &self.removal_info {
                    let peer = if *party { &self.key.0 } else { &self.key.1 };
                    let del_hash = self.hash();
                    signature.verify(del_hash.as_ref(), &peer.public_key())
                } else {
                    false
                }
            }
        }
    }

    pub fn get_pair(&self) -> &(PeerId, PeerId) {
        &self.key
    }

    /// It will be considered as a new edge if the nonce is odd, otherwise it is canceling the
    /// previous edge.
    pub fn edge_type(&self) -> EdgeType {
        if self.nonce % 2 == 1 {
            EdgeType::Added
        } else {
            EdgeType::Removed
        }
    }
    /// Next nonce of valid addition edge.
    pub fn next(&self) -> u64 {
        Edge::next_nonce(self.nonce)
    }

    pub fn contains_peer(&self, peer_id: &PeerId) -> bool {
        self.key.0 == *peer_id || self.key.1 == *peer_id
    }

    /// Find a peer id in this edge different from `me`.
    pub fn other(&self, me: &PeerId) -> Option<&PeerId> {
        if self.key.0 == *me {
            Some(&self.key.1)
        } else if self.key.1 == *me {
            Some(&self.key.0)
        } else {
            None
        }
    }
}

/// Represents edge between two nodes. Unlike `Edge` it doesn't contain signatures.
#[derive(Hash, Clone, Eq, PartialEq, Debug)]
#[cfg_attr(feature = "test_features", derive(Serialize, Deserialize))]
pub struct SimpleEdge {
    key: (PeerId, PeerId),
    nonce: u64,
}

impl SimpleEdge {
    pub fn new(peer0: PeerId, peer1: PeerId, nonce: u64) -> SimpleEdge {
        let (peer0, peer1) = Edge::make_key(peer0, peer1);
        SimpleEdge { key: (peer0, peer1), nonce }
    }

    pub fn key(&self) -> &(PeerId, PeerId) {
        &self.key
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    pub fn edge_type(&self) -> EdgeType {
        if self.nonce % 2 == 1 {
            EdgeType::Added
        } else {
            EdgeType::Removed
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, PartialEq, Eq, Clone, Debug, Copy)]
pub struct ValidIBFLevel(pub u64);

/// We create IbfSets of various sizes from 2^10+2 up to 2^17+2. Those constants specify valid ranges.
pub const MIN_IBF_LEVEL: ValidIBFLevel = ValidIBFLevel(10);
pub const MAX_IBF_LEVEL: ValidIBFLevel = ValidIBFLevel(17);

/// Represents IbfLevel from 10 to 17.
impl ValidIBFLevel {
    pub fn inc(&self) -> Option<ValidIBFLevel> {
        if self.0 + 1 >= MIN_IBF_LEVEL.0 && self.0 + 1 <= MAX_IBF_LEVEL.0 {
            Some(ValidIBFLevel(self.0 + 1))
        } else {
            None
        }
    }

    pub fn is_valid(&self) -> bool {
        return self.0 >= MIN_IBF_LEVEL.0 && self.0 <= MAX_IBF_LEVEL.0;
    }
}

#[derive(Debug)]
#[cfg_attr(feature = "test_features", derive(Serialize))]
pub struct PeerRequestResult {
    pub peers: Vec<PeerInfo>,
}

impl<A, M> MessageResponse<A, M> for PeerRequestResult
where
    A: Actor,
    M: Message<Result = PeerRequestResult>,
{
    fn handle<R: ResponseChannel<M>>(self, _: &mut A::Context, tx: Option<R>) {
        if let Some(tx) = tx {
            tx.send(self)
        }
    }
}

#[derive(MessageResponse, Debug)]
#[cfg_attr(feature = "test_features", derive(Serialize))]
pub struct GetRoutingTableResult {
    pub edges_info: Vec<SimpleEdge>,
}

pub struct EdgeVerifierHelper {
    /// Shared version of edges_info used by multiple threads
    pub edges_info_shared: Arc<Mutex<HashMap<(PeerId, PeerId), u64>>>,
    /// Queue of edges verified, but not added yes
    pub edges_to_add_receiver: QueueReceiver<Edge>,
    pub edges_to_add_sender: QueueSender<Edge>,
}

impl Default for EdgeVerifierHelper {
    fn default() -> Self {
        let (tx, rx) = conqueue::Queue::unbounded::<Edge>();
        Self {
            edges_info_shared: Default::default(),
            edges_to_add_sender: tx,
            edges_to_add_receiver: rx,
        }
    }
}

pub struct RoutingTableView {
    /// PeerId associated with this instance.
    my_peer_id: PeerId,
    /// PeerId associated for every known account id.
    account_peers: SizedCache<AccountId, AnnounceAccount>,
    /// Active PeerId that are part of the shortest path to each PeerId.
    pub peer_forwarding: Arc<HashMap<PeerId, Vec<PeerId>>>,
    /// Store last update for known edges. This is limited to list of adjacent edges to `my_peer_id`.
    pub local_edges_info: HashMap<(PeerId, PeerId), Edge>,
    /// Hash of messages that requires routing back to respective previous hop.
    pub route_back: RouteBackCache,
    /// Access to store on disk
    store: Arc<Store>,
    /// Number of times each active connection was used to route a message.
    /// If there are several options use route with minimum nonce.
    /// New routes are added with minimum nonce.
    route_nonce: SizedCache<PeerId, usize>,
    /// Ping received by nonce.
    ping_info: SizedCache<usize, (Ping, usize)>,
    /// Ping received by nonce.
    pong_info: SizedCache<usize, (Pong, usize)>,
    /// List of pings sent for which we haven't received any pong yet.
    waiting_pong: SizedCache<PeerId, SizedCache<usize, Instant>>,
    /// Last nonce sent to each peer through pings.
    last_ping_nonce: SizedCache<PeerId, usize>,
}

#[derive(Debug)]
pub enum FindRouteError {
    Disconnected,
    PeerNotFound,
    AccountNotFound,
    RouteBackNotFound,
}

impl RoutingTableView {
    pub fn new(my_peer_id: PeerId, store: Arc<Store>) -> Self {
        // Find greater nonce on disk and set `component_nonce` to this value.

        Self {
            my_peer_id,
            account_peers: SizedCache::with_size(ANNOUNCE_ACCOUNT_CACHE_SIZE),
            peer_forwarding: Default::default(),
            local_edges_info: Default::default(),
            route_back: RouteBackCache::new(
                ROUTE_BACK_CACHE_SIZE,
                ROUTE_BACK_CACHE_EVICT_TIMEOUT,
                ROUTE_BACK_CACHE_REMOVE_BATCH,
            ),
            store,
            route_nonce: SizedCache::with_size(ROUND_ROBIN_NONCE_CACHE_SIZE),
            ping_info: SizedCache::with_size(PING_PONG_CACHE_SIZE),
            pong_info: SizedCache::with_size(PING_PONG_CACHE_SIZE),
            waiting_pong: SizedCache::with_size(PING_PONG_CACHE_SIZE),
            last_ping_nonce: SizedCache::with_size(PING_PONG_CACHE_SIZE),
        }
    }

    /// Checks whenever edge is newer than the one we already have.
    /// Works only for local edges.
    pub fn is_local_edge_newer(&self, key: &(PeerId, PeerId), nonce: u64) -> bool {
        assert!(key.0 == self.my_peer_id || key.1 == self.my_peer_id);
        self.local_edges_info.get(&key).map_or(0, |x| x.nonce) < nonce
    }

    pub fn reachable_peers(&self) -> impl Iterator<Item = &PeerId> {
        self.peer_forwarding.keys()
    }

    /// Find peer that is connected to `source` and belong to the shortest path
    /// from `source` to `peer_id`.
    pub fn find_route_from_peer_id(&mut self, peer_id: &PeerId) -> Result<PeerId, FindRouteError> {
        if let Some(routes) = self.peer_forwarding.get(&peer_id).cloned() {
            if routes.is_empty() {
                return Err(FindRouteError::Disconnected);
            }

            // Strategy similar to Round Robin. Select node with least nonce and send it. Increase its
            // nonce by one. Additionally if the difference between the highest nonce and the lowest
            // nonce is greater than some threshold increase the lowest nonce to be at least
            // max nonce - threshold.
            let nonce_peer = routes
                .iter()
                .map(|peer_id| {
                    (self.route_nonce.cache_get(&peer_id).cloned().unwrap_or(0), peer_id)
                })
                .collect::<Vec<_>>();

            // Neighbor with minimum and maximum nonce respectively.
            let min_v = nonce_peer.iter().min().cloned().unwrap();
            let max_v = nonce_peer.into_iter().max().unwrap();

            if min_v.0 + ROUND_ROBIN_MAX_NONCE_DIFFERENCE_ALLOWED < max_v.0 {
                self.route_nonce
                    .cache_set(min_v.1.clone(), max_v.0 - ROUND_ROBIN_MAX_NONCE_DIFFERENCE_ALLOWED);
            }

            let next_hop = min_v.1;
            let nonce = self.route_nonce.cache_get(&next_hop).cloned();
            self.route_nonce.cache_set(next_hop.clone(), nonce.map_or(1, |nonce| nonce + 1));
            Ok(next_hop.clone())
        } else {
            Err(FindRouteError::PeerNotFound)
        }
    }

    pub fn find_route(&mut self, target: &PeerIdOrHash) -> Result<PeerId, FindRouteError> {
        match target {
            PeerIdOrHash::PeerId(peer_id) => self.find_route_from_peer_id(&peer_id),
            PeerIdOrHash::Hash(hash) => {
                self.fetch_route_back(hash.clone()).ok_or(FindRouteError::RouteBackNotFound)
            }
        }
    }

    /// Find peer that owns this AccountId.
    pub fn account_owner(&mut self, account_id: &AccountId) -> Result<PeerId, FindRouteError> {
        self.get_announce(account_id)
            .map(|announce_account| announce_account.peer_id)
            .ok_or_else(|| FindRouteError::AccountNotFound)
    }

    /// Add (account id, peer id) to routing table.
    /// Note: There is at most on peer id per account id.
    pub fn add_account(&mut self, announce_account: AnnounceAccount) {
        let account_id = announce_account.account_id.clone();
        self.account_peers.cache_set(account_id.clone(), announce_account.clone());

        // Add account to store
        let mut update = self.store.store_update();
        if let Err(e) = update
            .set_ser(ColAccountAnnouncements, account_id.as_ref().as_bytes(), &announce_account)
            .and_then(|_| update.commit())
        {
            warn!(target: "network", "Error saving announce account to store: {:?}", e);
        }
    }

    // TODO(MarX, #1694): Allow one account id to be routed to several peer id.
    pub fn contains_account(&mut self, announce_account: &AnnounceAccount) -> bool {
        self.get_announce(&announce_account.account_id).map_or(false, |current_announce_account| {
            current_announce_account.epoch_id == announce_account.epoch_id
        })
    }

    pub fn remove_edges(&mut self, edges: &Vec<Edge>) {
        for edge in edges.iter() {
            assert!(edge.key.0 == self.my_peer_id || edge.key.1 == self.my_peer_id);
            let key = (edge.key.0.clone(), edge.key.1.clone());
            self.local_edges_info.remove(&key);
        }
    }

    pub fn add_route_back(&mut self, hash: CryptoHash, peer_id: PeerId) -> bool {
        self.route_back.insert(hash, peer_id)
    }

    // Find route back with given hash and removes it from cache.
    fn fetch_route_back(&mut self, hash: CryptoHash) -> Option<PeerId> {
        self.route_back.remove(&hash)
    }

    pub fn compare_route_back(&mut self, hash: CryptoHash, peer_id: &PeerId) -> bool {
        self.route_back.get(&hash).map_or(false, |value| value == peer_id)
    }

    pub fn add_ping(&mut self, ping: Ping) {
        let cnt = self.ping_info.cache_get(&(ping.nonce as usize)).map(|v| v.1).unwrap_or(0);

        self.ping_info.cache_set(ping.nonce as usize, (ping, cnt + 1));
    }

    /// Return time of the round trip of ping + pong
    pub fn add_pong(&mut self, pong: Pong) -> Option<f64> {
        let mut res = None;

        if let Some(nonces) = self.waiting_pong.cache_get_mut(&pong.source) {
            res = nonces.cache_remove(&(pong.nonce as usize)).and_then(|sent| {
                Some(Clock::instant().saturating_duration_since(sent).as_secs_f64() * 1000f64)
            });
        }

        let cnt = self.pong_info.cache_get(&(pong.nonce as usize)).map(|v| v.1).unwrap_or(0);

        self.pong_info.cache_set(pong.nonce as usize, (pong, (cnt + 1)));

        res
    }

    // for unit tests
    pub fn sending_ping(&mut self, nonce: usize, target: PeerId) {
        let entry = if let Some(entry) = self.waiting_pong.cache_get_mut(&target) {
            entry
        } else {
            self.waiting_pong.cache_set(target.clone(), SizedCache::with_size(10));
            self.waiting_pong.cache_get_mut(&target).unwrap()
        };

        entry.cache_set(nonce, Clock::instant());
    }

    pub fn get_ping(&mut self, peer_id: PeerId) -> usize {
        if let Some(entry) = self.last_ping_nonce.cache_get_mut(&peer_id) {
            *entry += 1;
            *entry - 1
        } else {
            self.last_ping_nonce.cache_set(peer_id, 1);
            0
        }
    }

    // for unit tests
    pub fn fetch_ping_pong(
        &self,
    ) -> (HashMap<usize, (Ping, usize)>, HashMap<usize, (Pong, usize)>) {
        (cache_to_hashmap(&self.ping_info), cache_to_hashmap(&self.pong_info))
    }

    pub fn info(&mut self) -> RoutingTableInfo {
        let account_peers = self
            .get_announce_accounts()
            .into_iter()
            .map(|announce_account| (announce_account.account_id, announce_account.peer_id))
            .collect();
        RoutingTableInfo { account_peers, peer_forwarding: self.peer_forwarding.clone() }
    }

    /// Public interface for `account_peers`
    ///
    /// Get keys currently on cache.
    pub fn get_accounts_keys(&mut self) -> Vec<AccountId> {
        self.account_peers.key_order().cloned().collect()
    }

    /// Get announce accounts on cache.
    pub fn get_announce_accounts(&mut self) -> Vec<AnnounceAccount> {
        self.account_peers.value_order().cloned().collect()
    }

    /// Get number of accounts
    pub fn get_announce_accounts_size(&mut self) -> usize {
        self.account_peers.cache_size()
    }

    /// Get account announce from
    pub fn get_announce(&mut self, account_id: &AccountId) -> Option<AnnounceAccount> {
        if let Some(announce_account) = self.account_peers.cache_get(&account_id) {
            Some(announce_account.clone())
        } else {
            self.store
                .get_ser(ColAccountAnnouncements, account_id.as_ref().as_bytes())
                .and_then(|res: Option<AnnounceAccount>| {
                    if let Some(announce_account) = res {
                        self.add_account(announce_account.clone());
                        Ok(Some(announce_account))
                    } else {
                        Ok(None)
                    }
                })
                .unwrap_or_else(|e| {
                    warn!(target: "network", "Error loading announce account from store: {:?}", e);
                    None
                })
        }
    }

    pub fn get_edge(&self, peer0: PeerId, peer1: PeerId) -> Option<&Edge> {
        assert!(peer0 == self.my_peer_id || peer1 == self.my_peer_id);

        let key = Edge::make_key(peer0, peer1);
        self.local_edges_info.get(&key)
    }
}
#[derive(Debug)]
pub struct RoutingTableInfo {
    pub account_peers: HashMap<AccountId, PeerId>,
    pub peer_forwarding: Arc<HashMap<PeerId, Vec<PeerId>>>,
}

/// `Graph` is used to compute `peer_routing`, which contains information how to route messages to
/// all known peers. That is, for each `peer`, we get a sub-set of peers to which we are connected
/// to that are on the shortest path between us as destination `peer`.
#[derive(Clone)]
pub struct Graph {
    /// peer_id of current peer
    my_peer_id: PeerId,
    /// `id` as integer corresponding to `my_peer_id`.
    /// We use u32 to reduce both improve performance, and reduce memory usage.
    source_id: u32,
    /// Mapping from `PeerId` to `id`
    p2id: HashMap<PeerId, u32>,
    /// List of existing `PeerId`s
    id2p: Vec<PeerId>,
    /// Which ids are currently in use
    used: Vec<bool>,
    /// List of unused peer ids
    unused: Vec<u32>,
    /// Compressed adjacency table, we use 32 bit integer as ids instead of using full `PeerId`.
    /// This is undirected graph, we store edges in both directions.
    adjacency: Vec<Vec<u32>>,

    /// Total number of edges used for stats.
    total_active_edges: u64,
}

impl Graph {
    pub fn new(source: PeerId) -> Self {
        let mut res = Self {
            my_peer_id: source.clone(),
            source_id: 0,
            p2id: HashMap::default(),
            id2p: Vec::default(),
            used: Vec::default(),
            unused: Vec::default(),
            adjacency: Vec::default(),
            total_active_edges: 0,
        };
        res.id2p.push(source.clone());
        res.adjacency.push(Vec::default());
        res.p2id.insert(source, res.source_id);
        res.used.push(true);

        res
    }

    pub fn my_peer_id(&self) -> &PeerId {
        &self.my_peer_id
    }

    pub fn total_active_edges(&self) -> u64 {
        self.total_active_edges
    }

    // Compute number of active edges. We divide by 2 to remove duplicates.
    pub fn compute_total_active_edges(&self) -> u64 {
        let result: u64 = self.adjacency.iter().map(|x| x.len() as u64).sum();
        assert_eq!(result % 2, 0);
        result / 2
    }

    fn contains_edge(&self, peer0: &PeerId, peer1: &PeerId) -> bool {
        if let Some(&id0) = self.p2id.get(&peer0) {
            if let Some(&id1) = self.p2id.get(&peer1) {
                return self.adjacency[id0 as usize].contains(&id1);
            }
        }
        false
    }

    fn remove_if_unused(&mut self, id: u32) {
        let entry = &self.adjacency[id as usize];

        if entry.is_empty() && id != self.source_id {
            self.used[id as usize] = false;
            self.unused.push(id);
            self.p2id.remove(&self.id2p[id as usize]);
        }
    }

    fn get_id(&mut self, peer: &PeerId) -> u32 {
        match self.p2id.entry(peer.clone()) {
            Entry::Occupied(occupied) => *occupied.get(),
            Entry::Vacant(vacant) => {
                let val = if let Some(val) = self.unused.pop() {
                    assert!(!self.used[val as usize]);
                    assert!(self.adjacency[val as usize].is_empty());
                    self.id2p[val as usize] = peer.clone();
                    self.used[val as usize] = true;
                    val
                } else {
                    let val = self.id2p.len() as u32;
                    self.id2p.push(peer.clone());
                    self.used.push(true);
                    self.adjacency.push(Vec::default());
                    val
                };

                vacant.insert(val);
                val
            }
        }
    }

    pub fn add_edge(&mut self, peer0: &PeerId, peer1: &PeerId) {
        assert_ne!(peer0, peer1);
        if !self.contains_edge(peer0, peer1) {
            let id0 = self.get_id(peer0);
            let id1 = self.get_id(peer1);

            self.adjacency[id0 as usize].push(id1);
            self.adjacency[id1 as usize].push(id0);

            self.total_active_edges += 1;
        }
    }

    pub fn remove_edge(&mut self, peer0: &PeerId, peer1: &PeerId) {
        assert_ne!(peer0, peer1);
        if self.contains_edge(&peer0, &peer1) {
            let id0 = self.get_id(&peer0);
            let id1 = self.get_id(&peer1);

            self.adjacency[id0 as usize].retain(|&x| x != id1);
            self.adjacency[id1 as usize].retain(|&x| x != id0);

            self.remove_if_unused(id0);
            self.remove_if_unused(id1);

            self.total_active_edges -= 1;
        }
    }

    /// Compute for every node `u` on the graph (other than `source`) which are the neighbors of
    /// `sources` which belong to the shortest path from `source` to `u`. Nodes that are
    /// not connected to `source` will not appear in the result.
    pub fn calculate_distance(&self) -> HashMap<PeerId, Vec<PeerId>> {
        // TODO add removal of unreachable nodes

        let mut queue = VecDeque::new();

        let nodes = self.id2p.len();
        let mut distance: Vec<i32> = vec![-1; nodes];
        let mut routes: Vec<u128> = vec![0; nodes];

        distance[self.source_id as usize] = 0;

        {
            let neighbors = &self.adjacency[self.source_id as usize];
            for (id, &neighbor) in neighbors.iter().enumerate().take(MAX_NUM_PEERS) {
                queue.push_back(neighbor);
                distance[neighbor as usize] = 1;
                routes[neighbor as usize] = 1u128 << id;
            }
        }

        while let Some(cur_peer) = queue.pop_front() {
            let cur_distance = distance[cur_peer as usize];

            for &neighbor in &self.adjacency[cur_peer as usize] {
                if distance[neighbor as usize] == -1 {
                    distance[neighbor as usize] = cur_distance + 1;
                    queue.push_back(neighbor);
                }
                // If this edge belong to a shortest path, all paths to
                // the closer nodes are also valid for the current node.
                if distance[neighbor as usize] == cur_distance + 1 {
                    routes[neighbor as usize] |= routes[cur_peer as usize];
                }
            }
        }

        self.compute_result(&mut routes, &distance)
    }

    fn compute_result(&self, routes: &[u128], distance: &[i32]) -> HashMap<PeerId, Vec<PeerId>> {
        let mut res = HashMap::with_capacity(routes.len());

        let neighbors = &self.adjacency[self.source_id as usize];
        let mut unreachable_nodes = 0;

        for (key, &cur_route) in routes.iter().enumerate() {
            if distance[key] == -1 && self.used[key] {
                unreachable_nodes += 1;
            }
            if key as u32 == self.source_id
                || distance[key] == -1
                || cur_route == 0u128
                || !self.used[key]
            {
                continue;
            }
            let mut peer_set: Vec<PeerId> = Vec::with_capacity(cur_route.count_ones() as usize);

            for (id, &neighbor) in neighbors.iter().enumerate().take(MAX_NUM_PEERS) {
                if (cur_route & (1u128 << id)) != 0 {
                    peer_set.push(self.id2p[neighbor as usize].clone());
                };
            }
            res.insert(self.id2p[key].clone(), peer_set);
        }
        if unreachable_nodes > 1000 {
            warn!("We store more than 1000 unreachable nodes: {}", unreachable_nodes);
        }
        res
    }
}

#[cfg(test)]
mod test {
    use crate::routing::routing::Graph;
    use crate::test_utils::{expected_routing_tables, random_peer_id};

    #[test]
    fn graph_contains_edge() {
        let source = random_peer_id();

        let node0 = random_peer_id();
        let node1 = random_peer_id();

        let mut graph = Graph::new(source.clone());

        assert_eq!(graph.contains_edge(&source, &node0), false);
        assert_eq!(graph.contains_edge(&source, &node1), false);
        assert_eq!(graph.contains_edge(&node0, &node1), false);
        assert_eq!(graph.contains_edge(&node1, &node0), false);

        graph.add_edge(&node0, &node1);

        assert_eq!(graph.contains_edge(&source, &node0), false);
        assert_eq!(graph.contains_edge(&source, &node1), false);
        assert_eq!(graph.contains_edge(&node0, &node1), true);
        assert_eq!(graph.contains_edge(&node1, &node0), true);

        graph.remove_edge(&node1, &node0);

        assert_eq!(graph.contains_edge(&node0, &node1), false);
        assert_eq!(graph.contains_edge(&node1, &node0), false);

        assert_eq!(0, graph.total_active_edges() as usize);
        assert_eq!(0, graph.compute_total_active_edges() as usize);
    }

    #[test]
    fn graph_distance0() {
        let source = random_peer_id();
        let node0 = random_peer_id();

        let mut graph = Graph::new(source.clone());
        graph.add_edge(&source, &node0);
        graph.remove_edge(&source, &node0);
        graph.add_edge(&source, &node0);

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![(node0.clone(), vec![node0.clone()])],
        ));

        assert_eq!(1, graph.total_active_edges() as usize);
        assert_eq!(1, graph.compute_total_active_edges() as usize);
    }

    #[test]
    fn graph_distance1() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(&nodes[0], &nodes[1]);
        graph.add_edge(&nodes[2], &nodes[1]);
        graph.add_edge(&nodes[1], &nodes[2]);

        assert!(expected_routing_tables(graph.calculate_distance(), vec![]));

        assert_eq!(2, graph.total_active_edges() as usize);
        assert_eq!(2, graph.compute_total_active_edges() as usize);
    }

    #[test]
    fn graph_distance2() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(&nodes[0], &nodes[1]);
        graph.add_edge(&nodes[2], &nodes[1]);
        graph.add_edge(&nodes[1], &nodes[2]);
        graph.add_edge(&source, &nodes[0]);

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![
                (nodes[0].clone(), vec![nodes[0].clone()]),
                (nodes[1].clone(), vec![nodes[0].clone()]),
                (nodes[2].clone(), vec![nodes[0].clone()]),
            ],
        ));

        assert_eq!(3, graph.total_active_edges() as usize);
        assert_eq!(3, graph.compute_total_active_edges() as usize);
    }

    #[test]
    fn graph_distance3() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(&nodes[0], &nodes[1]);
        graph.add_edge(&nodes[2], &nodes[1]);
        graph.add_edge(&nodes[0], &nodes[2]);
        graph.add_edge(&source, &nodes[0]);
        graph.add_edge(&source, &nodes[1]);

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![
                (nodes[0].clone(), vec![nodes[0].clone()]),
                (nodes[1].clone(), vec![nodes[1].clone()]),
                (nodes[2].clone(), vec![nodes[0].clone(), nodes[1].clone()]),
            ],
        ));

        assert_eq!(5, graph.total_active_edges() as usize);
        assert_eq!(5, graph.compute_total_active_edges() as usize);
    }

    /// Test the following graph
    ///     0 - 3 - 6
    ///   /   x   x
    /// s - 1 - 4 - 7
    ///   \   x   x
    ///     2 - 5 - 8
    ///
    ///    9 - 10 (Dummy edge disconnected)
    ///
    /// There is a shortest path to nodes [3..9) going through 0, 1, and 2.
    #[test]
    fn graph_distance4() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..11).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        for i in 0..3 {
            graph.add_edge(&source, &nodes[i]);
        }

        for level in 0..2 {
            for i in 0..3 {
                for j in 0..3 {
                    graph.add_edge(&nodes[level * 3 + i], &nodes[level * 3 + 3 + j]);
                }
            }
        }

        // Dummy edge.
        graph.add_edge(&nodes[9], &nodes[10]);

        let mut next_hops: Vec<_> =
            (0..3).map(|i| (nodes[i].clone(), vec![nodes[i].clone()])).collect();
        let target: Vec<_> = (0..3).map(|i| nodes[i].clone()).collect();

        for i in 3..9 {
            next_hops.push((nodes[i].clone(), target.clone()));
        }

        assert!(expected_routing_tables(graph.calculate_distance(), next_hops));

        assert_eq!(22, graph.total_active_edges() as usize);
        assert_eq!(22, graph.compute_total_active_edges() as usize);
    }
}