use super::scheduler::Scheduler;
use super::scheduler_policy::LasCompactionTimeSchedulerPolicy;
use super::OneOffCompactionMessage;
use crate::compactor::types::CompactionJob;
use crate::compactor::types::ScheduledCompactionMessage;
use crate::config::CompactionServiceConfig;
use crate::execution::orchestration::CompactOrchestrator;
use crate::execution::orchestration::CompactionResponse;
use async_trait::async_trait;
use chroma_blockstore::provider::BlockfileProvider;
use chroma_config::assignment;
use chroma_config::Configurable;
use chroma_error::{ChromaError, ErrorCodes};
use chroma_index::hnsw_provider::HnswIndexProvider;
use chroma_log::Log;
use chroma_memberlist::memberlist_provider::Memberlist;
use chroma_storage::Storage;
use chroma_sysdb::SysDb;
use chroma_system::Dispatcher;
use chroma_system::Orchestrator;
use chroma_system::{Component, ComponentContext, ComponentHandle, Handler, System};
use chroma_types::CollectionUuid;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::str::FromStr;
use std::time::Duration;
use thiserror::Error;
use tracing::instrument;
use tracing::span;
use tracing::Instrument;
use tracing::Span;
use uuid::Uuid;

pub(crate) struct CompactionManager {
    system: Option<System>,
    scheduler: Scheduler,
    // Dependencies
    log: Box<Log>,
    sysdb: Box<SysDb>,
    #[allow(dead_code)]
    storage: Storage,
    blockfile_provider: BlockfileProvider,
    hnsw_index_provider: HnswIndexProvider,
    // Dispatcher
    dispatcher: Option<ComponentHandle<Dispatcher>>,
    // Config
    compaction_manager_queue_size: usize,
    compaction_interval: Duration,
    #[allow(dead_code)]
    min_compaction_size: usize,
    max_compaction_size: usize,
    max_partition_size: usize,
}

#[derive(Error, Debug)]
pub(crate) enum CompactionError {
    #[error("Failed to compact")]
    FailedToCompact,
}

impl ChromaError for CompactionError {
    fn code(&self) -> ErrorCodes {
        match self {
            CompactionError::FailedToCompact => ErrorCodes::Internal,
        }
    }
}

impl CompactionManager {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        scheduler: Scheduler,
        log: Box<Log>,
        sysdb: Box<SysDb>,
        storage: Storage,
        blockfile_provider: BlockfileProvider,
        hnsw_index_provider: HnswIndexProvider,
        compaction_manager_queue_size: usize,
        compaction_interval: Duration,
        min_compaction_size: usize,
        max_compaction_size: usize,
        max_partition_size: usize,
    ) -> Self {
        CompactionManager {
            system: None,
            scheduler,
            log,
            sysdb,
            storage,
            blockfile_provider,
            hnsw_index_provider,
            dispatcher: None,
            compaction_manager_queue_size,
            compaction_interval,
            min_compaction_size,
            max_compaction_size,
            max_partition_size,
        }
    }

    #[instrument(name = "CompactionManager::compact")]
    async fn compact(
        &self,
        compaction_job: &CompactionJob,
    ) -> Result<CompactionResponse, Box<dyn ChromaError>> {
        let dispatcher = match self.dispatcher {
            Some(ref dispatcher) => dispatcher.clone(),
            None => {
                tracing::error!("No dispatcher found");
                return Err(Box::new(CompactionError::FailedToCompact));
            }
        };

        match self.system {
            Some(ref system) => {
                let orchestrator = CompactOrchestrator::new(
                    compaction_job.clone(),
                    compaction_job.collection_id,
                    self.log.clone(),
                    self.sysdb.clone(),
                    self.blockfile_provider.clone(),
                    self.hnsw_index_provider.clone(),
                    dispatcher,
                    None,
                    self.max_compaction_size,
                    self.max_partition_size,
                );

                match orchestrator.run(system.clone()).await {
                    Ok(result) => {
                        tracing::info!("Compaction Job completed: {:?}", result);
                        return Ok(result);
                    }
                    Err(e) => {
                        tracing::error!("Compaction Job failed: {:?}", e);
                        return Err(Box::new(e));
                    }
                }
            }
            None => {
                tracing::error!("No system found");
                return Err(Box::new(CompactionError::FailedToCompact));
            }
        };
    }

    #[instrument(name = "CompactionManager::compact_batch")]
    pub(crate) async fn compact_batch(&mut self) -> Vec<CollectionUuid> {
        self.scheduler.schedule().await;
        let job_futures = self
            .scheduler
            .get_jobs()
            .map(|job| {
                let instrumented_span = span!(parent: None, tracing::Level::INFO, "Compacting job", collection_id = ?job.collection_id);
                instrumented_span.follows_from(Span::current());
                self.compact(job).instrument(instrumented_span)
            })
            .collect::<FuturesUnordered<_>>();

        tracing::info!("Running {} compaction jobs", job_futures.len());

        job_futures
            .filter_map(|result| async move {
                match result {
                    Ok(response) => {
                        tracing::info!("Compaction completed: {response:?}");
                        Some(response.compaction_job.collection_id)
                    }
                    Err(err) => {
                        tracing::error!("Compaction failed {err}");
                        None
                    }
                }
            })
            .collect()
            .await
    }

    pub(crate) fn set_dispatcher(&mut self, dispatcher: ComponentHandle<Dispatcher>) {
        self.dispatcher = Some(dispatcher);
    }

    pub(crate) fn set_system(&mut self, system: System) {
        self.system = Some(system);
    }
}

