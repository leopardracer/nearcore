use crate::adapter::ShardsManagerRequestFromClient;
use crate::client::ShardsManagerResponse;
use crate::shards_manager_actor::ShardsManagerActor;
use near_async::messaging::CanSend;
use near_chain::types::{EpochManagerAdapter, Tip};
use near_chain::{Chain, ChainStore};
use near_chain_configs::{MutableConfigValue, TrackedShardsConfig};
use near_epoch_manager::EpochManagerHandle;
use near_epoch_manager::shard_tracker::ShardTracker;
use near_epoch_manager::test_utils::setup_epoch_manager_with_block_and_chunk_producers;
use near_network::shards_manager::ShardsManagerRequestFromNetwork;
use near_network::test_utils::MockPeerManagerAdapter;
use near_o11y::span_wrapped_msg::SpanWrapped;
use near_primitives::bandwidth_scheduler::BandwidthRequests;
use near_primitives::hash::CryptoHash;
use near_primitives::merkle::{self, MerklePath};
use near_primitives::receipt::Receipt;
use near_primitives::sharding::{
    EncodedShardChunk, PartialEncodedChunk, PartialEncodedChunkPart, PartialEncodedChunkV2,
    ShardChunkHeader, ShardChunkWithEncoding,
};
use near_primitives::stateless_validation::ChunkProductionKey;
use near_primitives::test_utils::create_test_signer;
use near_primitives::types::MerkleHash;
use near_primitives::types::{AccountId, EpochId};
use near_store::adapter::StoreAdapter;
use near_store::adapter::chunk_store::ChunkStoreAdapter;
use near_store::set_genesis_height;
use near_store::test_utils::create_test_store;
use parking_lot::{Mutex, RwLock};
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::collections::VecDeque;
use std::sync::Arc;

pub struct ChunkTestFixture {
    pub store: ChunkStoreAdapter,
    pub epoch_manager: EpochManagerHandle,
    pub shard_tracker: ShardTracker,
    pub mock_network: Arc<MockPeerManagerAdapter>,
    pub mock_client_adapter: Arc<MockClientAdapterForShardsManager>,
    pub chain_store: ChainStore,
    pub all_part_ords: Vec<u64>,
    pub mock_part_ords: Vec<u64>,
    pub mock_merkle_paths: Vec<MerklePath>,
    pub mock_outgoing_receipts: Vec<Receipt>,
    pub mock_encoded_chunk: EncodedShardChunk,
    pub mock_chunk_part_owner: AccountId,
    pub mock_shard_tracker: AccountId,
    pub mock_chunk_header: ShardChunkHeader,
    pub mock_chunk_parts: Vec<PartialEncodedChunkPart>,
    pub mock_chain_head: Tip,
    pub rs: ReedSolomon,
}

impl Default for ChunkTestFixture {
    fn default() -> Self {
        Self::new(false, 3, 6, 6, true)
    }
}

