// Copyright (C) 2021 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use itertools::Itertools;
use quickwit_actors::{create_mailbox, Actor, ActorExitStatus, CommandOrMessage, Inbox, Mailbox};
use quickwit_config::KinesisSourceParams;
use quickwit_metastore::checkpoint::{CheckpointDelta, PartitionId, Position, SourceCheckpoint};
use rusoto_core::Region;
use rusoto_kinesis::{Kinesis, KinesisClient, Shard};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::api::list_shards;
use super::shard_consumer::{ShardConsumer, ShardConsumerHandle, ShardConsumerMessage};
use crate::models::RawDocBatch;
use crate::source::{IndexerMessage, Source, SourceContext};

const TARGET_BATCH_NUM_BYTES: u64 = 5_000_000;

type ShardId = String;

/// Sequence number
type SeqNo = String;

#[derive(Default)]
pub struct KinesisSourceState {
    assigned_shard_ids: HashMap<ShardId, PartitionId>,
    current_positions: HashMap<ShardId, Position>,
    shard_consumers: HashMap<ShardId, ShardConsumerHandle>,
    /// Number of active shards, i.e., that have not reached EOF.
    pub num_active_shards: usize,
    /// Number of bytes processed by the source.
    pub num_bytes_processed: u64,
    /// Number of records processed by the source (including invalid messages).
    pub num_records_processed: u64,
    // Number of invalid records, i.e., that were empty or could not be parsed.
    pub num_invalid_records: u64,
}

pub(crate) struct KinesisSource {
    stream_name: String,
    kinesis_client: KinesisClient,
    sink_tx: mpsc::Sender<ShardConsumerMessage>,
    sink_rx: mpsc::Receiver<ShardConsumerMessage>,
    state: KinesisSourceState,
}

impl fmt::Debug for KinesisSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KinesisSource {{ stream_name: {} }}", self.stream_name)
    }
}

impl KinesisSource {
    /// Instantiates a new `KinesisSource`.
    pub async fn try_new(
        params: KinesisSourceParams,
        checkpoint: SourceCheckpoint,
    ) -> anyhow::Result<Self> {
        let stream_name = params.stream_name;
        let region = get_region(params.region, params.endpoint);
        let kinesis_client = KinesisClient::new(region);
        let (sink_tx, sink_rx) = mpsc::channel(1_000);
        let shards = list_shards(&kinesis_client, &stream_name, None).await?;
        let assigned_shard_ids: HashMap<ShardId, PartitionId> = shards
            .into_iter()
            .map(|shard| (shard.shard_id.clone(), PartitionId::from(shard.shard_id)))
            .collect();
        let current_positions: HashMap<ShardId, Position> = assigned_shard_ids
            .iter()
            .flat_map(|(shard_id, partition_id)| {
                checkpoint
                    .position_for_partition(partition_id)
                    .map(|position| (shard_id.clone(), position.clone()))
            })
            .collect();
        let num_active_shards = assigned_shard_ids.len();

        // debug!(
        //     topic = %topic,
        //     assignment = ?assignment,
        //     "Starting Kafka source."
        // );

        let state = KinesisSourceState {
            assigned_shard_ids,
            current_positions,
            num_active_shards,
            ..Default::default()
        };
        Ok(KinesisSource {
            stream_name,
            kinesis_client,
            sink_tx,
            sink_rx,
            state,
        })
    }

    fn spawn_shard_consumer(&mut self, ctx: &SourceContext, shard_id: ShardId) {
        assert!(!self.state.shard_consumers.contains_key(&shard_id));
        let from_sequence_number_exclusive = self
            .state
            .current_positions
            .get(&shard_id)
            .map(|position| position.as_str().to_string());
        let shard_consumer = ShardConsumer::new(
            self.stream_name.clone(),
            shard_id.clone(),
            from_sequence_number_exclusive,
            true,
            Box::new(self.kinesis_client.clone()),
            self.sink_tx.clone(),
        );
        let shard_consumer_handle = shard_consumer.spawn(ctx);
        self.state
            .shard_consumers
            .insert(shard_id, shard_consumer_handle);
    }

    fn supervise_shard_consumers(&mut self) {
        // TODO
    }
}

