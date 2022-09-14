use crate::{
    errors::{BlockProcessResult, RuleError},
    model::{
        services::{reachability::MTReachabilityService, relations::MTRelationsService},
        stores::{
            block_window_cache::{BlockWindowCacheStore, BlockWindowHeap},
            daa::DbDaaStore,
            depth::DbDepthStore,
            errors::StoreResultExtensions,
            ghostdag::{DbGhostdagStore, GhostdagData},
            headers::DbHeadersStore,
            pruning::{DbPruningStore, PruningStore, PruningStoreReader},
            reachability::{DbReachabilityStore, StagingReachabilityStore},
            relations::{DbRelationsStore, RelationsStoreBatchExtensions},
            statuses::{
                BlockStatus::{self, StatusHeaderOnly, StatusInvalid},
                DbStatusesStore, StatusesStore, StatusesStoreBatchExtensions, StatusesStoreReader,
            },
            DB,
        },
    },
    params::Params,
    pipeline::deps_manager::{BlockTask, BlockTaskDependencyManager},
    processes::{
        block_at_depth::BlockDepthManager, dagtraversalmanager::DagTraversalManager, difficulty::DifficultyManager,
        ghostdag::protocol::GhostdagManager, pastmediantime::PastMedianTimeManager,
        reachability::inquirer as reachability,
    },
    test_helpers::header_from_precomputed_hash,
};
use consensus_core::{
    blockhash::{BlockHashes, ORIGIN},
    header::Header,
};
use crossbeam_channel::{Receiver, Sender};
use hashes::Hash;
use parking_lot::RwLock;
use rocksdb::WriteBatch;
use std::sync::{atomic::Ordering, Arc};

use super::super::ProcessingCounters;

pub struct HeaderProcessingContext<'a> {
    pub hash: Hash,
    pub header: &'a Header,
    pub pruning_point: Hash,

    // Staging data
    pub ghostdag_data: Option<Arc<GhostdagData>>,
    pub block_window_for_difficulty: Option<BlockWindowHeap>,
    pub block_window_for_past_median_time: Option<BlockWindowHeap>,
    pub daa_added_blocks: Option<Vec<Hash>>,
    pub merge_depth_root: Option<Hash>,
    pub finality_point: Option<Hash>,

    // Cache
    non_pruned_parents: Option<BlockHashes>,
}

impl<'a> HeaderProcessingContext<'a> {
    pub fn new(hash: Hash, header: &'a Header, pruning_point: Hash) -> Self {
        Self {
            hash,
            header,
            pruning_point,
            ghostdag_data: None,
            non_pruned_parents: None,
            block_window_for_difficulty: None,
            daa_added_blocks: None,
            block_window_for_past_median_time: None,
            merge_depth_root: None,
            finality_point: None,
        }
    }

    pub fn get_non_pruned_parents(&mut self) -> BlockHashes {
        if let Some(parents) = self.non_pruned_parents.clone() {
            return parents;
        }

        let non_pruned_parents = Arc::new(self.header.direct_parents().clone()); // TODO: Exclude pruned parents
        self.non_pruned_parents = Some(non_pruned_parents.clone());
        non_pruned_parents
    }
}

pub struct HeaderProcessor {
    // Channels
    receiver: Receiver<BlockTask>,
    body_sender: Sender<BlockTask>,

    // Config
    pub(super) genesis_hash: Hash,
    pub(super) timestamp_deviation_tolerance: u64,
    pub(super) target_time_per_block: u64,
    pub(super) max_block_parents: u8,
    pub(super) difficulty_window_size: usize,
    pub(super) mergeset_size_limit: u64,
    pub(super) genesis_bits: u32,

    // DB
    db: Arc<DB>,

    // Stores
    relations_store: Arc<RwLock<DbRelationsStore>>,
    reachability_store: Arc<RwLock<DbReachabilityStore>>,
    ghostdag_store: Arc<DbGhostdagStore>,
    pub(super) statuses_store: Arc<RwLock<DbStatusesStore>>,
    pub(super) pruning_store: Arc<RwLock<DbPruningStore>>,
    pub(super) block_window_cache_for_difficulty: Arc<BlockWindowCacheStore>,
    pub(super) block_window_cache_for_past_median_time: Arc<BlockWindowCacheStore>,
    pub(super) daa_store: Arc<DbDaaStore>,
    pub(super) headers_store: Arc<DbHeadersStore>,
    depth_store: Arc<DbDepthStore>,

    // Managers and services
    ghostdag_manager: GhostdagManager<
        DbGhostdagStore,
        MTRelationsService<DbRelationsStore>,
        MTReachabilityService<DbReachabilityStore>,
        DbHeadersStore,
    >,
    pub(super) dag_traversal_manager: DagTraversalManager<DbGhostdagStore, BlockWindowCacheStore>,
    pub(super) difficulty_manager: DifficultyManager<DbHeadersStore>,
    pub(super) past_median_time_manager: PastMedianTimeManager<DbHeadersStore, DbGhostdagStore, BlockWindowCacheStore>,
    pub(super) depth_manager: BlockDepthManager<DbDepthStore, DbReachabilityStore, DbGhostdagStore>,
    pub(super) reachability_service: MTReachabilityService<DbReachabilityStore>,

