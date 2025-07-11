// Copyright 2023 Greptime Team
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

use std::sync::Arc;

use common_procedure::{watcher, ProcedureId, ProcedureManagerRef, ProcedureWithId};
use common_telemetry::tracing_context::{FutureExt, TracingContext};
use common_telemetry::{info, tracing};
use snafu::{OptionExt, ResultExt};

use crate::cache_invalidator::CacheInvalidatorRef;
use crate::datanode_manager::DatanodeManagerRef;
use crate::ddl::alter_table::AlterTableProcedure;
use crate::ddl::create_table::CreateTableProcedure;
use crate::ddl::drop_table::DropTableProcedure;
use crate::ddl::truncate_table::TruncateTableProcedure;
use crate::ddl::{
    DdlContext, DdlTaskExecutor, ExecutorContext, TableMetadataAllocatorContext,
    TableMetadataAllocatorRef,
};
use crate::error::{
    self, RegisterProcedureLoaderSnafu, Result, SubmitProcedureSnafu, TableNotFoundSnafu,
    WaitProcedureSnafu,
};
use crate::key::table_info::TableInfoValue;
use crate::key::table_name::TableNameKey;
use crate::key::table_route::TableRouteValue;
use crate::key::{DeserializedValueWithBytes, TableMetadataManagerRef};
use crate::region_keeper::MemoryRegionKeeperRef;
use crate::rpc::ddl::DdlTask::{AlterTable, CreateTable, DropTable, TruncateTable};
use crate::rpc::ddl::{
    AlterTableTask, CreateTableTask, DropTableTask, SubmitDdlTaskRequest, SubmitDdlTaskResponse,
    TruncateTableTask,
};
use crate::rpc::router::RegionRoute;
pub type DdlManagerRef = Arc<DdlManager>;

/// The [DdlManager] provides the ability to execute Ddl.
pub struct DdlManager {
    procedure_manager: ProcedureManagerRef,
    datanode_manager: DatanodeManagerRef,
    cache_invalidator: CacheInvalidatorRef,
    table_metadata_manager: TableMetadataManagerRef,
    table_meta_allocator: TableMetadataAllocatorRef,
    memory_region_keeper: MemoryRegionKeeperRef,
}

impl DdlManager {
    /// Returns a new [DdlManager] with all Ddl [BoxedProcedureLoader](common_procedure::procedure::BoxedProcedureLoader)s registered.
    pub fn try_new(
        procedure_manager: ProcedureManagerRef,
        datanode_clients: DatanodeManagerRef,
        cache_invalidator: CacheInvalidatorRef,
        table_metadata_manager: TableMetadataManagerRef,
        table_meta_allocator: TableMetadataAllocatorRef,
        memory_region_keeper: MemoryRegionKeeperRef,
    ) -> Result<Self> {
        let manager = Self {
            procedure_manager,
            datanode_manager: datanode_clients,
            cache_invalidator,
            table_metadata_manager,
            table_meta_allocator,
            memory_region_keeper,
        };
        manager.register_loaders()?;
        Ok(manager)
    }

    /// Returns the [TableMetadataManagerRef].
    pub fn table_metadata_manager(&self) -> &TableMetadataManagerRef {
        &self.table_metadata_manager
    }

    /// Returns the [DdlContext]
    pub fn create_context(&self) -> DdlContext {
        DdlContext {
            datanode_manager: self.datanode_manager.clone(),
            cache_invalidator: self.cache_invalidator.clone(),
            table_metadata_manager: self.table_metadata_manager.clone(),
            memory_region_keeper: self.memory_region_keeper.clone(),
        }
    }