#[async_trait]
impl Source for KinesisSource {
    async fn initialize(&mut self, ctx: &SourceContext) -> Result<(), ActorExitStatus> {
        let shard_ids: Vec<ShardId> = self.state.assigned_shard_ids.keys().cloned().collect();
        for shard_id in shard_ids {
            self.spawn_shard_consumer(ctx, shard_id);
        }
        Ok(())
    }

    async fn emit_batches(
        &mut self,
        batch_sink: &Mailbox<IndexerMessage>,
        ctx: &SourceContext,
    ) -> Result<(), ActorExitStatus> {
        let start = Instant::now();
        self.supervise_shard_consumers();

        let mut batch_num_bytes = 0;
        let mut docs = Vec::new();
        let mut checkpoint_delta = CheckpointDelta::default();

        while start.elapsed() < quickwit_actors::HEARTBEAT / 2 {
            // TODO: timeout
            match self.sink_rx.recv().await {
                Some(ShardConsumerMessage::ChildShards(shard_ids)) => {
                    for shard_id in shard_ids {
                        self.spawn_shard_consumer(ctx, shard_id);
                        self.state.num_active_shards += 1;
                    }
                }
                Some(ShardConsumerMessage::Records {
                    shard_id,
                    records,
                    lag_millis,
                }) => {
                    let records_len = records.len();
                    for (i, record) in records.into_iter().enumerate() {
                        match String::from_utf8(record.data.to_vec()) {
                            Ok(doc) => docs.push(doc),
                            Err(error) => {
                                warn!(error = ?error, "Record contains invalid UTF-8 characters.");
                                self.state.num_invalid_records += 1;
                            }
                        };
                        batch_num_bytes += record.data.len() as u64;
                        self.state.num_bytes_processed += record.data.len() as u64;
                        self.state.num_records_processed += 1;

                        if i == records_len - 1 {
                            let partition_id = self
                                .state
                                .assigned_shard_ids
                                .get(&shard_id)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "Received record from unassigned shard `{}`. Assigned \
                                         shards: `{{{}}}`.",
                                        shard_id,
                                        self.state.assigned_shard_ids.keys().join(", "),
                                    )
                                })?
                                .clone();
                            let current_position = Position::from(record.sequence_number);
                            let previous_position = self
                                .state
                                .current_positions
                                .insert(shard_id.clone(), current_position.clone())
                                .unwrap_or(Position::Beginning);

                            checkpoint_delta
                                .record_partition_delta(
                                    partition_id,
                                    previous_position,
                                    current_position,
                                )
                                .context("Failed to record partition delta.")?;
                        }
                    }
                    if batch_num_bytes >= TARGET_BATCH_NUM_BYTES {
                        break;
                    }
                }
                Some(ShardConsumerMessage::ShardClosed(shard_id)) => {
                    self.state.num_active_shards -= 1;
                    self.state.shard_consumers.remove(&shard_id);
                }
                Some(ShardConsumerMessage::ShardEOF(shard_id)) => {
                    self.state.num_active_shards -= 1;
                    self.state.shard_consumers.remove(&shard_id);
                }
                _ => (),
            }
        }
        if !checkpoint_delta.is_empty() {
            let batch = RawDocBatch {
                docs,
                checkpoint_delta,
            };
            ctx.send_message(batch_sink, IndexerMessage::from(batch))
                .await?;
        }
        if self.state.num_active_shards == 0 {
            info!(stream_name = %self.stream_name, "Reached end of stream.");
            ctx.send_exit_with_success(batch_sink).await?;
            return Err(ActorExitStatus::Success);
        }
        Ok(())
    }

    fn name(&self) -> String {
        "KinesisSource".to_string()
    }

    fn observable_state(&self) -> serde_json::Value {
        let assigned_shard_ids: Vec<&ShardId> =
            self.state.assigned_shard_ids.keys().sorted().collect();
        let current_positions: Vec<(&ShardId, &str)> = self
            .state
            .current_positions
            .iter()
            .map(|(partition_id, position)| (partition_id, position.as_str()))
            .sorted()
            .collect();
        json!({
            "stream_name": self.stream_name,
            "assigned_shard_ids": assigned_shard_ids,
            "current_positions": current_positions,
            "num_active_shards": self.state.num_active_shards,
            "num_bytes_processed": self.state.num_bytes_processed,
            "num_records_processed": self.state.num_records_processed,
            "num_invalid_records": self.state.num_invalid_records,
        })
    }
}