    // Dependency manager
    task_manager: BlockTaskDependencyManager,

    // Counters
    counters: Arc<ProcessingCounters>,
}

impl HeaderProcessor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        receiver: Receiver<BlockTask>, body_sender: Sender<BlockTask>, params: &Params, db: Arc<DB>,
        relations_store: Arc<RwLock<DbRelationsStore>>, reachability_store: Arc<RwLock<DbReachabilityStore>>,
        ghostdag_store: Arc<DbGhostdagStore>, headers_store: Arc<DbHeadersStore>, daa_store: Arc<DbDaaStore>,
        statuses_store: Arc<RwLock<DbStatusesStore>>, pruning_store: Arc<RwLock<DbPruningStore>>,
        depth_store: Arc<DbDepthStore>, block_window_cache_for_difficulty: Arc<BlockWindowCacheStore>,
        block_window_cache_for_past_median_time: Arc<BlockWindowCacheStore>,
        reachability_service: MTReachabilityService<DbReachabilityStore>,
        relations_service: Arc<MTRelationsService<DbRelationsStore>>,
        past_median_time_manager: PastMedianTimeManager<DbHeadersStore, DbGhostdagStore, BlockWindowCacheStore>,
        dag_traversal_manager: DagTraversalManager<DbGhostdagStore, BlockWindowCacheStore>,
        difficulty_manager: DifficultyManager<DbHeadersStore>,
        depth_manager: BlockDepthManager<DbDepthStore, DbReachabilityStore, DbGhostdagStore>,
        counters: Arc<ProcessingCounters>,
    ) -> Self {
        Self {
            receiver,
            body_sender,
            genesis_hash: params.genesis_hash,
            difficulty_window_size: params.difficulty_window_size,
            db,
            relations_store,
            reachability_store,
            ghostdag_store: ghostdag_store.clone(),
            statuses_store,
            pruning_store,
            daa_store,
            headers_store: headers_store.clone(),
            depth_store,
            block_window_cache_for_difficulty,
            block_window_cache_for_past_median_time,
            ghostdag_manager: GhostdagManager::new(
                params.genesis_hash,
                params.ghostdag_k,
                ghostdag_store,
                relations_service,
                headers_store,
                reachability_service.clone(),
            ),
            dag_traversal_manager,
            difficulty_manager,
            reachability_service,
            past_median_time_manager,
            depth_manager,
            task_manager: BlockTaskDependencyManager::new(),
            counters,
            timestamp_deviation_tolerance: params.timestamp_deviation_tolerance,
            target_time_per_block: params.target_time_per_block,
            max_block_parents: params.max_block_parents,
            mergeset_size_limit: params.mergeset_size_limit,
            genesis_bits: params.genesis_bits,
        }
    }

    pub fn worker(self: &Arc<HeaderProcessor>) {
        while let Ok(task) = self.receiver.recv() {
            match task {
                BlockTask::Exit => break,
                BlockTask::Process(block, result_transmitters) => {
                    let hash = block.header.hash;
                    if self
                        .task_manager
                        .register(block, result_transmitters)
                    {
                        let processor = self.clone();
                        rayon::spawn(move || {
                            processor.queue_block(hash);
                        });
                    }
                }
            };
        }

        // Wait until all workers are idle before exiting
        self.task_manager.wait_for_idle();

        // Pass the exit signal on to the following processor
        self.body_sender.send(BlockTask::Exit).unwrap();
    }

    fn queue_block(self: &Arc<HeaderProcessor>, hash: Hash) {
        if let Some(block) = self.task_manager.try_begin(hash) {
            let res = self.process_header(&block.header);

            let dependent_tasks = self
                .task_manager
                .end(hash, |block, result_transmitters| {
                    if res.is_err() || block.is_header_only() {
                        for transmitter in result_transmitters {
                            // We don't care if receivers were dropped
                            let _ = transmitter.send(res.clone());
                        }
                    } else {
                        self.body_sender
                            .send(BlockTask::Process(block, result_transmitters))
                            .unwrap();
                    }
                });

            for dep in dependent_tasks {
                let processor = self.clone();
                rayon::spawn(move || processor.queue_block(dep));
            }
        }
    }

    fn header_was_processed(self: &Arc<HeaderProcessor>, hash: Hash) -> bool {
        self.statuses_store.read().has(hash).unwrap()
    }

    fn process_header(self: &Arc<HeaderProcessor>, header: &Header) -> BlockProcessResult<BlockStatus> {
        let status_option = self
            .statuses_store
            .read()
            .get(header.hash)
            .unwrap_option();

        match status_option {
            Some(StatusInvalid) => return Err(RuleError::KnownInvalid),
            Some(status) => return Ok(status),
            None => {}
        }

        // Create processing context
        let mut ctx =
            HeaderProcessingContext::new(header.hash, header, self.pruning_store.read().pruning_point().unwrap());

        // Run GHOSTDAG for the new header
        self.pre_ghostdag_validation(&mut ctx, header)?;
        self.ghostdag_manager
            .add_block(&mut ctx, header.hash); // TODO: Run GHOSTDAG for all block levels

        //
        // TODO: imp all remaining header validation and processing steps :)
        //
        self.pre_pow_validation(&mut ctx, header)?;

        if let Err(e) = self.post_pow_validation(&mut ctx, header) {
            self.statuses_store
                .write()
                .set(ctx.hash, StatusInvalid)
                .unwrap();
            return Err(e);
        }

        self.commit_header(ctx, header);

        // Report counters
        self.counters
            .header_counts
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .dep_counts
            .fetch_add(header.direct_parents().len() as u64, Ordering::Relaxed);
        Ok(StatusHeaderOnly)
    }

    fn commit_header(self: &Arc<HeaderProcessor>, ctx: HeaderProcessingContext, header: &Header) {
        let ghostdag_data = ctx.ghostdag_data.unwrap();

        // Create a DB batch writer
        let mut batch = WriteBatch::default();

        // Write to append only stores: this requires no lock and hence done first
        self.ghostdag_store
            .insert_batch(&mut batch, ctx.hash, &ghostdag_data)
            .unwrap();
        self.block_window_cache_for_difficulty
            .insert(ctx.hash, Arc::new(ctx.block_window_for_difficulty.unwrap()));
        self.block_window_cache_for_past_median_time
            .insert(ctx.hash, Arc::new(ctx.block_window_for_past_median_time.unwrap()));
        self.daa_store
            .insert_batch(&mut batch, ctx.hash, Arc::new(ctx.daa_added_blocks.unwrap()))
            .unwrap();
        self.headers_store
            .insert_batch(&mut batch, ctx.hash, Arc::new(ctx.header.clone()))
            .unwrap();
        self.depth_store
            .insert_batch(&mut batch, ctx.hash, ctx.merge_depth_root.unwrap(), ctx.finality_point.unwrap())
            .unwrap();

        // Create staging reachability store. We use an upgradable read here to avoid concurrent
        // staging reachability operations. PERF: we assume that reachability processing time << header processing
        // time, and thus serializing this part will do no harm. However this should be benchmarked. The
        // alternative is to create a separate ReachabilityProcessor and to manage things more tightly.
        let mut staging = StagingReachabilityStore::new(self.reachability_store.upgradable_read());

        // Add block to staging reachability
        reachability::add_block(
            &mut staging,
            ctx.hash,
            ghostdag_data.selected_parent,
            &mut ghostdag_data.unordered_mergeset_without_selected_parent(),
        )
        .unwrap();
        // Hint reachability about the new tip.
        // TODO: imp header tips store and call this only for an actual header selected tip
        reachability::hint_virtual_selected_parent(&mut staging, ctx.hash).unwrap();

        // Non-append only stores need to use write locks.
        // Note we need to keep the lock write guards until the batch is written.
        let relations_write_guard = self
            .relations_store
            .insert_batch(&mut batch, header.hash, BlockHashes::new(header.direct_parents().clone()))
            .unwrap();

        let statuses_write_guard = self
            .statuses_store
            .set_batch(&mut batch, ctx.hash, StatusHeaderOnly)
            .unwrap();

        // Write reachability data. Only at this brief moment the reachability store is locked for reads.
        // We take special care for this since reachability read queries are used throughout the system frequently.
        // Note we hold the lock until the batch is written
        let reachability_write_guard = staging.commit(&mut batch).unwrap();

        // Flush the batch to the DB
        self.db.write(batch).unwrap();

        // Calling the drops explicitly after the batch is written in order to avoid possible errors.
        drop(reachability_write_guard);
        drop(statuses_write_guard);
        drop(relations_write_guard);
    }

    pub fn process_genesis_if_needed(self: &Arc<HeaderProcessor>) {
        if self.header_was_processed(self.genesis_hash) {
            return;
        }

        self.pruning_store
            .write()
            .set(self.genesis_hash)
            .unwrap();
        let mut header = header_from_precomputed_hash(self.genesis_hash, vec![]); // TODO
        header.bits = self.genesis_bits;
        let mut ctx = HeaderProcessingContext::new(self.genesis_hash, &header, ORIGIN);
        self.ghostdag_manager
            .add_genesis_if_needed(&mut ctx);
        ctx.block_window_for_difficulty = Some(Default::default());
        ctx.block_window_for_past_median_time = Some(Default::default());
        ctx.daa_added_blocks = Some(Default::default());
        ctx.merge_depth_root = Some(ORIGIN);
        ctx.finality_point = Some(ORIGIN);
        self.commit_header(ctx, &header);
    }
}