    fn register_loaders(&self) -> Result<()> {
        let context = self.create_context();

        self.procedure_manager
            .register_loader(
                CreateTableProcedure::TYPE_NAME,
                Box::new(move |json| {
                    let context = context.clone();
                    CreateTableProcedure::from_json(json, context).map(|p| Box::new(p) as _)
                }),
            )
            .context(RegisterProcedureLoaderSnafu {
                type_name: CreateTableProcedure::TYPE_NAME,
            })?;

        let context = self.create_context();

        self.procedure_manager
            .register_loader(
                DropTableProcedure::TYPE_NAME,
                Box::new(move |json| {
                    let context = context.clone();
                    DropTableProcedure::from_json(json, context).map(|p| Box::new(p) as _)
                }),
            )
            .context(RegisterProcedureLoaderSnafu {
                type_name: DropTableProcedure::TYPE_NAME,
            })?;

        let context = self.create_context();

        self.procedure_manager
            .register_loader(
                AlterTableProcedure::TYPE_NAME,
                Box::new(move |json| {
                    let context = context.clone();
                    AlterTableProcedure::from_json(json, context).map(|p| Box::new(p) as _)
                }),
            )
            .context(RegisterProcedureLoaderSnafu {
                type_name: AlterTableProcedure::TYPE_NAME,
            })?;

        let context = self.create_context();

        self.procedure_manager
            .register_loader(
                TruncateTableProcedure::TYPE_NAME,
                Box::new(move |json| {
                    let context = context.clone();
                    TruncateTableProcedure::from_json(json, context).map(|p| Box::new(p) as _)
                }),
            )
            .context(RegisterProcedureLoaderSnafu {
                type_name: TruncateTableProcedure::TYPE_NAME,
            })
    }

    #[tracing::instrument(skip_all)]
    /// Submits and executes an alter table task.
    pub async fn submit_alter_table_task(
        &self,
        cluster_id: u64,
        alter_table_task: AlterTableTask,
        table_info_value: DeserializedValueWithBytes<TableInfoValue>,
    ) -> Result<ProcedureId> {
        let context = self.create_context();

        let procedure =
            AlterTableProcedure::new(cluster_id, alter_table_task, table_info_value, context)?;

        let procedure_with_id = ProcedureWithId::with_random_id(Box::new(procedure));

        self.submit_procedure(procedure_with_id).await
    }

    #[tracing::instrument(skip_all)]
    /// Submits and executes a create table task.
    pub async fn submit_create_table_task(
        &self,
        cluster_id: u64,
        create_table_task: CreateTableTask,
        region_routes: Vec<RegionRoute>,
    ) -> Result<ProcedureId> {
        let context = self.create_context();

        let procedure =
            CreateTableProcedure::new(cluster_id, create_table_task, region_routes, context);

        let procedure_with_id = ProcedureWithId::with_random_id(Box::new(procedure));

        self.submit_procedure(procedure_with_id).await
    }

    #[tracing::instrument(skip_all)]
    /// Submits and executes a drop table task.
    pub async fn submit_drop_table_task(
        &self,
        cluster_id: u64,
        drop_table_task: DropTableTask,
        table_info_value: DeserializedValueWithBytes<TableInfoValue>,
        table_route_value: DeserializedValueWithBytes<TableRouteValue>,
    ) -> Result<ProcedureId> {
        let context = self.create_context();

        let procedure = DropTableProcedure::new(
            cluster_id,
            drop_table_task,
            table_route_value,
            table_info_value,
            context,
        );

        let procedure_with_id = ProcedureWithId::with_random_id(Box::new(procedure));

        self.submit_procedure(procedure_with_id).await
    }

    #[tracing::instrument(skip_all)]
    /// Submits and executes a truncate table task.
    pub async fn submit_truncate_table_task(
        &self,
        cluster_id: u64,
        truncate_table_task: TruncateTableTask,
        table_info_value: DeserializedValueWithBytes<TableInfoValue>,
        region_routes: Vec<RegionRoute>,
    ) -> Result<ProcedureId> {
        let context = self.create_context();
        let procedure = TruncateTableProcedure::new(
            cluster_id,
            truncate_table_task,
            table_info_value,
            region_routes,
            context,
        );

        let procedure_with_id = ProcedureWithId::with_random_id(Box::new(procedure));

        self.submit_procedure(procedure_with_id).await
    }

    async fn submit_procedure(&self, procedure_with_id: ProcedureWithId) -> Result<ProcedureId> {
        let procedure_id = procedure_with_id.id;

        let mut watcher = self
            .procedure_manager
            .submit(procedure_with_id)
            .await
            .context(SubmitProcedureSnafu)?;

        watcher::wait(&mut watcher)
            .await
            .context(WaitProcedureSnafu)?;

        Ok(procedure_id)
    }
}

