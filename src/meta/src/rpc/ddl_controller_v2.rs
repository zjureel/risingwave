// Copyright 2024 RisingWave Labs
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

use itertools::Itertools;
use risingwave_common::util::column_index_mapping::ColIndexMapping;
use risingwave_common::util::stream_graph_visitor::visit_fragment;
use risingwave_pb::catalog::CreateType;
use risingwave_pb::ddl_service::TableJobType;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::update_mutation::PbMergeUpdate;
use risingwave_pb::stream_plan::StreamFragmentGraph as StreamFragmentGraphProto;
use thiserror_ext::AsReport;

use crate::manager::{
    MetadataManager, MetadataManagerV2, NotificationVersion, StreamingJob,
    IGNORED_NOTIFICATION_VERSION,
};
use crate::model::{MetadataModel, StreamContext};
use crate::rpc::ddl_controller::{fill_table_stream_graph_info, DdlController};
use crate::stream::{validate_sink, StreamFragmentGraph};
use crate::MetaResult;

impl DdlController {
    pub async fn create_streaming_job_v2(
        &self,
        mut streaming_job: StreamingJob,
        mut fragment_graph: StreamFragmentGraphProto,
    ) -> MetaResult<NotificationVersion> {
        let MetadataManager::V2(mgr) = &self.metadata_manager else {
            unreachable!("MetadataManager should be V2")
        };

        let ctx = StreamContext::from_protobuf(fragment_graph.get_ctx().unwrap());
        mgr.catalog_controller
            .create_job_catalog(&mut streaming_job, &ctx)
            .await?;
        let job_id = streaming_job.id();

        match &mut streaming_job {
            StreamingJob::Table(src, table, job_type) => {
                // If we're creating a table with connector, we should additionally fill its ID first.
                fill_table_stream_graph_info(src, table, *job_type, &mut fragment_graph);
            }
            StreamingJob::Source(src) => {
                // set the inner source id of source node.
                for fragment in fragment_graph.fragments.values_mut() {
                    visit_fragment(fragment, |node_body| {
                        if let NodeBody::Source(source_node) = node_body {
                            source_node.source_inner.as_mut().unwrap().source_id = src.id;
                        }
                    });
                }
            }
            _ => {}
        }

        tracing::debug!(
            id = job_id,
            definition = streaming_job.definition(),
            "starting streaming job",
        );
        let _permit = self
            .creating_streaming_job_permits
            .semaphore
            .acquire()
            .await
            .unwrap();
        let _reschedule_job_lock = self.stream_manager.reschedule_lock.read().await;

        // create streaming job.
        match self
            .create_streaming_job_inner_v2(mgr, ctx, &mut streaming_job, fragment_graph)
            .await
        {
            Ok(version) => Ok(version),
            Err(err) => {
                tracing::error!(id = job_id, error = ?err.as_report(), "failed to create streaming job");
                let aborted = mgr
                    .catalog_controller
                    .try_abort_creating_streaming_job(job_id as _)
                    .await?;
                if aborted {
                    tracing::warn!(id = job_id, "aborted streaming job");
                    match &streaming_job {
                        StreamingJob::Table(Some(src), _, _) | StreamingJob::Source(src) => {
                            self.source_manager.unregister_sources(vec![src.id]).await;
                        }
                        _ => {}
                    }
                }
                Err(err)
            }
        }
    }