fn get_region(region_opt: Option<String>, endpoint_opt: Option<String>) -> Region {
    if let Some(endpoint) = endpoint_opt {
        return Region::Custom {
            name: "Custom".to_string(),
            endpoint,
        };
    }
    if let Some(Ok(region)) = region_opt.map(|rgn| rgn.parse()) {
        return region;
    }
    Region::default()
}

#[cfg(all(test, feature = "kinesis-localstack-tests"))]
mod tests {
    use quickwit_actors::{create_test_mailbox, Universe};
    use quickwit_metastore::checkpoint;

    use super::*;
    use crate::source::kinesis::api::tests::{merge_shards, split_shard};
    use crate::source::kinesis::helpers::tests::{
        make_shard_id, put_records_into_shards, setup, teardown,
    };
    use crate::source::SourceActor;

    fn merge_messages(messages: Vec<IndexerMessage>) -> anyhow::Result<RawDocBatch> {
        let mut merged_batch = RawDocBatch::default();
        for message in messages {
            if let IndexerMessage::Batch(batch) = message {
                merged_batch.docs.extend(batch.docs);
                merged_batch
                    .checkpoint_delta
                    .extend(batch.checkpoint_delta)?;
            }
        }
        merged_batch.docs.sort();
        Ok(merged_batch)
    }