async fn handle_truncate_table_task(
    ddl_manager: &DdlManager,
    cluster_id: u64,
    truncate_table_task: TruncateTableTask,
) -> Result<SubmitDdlTaskResponse> {
    let table_id = truncate_table_task.table_id;
    let table_metadata_manager = &ddl_manager.table_metadata_manager();
    let table_ref = truncate_table_task.table_ref();

    let (table_info_value, table_route_value) =
        table_metadata_manager.get_full_table_info(table_id).await?;

    let table_info_value = table_info_value.with_context(|| error::TableInfoNotFoundSnafu {
        table_name: table_ref.to_string(),
    })?;

    let table_route_value = table_route_value.with_context(|| error::TableRouteNotFoundSnafu {
        table_name: table_ref.to_string(),
    })?;

    let table_route = table_route_value.into_inner().region_routes;

    let id = ddl_manager
        .submit_truncate_table_task(
            cluster_id,
            truncate_table_task,
            table_info_value,
            table_route,
        )
        .await?;

    info!("Table: {table_id} is truncated via procedure_id {id:?}");

    Ok(SubmitDdlTaskResponse {
        key: id.to_string().into(),
        ..Default::default()
    })
}

async fn handle_alter_table_task(
    ddl_manager: &DdlManager,
    cluster_id: u64,
    alter_table_task: AlterTableTask,
) -> Result<SubmitDdlTaskResponse> {
    let table_ref = alter_table_task.table_ref();

    let table_id = ddl_manager
        .table_metadata_manager
        .table_name_manager()
        .get(TableNameKey::new(
            table_ref.catalog,
            table_ref.schema,
            table_ref.table,
        ))
        .await?
        .with_context(|| TableNotFoundSnafu {
            table_name: table_ref.to_string(),
        })?
        .table_id();

    let table_info_value = ddl_manager
        .table_metadata_manager()
        .table_info_manager()
        .get(table_id)
        .await?
        .with_context(|| error::TableInfoNotFoundSnafu {
            table_name: table_ref.to_string(),
        })?;

    let id = ddl_manager
        .submit_alter_table_task(cluster_id, alter_table_task, table_info_value)
        .await?;

    info!("Table: {table_id} is altered via procedure_id {id:?}");

    Ok(SubmitDdlTaskResponse {
        key: id.to_string().into(),
        ..Default::default()
    })
}

async fn handle_drop_table_task(
    ddl_manager: &DdlManager,
    cluster_id: u64,
    drop_table_task: DropTableTask,
) -> Result<SubmitDdlTaskResponse> {
    let table_id = drop_table_task.table_id;
    let table_metadata_manager = &ddl_manager.table_metadata_manager();
    let table_ref = drop_table_task.table_ref();

    let (table_info_value, table_route_value) =
        table_metadata_manager.get_full_table_info(table_id).await?;

    let table_info_value = table_info_value.with_context(|| error::TableInfoNotFoundSnafu {
        table_name: table_ref.to_string(),
    })?;

    let table_route_value = table_route_value.with_context(|| error::TableRouteNotFoundSnafu {
        table_name: table_ref.to_string(),
    })?;

    let id = ddl_manager
        .submit_drop_table_task(
            cluster_id,
            drop_table_task,
            table_info_value,
            table_route_value,
        )
        .await?;

    info!("Table: {table_id} is dropped via procedure_id {id:?}");

    Ok(SubmitDdlTaskResponse {
        key: id.to_string().into(),
        ..Default::default()
    })
}

async fn handle_create_table_task(
    ddl_manager: &DdlManager,
    cluster_id: u64,
    mut create_table_task: CreateTableTask,
) -> Result<SubmitDdlTaskResponse> {
    let (table_id, region_routes) = ddl_manager
        .table_meta_allocator
        .create(
            &TableMetadataAllocatorContext { cluster_id },
            &mut create_table_task.table_info,
            &create_table_task.partitions,
        )
        .await?;

    let id = ddl_manager
        .submit_create_table_task(cluster_id, create_table_task, region_routes)
        .await?;

    info!("Table: {table_id:?} is created via procedure_id {id:?}");

    Ok(SubmitDdlTaskResponse {
        key: id.to_string().into(),
        table_id: Some(table_id),
    })
}