    async fn create_streaming_job_inner_v2(
        &self,
        mgr: &MetadataManagerV2,
        ctx: StreamContext,
        streaming_job: &mut StreamingJob,
        fragment_graph: StreamFragmentGraphProto,
    ) -> MetaResult<NotificationVersion> {
        let mut fragment_graph =
            StreamFragmentGraph::new(&self.env, fragment_graph, streaming_job).await?;
        streaming_job.set_table_fragment_id(fragment_graph.table_fragment_id());
        streaming_job.set_dml_fragment_id(fragment_graph.dml_fragment_id());

        // create internal table catalogs and refill table id.
        let internal_tables = fragment_graph.internal_tables().into_values().collect_vec();
        let table_id_map = mgr
            .catalog_controller
            .create_internal_table_catalog(streaming_job.id() as _, internal_tables)
            .await?;
        fragment_graph.refill_internal_table_ids(table_id_map);

        // create fragment and actor catalogs.
        tracing::debug!(id = streaming_job.id(), "building streaming job");
        let (ctx, table_fragments) = self
            .build_stream_job(ctx, streaming_job, fragment_graph, None)
            .await?;

        match streaming_job {
            StreamingJob::Table(None, table, TableJobType::SharedCdcSource) => {
                Self::validate_cdc_table(table, &table_fragments).await?;
            }
            StreamingJob::Table(Some(source), ..) => {
                // Register the source on the connector node.
                self.source_manager.register_source(source).await?;
            }
            StreamingJob::Sink(sink, target_table) => {
                if target_table.is_some() {
                    unimplemented!("support create sink into table in v2");
                }
                // Validate the sink on the connector node.
                validate_sink(sink).await?;
            }
            StreamingJob::Source(source) => {
                // Register the source on the connector node.
                self.source_manager.register_source(source).await?;
            }
            _ => {}
        }

        mgr.catalog_controller
            .prepare_streaming_job(table_fragments.to_protobuf(), streaming_job, false)
            .await?;

        // create streaming jobs.
        let stream_job_id = streaming_job.id();
        match streaming_job.create_type() {
            CreateType::Unspecified | CreateType::Foreground => {
                self.stream_manager
                    .create_streaming_job(table_fragments, ctx)
                    .await?;
                let version = mgr
                    .catalog_controller
                    .finish_streaming_job(stream_job_id as _)
                    .await?;
                Ok(version)
            }
            CreateType::Background => {
                let ctrl = self.clone();
                let mgr = mgr.clone();
                let fut = async move {
                    let result = ctrl
                        .stream_manager
                        .create_streaming_job(table_fragments, ctx)
                        .await.inspect_err(|err| {
                            tracing::error!(id = stream_job_id, error = ?err.as_report(), "failed to create background streaming job");
                        });
                    if result.is_ok() {
                        let _ = mgr
                            .catalog_controller
                            .finish_streaming_job(stream_job_id as _)
                            .await.inspect_err(|err| {
                                tracing::error!(id = stream_job_id, error = ?err.as_report(), "failed to finish background streaming job");
                            });
                    }
                };
                tokio::spawn(fut);
                Ok(IGNORED_NOTIFICATION_VERSION)
            }
        }
    }

    /// This is used for `ALTER TABLE ADD/DROP COLUMN`.
    pub async fn replace_table_v2(
        &self,
        mut streaming_job: StreamingJob,
        fragment_graph: StreamFragmentGraphProto,
        table_col_index_mapping: Option<ColIndexMapping>,
    ) -> MetaResult<NotificationVersion> {
        let MetadataManager::V2(mgr) = &self.metadata_manager else {
            unreachable!("MetadataManager should be V2")
        };
        let job_id = streaming_job.id();

        let _reschedule_job_lock = self.stream_manager.reschedule_lock.read().await;
        let ctx = StreamContext::from_protobuf(fragment_graph.get_ctx().unwrap());

        // 1. build fragment graph.
        let fragment_graph =
            StreamFragmentGraph::new(&self.env, fragment_graph, &streaming_job).await?;
        streaming_job.set_table_fragment_id(fragment_graph.table_fragment_id());
        streaming_job.set_dml_fragment_id(fragment_graph.dml_fragment_id());
        let streaming_job = streaming_job;

        let StreamingJob::Table(_, table, ..) = &streaming_job else {
            unreachable!("unexpected job: {streaming_job:?}")
        };
        let dummy_id = mgr
            .catalog_controller
            .create_job_catalog_for_replace(&streaming_job, &ctx, table.get_version()?)
            .await?;

        tracing::debug!(id = streaming_job.id(), "building replace streaming job");
        let result: MetaResult<Vec<PbMergeUpdate>> = try {
            let (ctx, table_fragments) = self
                .build_replace_table(
                    ctx,
                    &streaming_job,
                    fragment_graph,
                    table_col_index_mapping.clone(),
                    dummy_id as _,
                )
                .await?;
            let merge_updates = ctx.merge_updates.clone();

            mgr.catalog_controller
                .prepare_streaming_job(table_fragments.to_protobuf(), &streaming_job, true)
                .await?;

            self.stream_manager
                .replace_table(table_fragments, ctx)
                .await?;
            merge_updates
        };

        match result {
            Ok(merge_updates) => {
                let version = mgr
                    .catalog_controller
                    .finish_replace_streaming_job(
                        dummy_id,
                        streaming_job,
                        merge_updates,
                        table_col_index_mapping,
                        None,
                        None,
                    )
                    .await?;
                Ok(version)
            }
            Err(err) => {
                tracing::error!(id = job_id, error = ?err.as_report(), "failed to replace table");
                let _ = mgr
                    .catalog_controller
                    .try_abort_replacing_streaming_job(dummy_id)
                    .await.inspect_err(|err| {
                        tracing::error!(id = job_id, error = ?err.as_report(), "failed to abort replacing table");
                    });
                Err(err)
            }
        }
    }
}