impl ChunkTestFixture {
    pub fn new(
        orphan_chunk: bool,
        num_shards: u64,
        num_block_producers: usize,
        num_chunk_only_producers: usize,
        track_all_shards: bool,
    ) -> Self {
        if num_shards > num_block_producers as u64 {
            panic!("Invalid setup: there must be at least as many block producers as shards");
        }
        let store = create_test_store();
        let mut store_update = store.store_update();
        set_genesis_height(&mut store_update, &0);
        store_update.commit().unwrap();

        let epoch_manager = setup_epoch_manager_with_block_and_chunk_producers(
            store.clone(),
            (0..num_block_producers).map(|i| format!("test_bp_{}", i).parse().unwrap()).collect(),
            (0..num_chunk_only_producers)
                .map(|i| format!("test_cp_{}", i).parse().unwrap())
                .collect(),
            num_shards,
            2,
        );
        let epoch_manager = epoch_manager.into_handle();
        let shard_layout = epoch_manager.get_shard_layout(&EpochId::default()).unwrap();
        let shard_tracker = ShardTracker::new(
            if track_all_shards {
                TrackedShardsConfig::AllShards
            } else {
                TrackedShardsConfig::NoShards
            },
            Arc::new(epoch_manager.clone()),
            MutableConfigValue::new(None, "validator_signer"),
        );
        let mock_network = Arc::new(MockPeerManagerAdapter::default());
        let mock_client_adapter = Arc::new(MockClientAdapterForShardsManager::default());

        let data_parts = epoch_manager.num_data_parts();
        let parity_parts = epoch_manager.num_total_parts() - data_parts;
        let rs = ReedSolomon::new(data_parts, parity_parts).unwrap();
        let mock_ancestor_hash = CryptoHash::default();
        // generate a random block hash for the block at height 1
        let (mock_parent_hash, mock_height) =
            if orphan_chunk { (CryptoHash::hash_bytes(&[]), 2) } else { (mock_ancestor_hash, 1) };
        // setting this to 2 instead of 0 so that when chunk producers
        let mock_shard_id = shard_layout.shard_ids().next().unwrap();
        let mock_epoch_id =
            epoch_manager.get_epoch_id_from_prev_block(&mock_ancestor_hash).unwrap();
        let mock_chunk_producer = epoch_manager
            .get_chunk_producer_info(&ChunkProductionKey {
                epoch_id: mock_epoch_id,
                height_created: mock_height,
                shard_id: mock_shard_id,
            })
            .unwrap()
            .take_account_id();
        let signer = create_test_signer(mock_chunk_producer.as_str());
        let validators: Vec<_> = epoch_manager
            .get_epoch_block_producers_ordered(&EpochId::default())
            .unwrap()
            .into_iter()
            .map(|v| v.account_id().clone())
            .collect();
        let mock_shard_tracker = validators
            .iter()
            .find(|v| {
                if v == &&mock_chunk_producer {
                    false
                } else {
                    let tracks_shard = shard_tracker
                        .cares_about_shard_this_or_next_epoch_for_account_id(
                            *v,
                            &mock_ancestor_hash,
                            mock_shard_id,
                        );
                    tracks_shard
                }
            })
            .cloned()
            .unwrap();
        let mock_chunk_part_owner = validators
            .into_iter()
            .find(|v| v != &mock_chunk_producer && v != &mock_shard_tracker)
            .unwrap();

        let shard_layout = epoch_manager.get_shard_layout(&EpochId::default()).unwrap();
        let receipts_hashes = Chain::build_receipts_hashes(&[], &shard_layout).unwrap();
        let (receipts_root, _) = merkle::merklize(&receipts_hashes);
        let (mock_chunk, mock_merkle_paths) = ShardChunkWithEncoding::new(
            mock_parent_hash,
            Default::default(),
            Default::default(),
            mock_height,
            mock_shard_id,
            0,
            1000,
            0,
            Vec::new(),
            Vec::new(),
            vec![],
            receipts_root,
            MerkleHash::default(),
            Default::default(),
            BandwidthRequests::empty(),
            &signer,
            &rs,
        );

        let mock_encoded_chunk = mock_chunk.into_parts().1;

        let all_part_ords: Vec<u64> =
            (0..mock_encoded_chunk.content().parts.len()).map(|p| p as u64).collect();
        let mock_part_ords = all_part_ords
            .iter()
            .copied()
            .filter(|p| {
                epoch_manager.get_part_owner(&mock_epoch_id, *p).unwrap() == mock_chunk_part_owner
            })
            .collect();
        let encoded_chunk = mock_encoded_chunk.create_partial_encoded_chunk(
            all_part_ords.clone(),
            Vec::new(),
            &mock_merkle_paths,
        );
        let chain_store = ChainStore::new(store.clone(), true, 5);

        ChunkTestFixture {
            store: store.chunk_store(),
            epoch_manager,
            shard_tracker,
            mock_network,
            mock_client_adapter,
            chain_store,
            all_part_ords,
            mock_part_ords,
            mock_encoded_chunk,
            mock_merkle_paths,
            mock_outgoing_receipts: vec![],
            mock_chunk_part_owner,
            mock_shard_tracker,
            mock_chunk_header: encoded_chunk.cloned_header(),
            mock_chunk_parts: encoded_chunk.parts().to_vec(),
            mock_chain_head: Tip {
                height: 0,
                last_block_hash: CryptoHash::default(),
                prev_block_hash: CryptoHash::default(),
                epoch_id: EpochId::default(),
                next_epoch_id: EpochId::default(),
            },
            rs,
        }
    }