#[async_trait::async_trait]
impl DdlTaskExecutor for DdlManager {
    async fn submit_ddl_task(
        &self,
        ctx: &ExecutorContext,
        request: SubmitDdlTaskRequest,
    ) -> Result<SubmitDdlTaskResponse> {
        let span = ctx
            .tracing_context
            .as_ref()
            .map(TracingContext::from_w3c)
            .unwrap_or(TracingContext::from_current_span())
            .attach(tracing::info_span!("DdlManager::submit_ddl_task"));
        async move {
            let cluster_id = ctx.cluster_id.unwrap_or_default();
            info!("Submitting Ddl task: {:?}", request.task);
            match request.task {
                CreateTable(create_table_task) => {
                    handle_create_table_task(self, cluster_id, create_table_task).await
                }
                DropTable(drop_table_task) => {
                    handle_drop_table_task(self, cluster_id, drop_table_task).await
                }
                AlterTable(alter_table_task) => {
                    handle_alter_table_task(self, cluster_id, alter_table_task).await
                }
                TruncateTable(truncate_table_task) => {
                    handle_truncate_table_task(self, cluster_id, truncate_table_task).await
                }
            }
        }
        .trace(span)
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use api::v1::meta::Partition;
    use common_procedure::local::LocalManager;
    use table::metadata::{RawTableInfo, TableId};

    use super::DdlManager;
    use crate::cache_invalidator::DummyCacheInvalidator;
    use crate::datanode_manager::{DatanodeManager, DatanodeRef};
    use crate::ddl::alter_table::AlterTableProcedure;
    use crate::ddl::create_table::CreateTableProcedure;
    use crate::ddl::drop_table::DropTableProcedure;
    use crate::ddl::truncate_table::TruncateTableProcedure;
    use crate::ddl::{TableMetadataAllocator, TableMetadataAllocatorContext};
    use crate::error::Result;
    use crate::key::TableMetadataManager;
    use crate::kv_backend::memory::MemoryKvBackend;
    use crate::peer::Peer;
    use crate::region_keeper::MemoryRegionKeeper;
    use crate::rpc::router::RegionRoute;
    use crate::state_store::KvStateStore;

    /// A dummy implemented [DatanodeManager].
    pub struct DummyDatanodeManager;

    #[async_trait::async_trait]
    impl DatanodeManager for DummyDatanodeManager {
        async fn datanode(&self, _datanode: &Peer) -> DatanodeRef {
            unimplemented!()
        }
    }

    /// A dummy implemented [TableMetadataAllocator].
    pub struct DummyTableMetadataAllocator;

    #[async_trait::async_trait]
    impl TableMetadataAllocator for DummyTableMetadataAllocator {
        async fn create(
            &self,
            _ctx: &TableMetadataAllocatorContext,
            _table_info: &mut RawTableInfo,
            _partitions: &[Partition],
        ) -> Result<(TableId, Vec<RegionRoute>)> {
            unimplemented!()
        }
    }

    #[test]
    fn test_try_new() {
        let kv_backend = Arc::new(MemoryKvBackend::new());
        let table_metadata_manager = Arc::new(TableMetadataManager::new(kv_backend.clone()));

        let state_store = Arc::new(KvStateStore::new(kv_backend));
        let procedure_manager = Arc::new(LocalManager::new(Default::default(), state_store));

        let _ = DdlManager::try_new(
            procedure_manager.clone(),
            Arc::new(DummyDatanodeManager),
            Arc::new(DummyCacheInvalidator),
            table_metadata_manager,
            Arc::new(DummyTableMetadataAllocator),
            Arc::new(MemoryRegionKeeper::default()),
        );

        let expected_loaders = vec![
            CreateTableProcedure::TYPE_NAME,
            AlterTableProcedure::TYPE_NAME,
            DropTableProcedure::TYPE_NAME,
            TruncateTableProcedure::TYPE_NAME,
        ];

        for loader in expected_loaders {
            assert!(procedure_manager.contains_loader(loader));
        }
    }
}