    #[tokio::test]
    async fn test_kinesis_source() {
        let universe = Universe::new();
        let (mailbox, inbox) = create_test_mailbox();
        let (kinesis_client, stream_name) = setup("test-kinesis-source", 3).await.unwrap();
        let params = KinesisSourceParams {
            stream_name: stream_name.clone(),
            region: None,
            endpoint: Some("http://localhost:4566".to_string()),
        };
        {
            let checkpoint = SourceCheckpoint::default();
            let kinesis_source = KinesisSource::try_new(params.clone(), checkpoint)
                .await
                .unwrap();
            let actor = SourceActor {
                source: Box::new(kinesis_source),
                batch_sink: mailbox.clone(),
            };
            let (_mailbox, handle) = universe.spawn_actor(actor).spawn_async();
            let (exit_status, exit_state) = handle.join().await;
            assert!(exit_status.is_success());

            let messages = inbox.drain_available_message_for_test();
            assert!(messages.is_empty());

            let expected_current_positions: Vec<(ShardId, SeqNo)> = vec![];
            let expected_state = json!({
                "stream_name":  stream_name,
                "assigned_shard_ids": vec![make_shard_id(0), make_shard_id(1), make_shard_id(2)],
                "current_positions":  expected_current_positions,
                "num_active_shards": 0,
                "num_bytes_processed": 0,
                "num_records_processed": 0,
                "num_invalid_records": 0,
            });
            assert_eq!(exit_state, expected_state);
        }
        let sequence_numbers = put_records_into_shards(
            &kinesis_client,
            &stream_name,
            [
                (0, "Record #00"),
                (0, "Record #01"),
                (1, "Record #10"),
                (1, "Record #11"),
                (2, "Record #20"),
                (2, "Record #21"),
            ],
        )
        .await
        .unwrap();
        let current_sequence_numbers: HashMap<usize, SeqNo> = sequence_numbers
            .iter()
            .map(|(shard_id, records)| (shard_id.clone(), records.last().unwrap().clone()))
            .collect();
        let current_positions: HashMap<usize, Position> = current_sequence_numbers
            .iter()
            .map(|(shard_id, seqno)| (shard_id.clone(), Position::from(seqno.clone())))
            .collect();
        {
            let checkpoint = SourceCheckpoint::default();
            let kinesis_source = KinesisSource::try_new(params.clone(), checkpoint)
                .await
                .unwrap();
            let actor = SourceActor {
                source: Box::new(kinesis_source),
                batch_sink: mailbox.clone(),
            };
            let (_mailbox, handle) = universe.spawn_actor(actor).spawn_async();
            let (exit_status, exit_state) = handle.join().await;
            assert!(exit_status.is_success());

            let messages = inbox.drain_available_message_for_test();
            assert!(messages.len() >= 1);

            let batch = merge_messages(messages).unwrap();
            let expected_docs = vec![
                "Record #00",
                "Record #01",
                "Record #10",
                "Record #11",
                "Record #20",
                "Record #21",
            ];
            assert_eq!(batch.docs, expected_docs);

            let mut expected_checkpoint_delta = CheckpointDelta::default();
            for shard_id in 0..3 {
                expected_checkpoint_delta
                    .record_partition_delta(
                        PartitionId::from(make_shard_id(shard_id)),
                        Position::Beginning,
                        current_positions.get(&shard_id).unwrap().clone(),
                    )
                    .unwrap();
            }
            assert_eq!(batch.checkpoint_delta, expected_checkpoint_delta);

            let expected_current_positions: Vec<(ShardId, String)> = current_sequence_numbers
                .iter()
                .map(|(shard_id, sequence_number)| {
                    (make_shard_id(*shard_id), sequence_number.clone())
                })
                .sorted()
                .collect();

            let expected_state = json!({
                "stream_name":  stream_name,
                "assigned_shard_ids": vec![make_shard_id(0), make_shard_id(1), make_shard_id(2)],
                "current_positions":  expected_current_positions,
                "num_active_shards": 0,
                "num_bytes_processed": 60,
                "num_records_processed": 6,
                "num_invalid_records": 0,
            });
            assert_eq!(exit_state, expected_state);
        }
        {
            let from_sequence_number_exclusive_shard_1 =
                sequence_numbers.get(&1).unwrap().first().unwrap().clone();
            let from_sequence_number_exclusive_shard_2 =
                sequence_numbers.get(&2).unwrap().last().unwrap().clone();
            let checkpoint: SourceCheckpoint = vec![
                (
                    make_shard_id(1),
                    from_sequence_number_exclusive_shard_1.clone(),
                ),
                (
                    make_shard_id(2),
                    from_sequence_number_exclusive_shard_2.clone(),
                ),
            ]
            .into_iter()
            .map(|(partition_id, offset)| (PartitionId::from(partition_id),
Position::from(offset)))             .collect();
            let kinesis_source = KinesisSource::try_new(params.clone(), checkpoint)
                .await
                .unwrap();
            let actor = SourceActor {
                source: Box::new(kinesis_source),
                batch_sink: mailbox,
            };
            let (_mailbox, handle) = universe.spawn_actor(actor).spawn_async();
            let (exit_status, exit_state) = handle.join().await;
            assert!(exit_status.is_success());

            let messages = inbox.drain_available_message_for_test();
            assert!(messages.len() >= 1);

            let batch = merge_messages(messages).unwrap();
            let expected_docs = vec!["Record #00", "Record #01", "Record #11"];
            assert_eq!(batch.docs, expected_docs);

            let mut expected_checkpoint_delta = CheckpointDelta::default();
            for (shard_id, from_position) in [
                Position::Beginning,
                Position::from(from_sequence_number_exclusive_shard_1),
            ]
            .into_iter()
            .enumerate()
            {
                expected_checkpoint_delta
                    .record_partition_delta(
                        PartitionId::from(make_shard_id(shard_id)),
                        from_position,
                        current_positions.get(&shard_id).unwrap().clone(),
                    )
                    .unwrap();
            }
            assert_eq!(batch.checkpoint_delta, expected_checkpoint_delta);

            let expected_current_positions: Vec<(ShardId, String)> = current_sequence_numbers
                .iter()
                .map(|(shard_id, sequence_number)| {
                    (make_shard_id(*shard_id), sequence_number.clone())
                })
                .sorted()
                .collect();
            let expected_state = json!({
                "stream_name":  stream_name,
                "assigned_shard_ids": vec![make_shard_id(0), make_shard_id(1), make_shard_id(2)],
                "current_positions":  expected_current_positions,
                "num_active_shards": 0,
                "num_bytes_processed": 30,
                "num_records_processed": 3,
                "num_invalid_records": 0,
            });
            assert_eq!(exit_state, expected_state);
        }
        teardown(&kinesis_client, &stream_name).await;
    }
}