#[async_trait]
impl Configurable<CompactionServiceConfig> for CompactionManager {
    async fn try_from_config(
        config: &crate::config::CompactionServiceConfig,
    ) -> Result<Self, Box<dyn ChromaError>> {
        let log_config = &config.log;
        let log = match chroma_log::from_config(log_config).await {
            Ok(log) => log,
            Err(err) => {
                return Err(err);
            }
        };
        let sysdb_config = &config.sysdb;
        let sysdb = match chroma_sysdb::from_config(sysdb_config).await {
            Ok(sysdb) => sysdb,
            Err(err) => {
                return Err(err);
            }
        };

        let storage = match chroma_storage::from_config(&config.storage).await {
            Ok(storage) => storage,
            Err(err) => {
                return Err(err);
            }
        };

        let my_ip = config.my_member_id.clone();
        let policy = Box::new(LasCompactionTimeSchedulerPolicy {});
        let compaction_interval_sec = config.compactor.compaction_interval_sec;
        let max_concurrent_jobs = config.compactor.max_concurrent_jobs;
        let compaction_manager_queue_size = config.compactor.compaction_manager_queue_size;
        let min_compaction_size = config.compactor.min_compaction_size;
        let max_compaction_size = config.compactor.max_compaction_size;
        let max_partition_size = config.compactor.max_partition_size;
        let mut disabled_collections =
            HashSet::with_capacity(config.compactor.disabled_collections.len());
        for collection_id_str in &config.compactor.disabled_collections {
            disabled_collections.insert(CollectionUuid(Uuid::from_str(collection_id_str).unwrap()));
        }

        let assignment_policy_config = &config.assignment_policy;
        let assignment_policy = match assignment::from_config(assignment_policy_config).await {
            Ok(assignment_policy) => assignment_policy,
            Err(err) => {
                return Err(err);
            }
        };
        let scheduler = Scheduler::new(
            my_ip,
            log.clone(),
            sysdb.clone(),
            policy,
            max_concurrent_jobs,
            min_compaction_size,
            assignment_policy,
            disabled_collections,
        );

        let blockfile_provider = BlockfileProvider::try_from_config(&(
            config.blockfile_provider.clone(),
            storage.clone(),
        ))
        .await?;

        let hnsw_index_provider =
            HnswIndexProvider::try_from_config(&(config.hnsw_provider.clone(), storage.clone()))
                .await?;

        Ok(CompactionManager::new(
            scheduler,
            log,
            sysdb,
            storage.clone(),
            blockfile_provider,
            hnsw_index_provider,
            compaction_manager_queue_size,
            Duration::from_secs(compaction_interval_sec),
            min_compaction_size,
            max_compaction_size,
            max_partition_size,
        ))
    }
}

// ============== Component Implementation ==============
#[async_trait]
impl Component for CompactionManager {
    fn get_name() -> &'static str {
        "Compaction manager"
    }

    fn queue_size(&self) -> usize {
        self.compaction_manager_queue_size
    }

    async fn start(&mut self, ctx: &ComponentContext<Self>) -> () {
        tracing::info!("Starting CompactionManager");
        ctx.scheduler.schedule(
            ScheduledCompactionMessage {},
            self.compaction_interval,
            ctx,
            || Some(span!(parent: None, tracing::Level::INFO, "Scheduled compaction")),
        );
    }
}

impl Debug for CompactionManager {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactionManager").finish()
    }
}

// ============== Handlers ==============
#[async_trait]
impl Handler<ScheduledCompactionMessage> for CompactionManager {
    type Result = ();