    pub fn make_partial_encoded_chunk(&self, part_ords: &[u64]) -> PartialEncodedChunk {
        let parts = part_ords
            .iter()
            .copied()
            .filter_map(|ord| self.mock_chunk_parts.iter().find(|part| part.part_ord == ord))
            .cloned()
            .collect();
        PartialEncodedChunk::V2(PartialEncodedChunkV2 {
            header: self.mock_chunk_header.clone(),
            parts,
            prev_outgoing_receipts: Vec::new(),
        })
    }

    pub fn count_chunk_completion_messages(&self) -> usize {
        let mut chunks_completed = 0;
        while let Some(message) = self.mock_client_adapter.pop() {
            if let ShardsManagerResponse::ChunkCompleted { .. } = message.span_unwrap() {
                chunks_completed += 1;
            }
        }
        chunks_completed
    }

    pub fn count_chunk_ready_for_inclusion_messages(&self) -> usize {
        let mut chunks_ready = 0;
        while let Some(message) = self.mock_client_adapter.pop() {
            if let ShardsManagerResponse::ChunkHeaderReadyForInclusion { .. } =
                message.span_unwrap()
            {
                chunks_ready += 1;
            }
        }
        chunks_ready
    }
}

/// Gets the Tip from the block hash and the EpochManager.
pub fn tip(epoch_manager: &dyn EpochManagerAdapter, last_block_hash: CryptoHash) -> Tip {
    let block_info = epoch_manager.get_block_info(&last_block_hash).unwrap();
    let epoch_id = epoch_manager.get_epoch_id(&last_block_hash).unwrap();
    let next_epoch_id = epoch_manager.get_next_epoch_id(&last_block_hash).unwrap();
    Tip {
        height: block_info.height(),
        last_block_hash,
        prev_block_hash: *block_info.prev_hash(),
        epoch_id,
        next_epoch_id,
    }
}

/// Returns the tip representing the genesis block for testing.
pub fn default_tip() -> Tip {
    Tip {
        height: 0,
        last_block_hash: CryptoHash::default(),
        prev_block_hash: CryptoHash::default(),
        epoch_id: EpochId::default(),
        next_epoch_id: EpochId::default(),
    }
}

// Mocked `PeerManager` adapter, has a queue of `PeerManagerMessageRequest` messages.
#[derive(Default)]
pub struct MockClientAdapterForShardsManager {
    pub requests: Arc<RwLock<VecDeque<SpanWrapped<ShardsManagerResponse>>>>,
}

impl CanSend<SpanWrapped<ShardsManagerResponse>> for MockClientAdapterForShardsManager {
    fn send(&self, msg: SpanWrapped<ShardsManagerResponse>) {
        self.requests.write().push_back(msg);
    }
}

impl MockClientAdapterForShardsManager {
    pub fn pop(&self) -> Option<SpanWrapped<ShardsManagerResponse>> {
        self.requests.write().pop_front()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardsManagerResendChunkRequests;

// Allows ShardsManagerActor-like behavior, except without having to spawn an actor,
// and without having to manually route ShardsManagerRequest messages. This only works
// for single-threaded (synchronous) tests. The ShardsManager is immediately called
// upon receiving a ShardsManagerRequest message.
#[derive(Clone)]
pub struct SynchronousShardsManagerAdapter {
    // Need a mutex here even though we only support single-threaded tests, because
    // MsgRecipient requires Sync.
    pub shards_manager: Arc<Mutex<ShardsManagerActor>>,
}

impl CanSend<ShardsManagerRequestFromClient> for SynchronousShardsManagerAdapter {
    fn send(&self, msg: ShardsManagerRequestFromClient) {
        let mut shards_manager = self.shards_manager.lock();
        shards_manager.handle_client_request(msg);
    }
}

impl CanSend<ShardsManagerRequestFromNetwork> for SynchronousShardsManagerAdapter {
    fn send(&self, msg: ShardsManagerRequestFromNetwork) {
        let mut shards_manager = self.shards_manager.lock();
        shards_manager.handle_network_request(msg);
    }
}

impl CanSend<ShardsManagerResendChunkRequests> for SynchronousShardsManagerAdapter {
    fn send(&self, _: ShardsManagerResendChunkRequests) {
        let mut shards_manager = self.shards_manager.lock();
        shards_manager.resend_chunk_requests();
    }
}

impl SynchronousShardsManagerAdapter {
    pub fn new(shards_manager: ShardsManagerActor) -> Self {
        Self { shards_manager: Arc::new(Mutex::new(shards_manager)) }
    }
}