    async fn handle(
        &mut self,
        _message: ScheduledCompactionMessage,
        ctx: &ComponentContext<CompactionManager>,
    ) {
        tracing::info!("CompactionManager: Performing scheduled compaction");
        let ids = self.compact_batch().await;
        self.hnsw_index_provider.purge_by_id(&ids).await;

        // Compaction is done, schedule the next compaction
        ctx.scheduler.schedule(
            ScheduledCompactionMessage {},
            self.compaction_interval,
            ctx,
            || Some(span!(parent: None, tracing::Level::INFO, "Scheduled compaction")),
        );
    }
}

#[async_trait]
impl Handler<OneOffCompactionMessage> for CompactionManager {
    type Result = ();
    async fn handle(
        &mut self,
        message: OneOffCompactionMessage,
        _ctx: &ComponentContext<CompactionManager>,
    ) {
        self.scheduler
            .add_oneoff_collections(message.collection_ids);
        tracing::info!(
            "One-off collections queued: {:?}",
            self.scheduler.get_oneoff_collections()
        );
    }
}

#[async_trait]
impl Handler<Memberlist> for CompactionManager {
    type Result = ();

    async fn handle(&mut self, message: Memberlist, _ctx: &ComponentContext<CompactionManager>) {
        self.scheduler.set_memberlist(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assignment::assignment_policy::AssignmentPolicy;
    use assignment::assignment_policy::RendezvousHashingAssignmentPolicy;
    use chroma_blockstore::arrow::config::TEST_MAX_BLOCK_SIZE_BYTES;
    use chroma_cache::{new_cache_for_test, new_non_persistent_cache_for_test};
    use chroma_log::in_memory_log::{InMemoryLog, InternalLogRecord};
    use chroma_memberlist::memberlist_provider::Member;
    use chroma_storage::local::LocalStorage;
    use chroma_sysdb::TestSysDb;
    use chroma_system::Dispatcher;
    use chroma_types::SegmentUuid;
    use chroma_types::{Collection, LogRecord, Operation, OperationRecord, Segment};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::str::FromStr;

    #[tokio::test]
    async fn test_compaction_manager() {
        let mut log = Box::new(Log::InMemory(InMemoryLog::new()));
        let in_memory_log = match *log {
            Log::InMemory(ref mut log) => log,
            _ => panic!("Expected InMemoryLog"),
        };
        let tmpdir = tempfile::tempdir().unwrap();
        let storage = Storage::Local(LocalStorage::new(tmpdir.path().to_str().unwrap()));

        let collection_uuid_1 =
            CollectionUuid::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        in_memory_log.add_log(
            collection_uuid_1,
            InternalLogRecord {
                collection_id: collection_uuid_1,
                log_offset: 0,
                log_ts: 1,
                record: LogRecord {
                    log_offset: 0,
                    record: OperationRecord {
                        id: "embedding_id_1".to_string(),
                        embedding: Some(vec![1.0, 2.0, 3.0]),
                        encoding: None,
                        metadata: None,
                        document: None,
                        operation: Operation::Add,
                    },
                },
            },
        );

        let collection_uuid_2 =
            CollectionUuid::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        in_memory_log.add_log(
            collection_uuid_2,
            InternalLogRecord {
                collection_id: collection_uuid_2,
                log_offset: 0,
                log_ts: 2,
                record: LogRecord {
                    log_offset: 0,
                    record: OperationRecord {
                        id: "embedding_id_2".to_string(),
                        embedding: Some(vec![4.0, 5.0, 6.0]),
                        encoding: None,
                        metadata: None,
                        document: None,
                        operation: Operation::Add,
                    },
                },
            },
        );

        let mut sysdb = Box::new(SysDb::Test(TestSysDb::new()));

        let tenant_1 = "tenant_1".to_string();
        let collection_1 = Collection {
            collection_id: collection_uuid_1,
            name: "collection_1".to_string(),
            metadata: None,
            dimension: Some(1),
            tenant: tenant_1.clone(),
            database: "database_1".to_string(),
            log_position: -1,
            version: 0,
            total_records_post_compaction: 0,
        };

        let tenant_2 = "tenant_2".to_string();
        let collection_2 = Collection {
            collection_id: collection_uuid_2,
            name: "collection_2".to_string(),
            metadata: None,
            dimension: Some(1),
            tenant: tenant_2.clone(),
            database: "database_2".to_string(),
            log_position: -1,
            version: 0,
            total_records_post_compaction: 0,
        };
        match *sysdb {
            SysDb::Test(ref mut sysdb) => {
                sysdb.add_collection(collection_1);
                sysdb.add_collection(collection_2);
            }
            _ => panic!("Invalid sysdb type"),
        }

        let collection_1_record_segment = Segment {
            id: SegmentUuid::new(),
            r#type: chroma_types::SegmentType::BlockfileRecord,
            scope: chroma_types::SegmentScope::RECORD,
            collection: collection_uuid_1,
            metadata: None,
            file_path: HashMap::new(),
        };

        let collection_2_record_segment = Segment {
            id: SegmentUuid::new(),
            r#type: chroma_types::SegmentType::BlockfileRecord,
            scope: chroma_types::SegmentScope::RECORD,
            collection: collection_uuid_2,
            metadata: None,
            file_path: HashMap::new(),
        };

        let collection_1_hnsw_segment = Segment {
            id: SegmentUuid::new(),
            r#type: chroma_types::SegmentType::HnswDistributed,
            scope: chroma_types::SegmentScope::VECTOR,
            collection: collection_uuid_1,
            metadata: None,
            file_path: HashMap::new(),
        };

        let collection_2_hnsw_segment = Segment {
            id: SegmentUuid::new(),
            r#type: chroma_types::SegmentType::HnswDistributed,
            scope: chroma_types::SegmentScope::VECTOR,
            collection: collection_uuid_2,
            metadata: None,
            file_path: HashMap::new(),
        };

        let collection_1_metadata_segment = Segment {
            id: SegmentUuid::new(),
            r#type: chroma_types::SegmentType::BlockfileMetadata,
            scope: chroma_types::SegmentScope::METADATA,
            collection: collection_uuid_1,
            metadata: None,
            file_path: HashMap::new(),
        };

        let collection_2_metadata_segment = Segment {
            id: SegmentUuid::new(),
            r#type: chroma_types::SegmentType::BlockfileMetadata,
            scope: chroma_types::SegmentScope::METADATA,
            collection: collection_uuid_2,
            metadata: None,
            file_path: HashMap::new(),
        };

        match *sysdb {
            SysDb::Test(ref mut sysdb) => {
                sysdb.add_segment(collection_1_record_segment);
                sysdb.add_segment(collection_2_record_segment);
                sysdb.add_segment(collection_1_hnsw_segment);
                sysdb.add_segment(collection_2_hnsw_segment);
                sysdb.add_segment(collection_1_metadata_segment);
                sysdb.add_segment(collection_2_metadata_segment);
                let last_compaction_time_1 = 2;
                sysdb.add_tenant_last_compaction_time(tenant_1, last_compaction_time_1);
                let last_compaction_time_2 = 1;
                sysdb.add_tenant_last_compaction_time(tenant_2, last_compaction_time_2);
            }
            _ => panic!("Invalid sysdb type"),
        }

        let my_member = Member {
            member_id: "member_1".to_string(),
            member_ip: "10.0.0.1".to_string(),
            member_node_name: "node_1".to_string(),
        };
        let compaction_manager_queue_size = 1000;
        let max_concurrent_jobs = 10;
        let compaction_interval = Duration::from_secs(1);
        let min_compaction_size = 0;
        let max_compaction_size = 1000;
        let max_partition_size = 1000;

        // Set assignment policy
        let mut assignment_policy = Box::new(RendezvousHashingAssignmentPolicy::default());
        assignment_policy.set_members(vec![my_member.member_id.clone()]);

        let mut scheduler = Scheduler::new(
            my_member.member_id.clone(),
            log.clone(),
            sysdb.clone(),
            Box::new(LasCompactionTimeSchedulerPolicy {}),
            max_concurrent_jobs,
            min_compaction_size,
            assignment_policy,
            HashSet::new(),
        );
        // Set memberlist
        scheduler.set_memberlist(vec![my_member.clone()]);

        let block_cache = new_cache_for_test();
        let sparse_index_cache = new_cache_for_test();
        let hnsw_cache = new_non_persistent_cache_for_test();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = CompactionManager::new(
            scheduler,
            log,
            sysdb,
            storage.clone(),
            BlockfileProvider::new_arrow(
                storage.clone(),
                TEST_MAX_BLOCK_SIZE_BYTES,
                block_cache,
                sparse_index_cache,
            ),
            HnswIndexProvider::new(
                storage,
                PathBuf::from(tmpdir.path().to_str().unwrap()),
                hnsw_cache,
                rx,
            ),
            compaction_manager_queue_size,
            compaction_interval,
            min_compaction_size,
            max_compaction_size,
            max_partition_size,
        );

        let system = System::new();

        let dispatcher = Dispatcher::new(10, 10, 10);
        let dispatcher_handle = system.start_component(dispatcher);
        manager.set_dispatcher(dispatcher_handle);
        manager.set_system(system);
        let compacted = manager.compact_batch().await;
        assert!(
            (compacted == vec![collection_uuid_1, collection_uuid_2])
                || (compacted == vec![collection_uuid_2, collection_uuid_1])
        );
    }
}
