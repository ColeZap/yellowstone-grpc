use {
    super::producer::ScyllaProducerStore,
    crate::scylladb::{
        scylladb_utils::LwtResult,
        types::{
            BlockchainEventType, CommitmentLevel, ConsumerGroupId, ConsumerGroupInfo,
            ConsumerGroupType, ConsumerId, ProducerId, ShardId, ShardOffset, ShardOffsetMap, Slot, TranslationStrategy,
        },
        yellowstone_log::{common::SeekLocation, consumer_group::error::StaleRevision},
    },
    rdkafka::producer,
    scylla::{prepared_statement::PreparedStatement, statement::Consistency, Session},
    std::{collections::BTreeMap, net::IpAddr, sync::Arc},
    tonic::async_trait,
    tracing::info,
    uuid::Uuid,
};

const NUM_SHARDS: usize = 64;

const INSERT_STATIC_GROUP_MEMBER_OFFSETS: &str = r###"
    INSERT INTO consumer_shard_offset_v2 (
        consumer_group_id, 
        consumer_id, 
        producer_id, 
        acc_shard_offset_map,
        tx_shard_offset_map,
        revision, 
        created_at, 
        updated_at
    )
    VALUES (?, ?, ?, ?, ?, ?, currentTimestamp(), currentTimestamp())
    IF NOT EXISTS
"###;

const CREATE_STATIC_CONSUMER_GROUP: &str = r###"
    INSERT INTO consumer_groups (
        consumer_group_id,
        group_type,
        producer_id,
        commitment_level,
        subscribed_event_types,
        instance_id_shard_assignments,
        last_access_ip_address,
        revision,
        translation_strategy,
        created_at,
        updated_at
    )
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, currentTimestamp(), currentTimestamp())
"###;

const GET_STATIC_CONSUMER_GROUP: &str = r###"
    SELECT
        consumer_group_id,
        group_type,
        producer_id,
        revision,
        commitment_level,
        subscribed_event_types,
        instance_id_shard_assignments,
        last_access_ip_address,
        translation_strategy
    FROM consumer_groups
    WHERE consumer_group_id = ?
"###;

const UPDATE_STATIC_CONSUMER_GROUP: &str = r###"
    UPDATE consumer_groups
    SET producer_id = ?,
        revision = ?
    WHERE consumer_group_id = ?
    IF revision < ?
"###;

const UPDATE_CONSUMER_SHARD_OFFSET_V2: &str = r###"
    UPDATE consumer_shard_offset_v2
    SET acc_shard_offset_map = ?, tx_shard_offset_map = ?, revision = ?
    WHERE 
        consumer_group_id = ?
        AND consumer_id = ?
        AND producer_id = ?
    IF revision < ?
"###;

const GET_ACC_UPDATE_SHARD_OFFSET: &str = r###"
    SELECT
        revision,
        acc_shard_offset_map
    FROM consumer_shard_offset_v2
    WHERE 
        consumer_group_id = ?
        AND consumer_id = ?
        AND producer_id = ?
"###;

const GET_NEW_TX_SHARD_OFFSET: &str = r###"
    SELECT
        revision,
        tx_shard_offset_map
    FROM consumer_shard_offset_v2
    WHERE 
        consumer_group_id = ?
        AND consumer_id = ?
        AND producer_id = ?
"###;

const GET_SHARD_OFFSET_FOR_SPECIFIC_GROUP_MEMBER: &str = r###"
    SELECT
        consumer_id,
        revision,
        acc_shard_offset_map,
        tx_shard_offset_map
    FROM consumer_shard_offset_v2
    WHERE 
        consumer_group_id = ?
        AND consumer_id IN ?
        AND producer_id = ?
"###;

#[derive(Clone)]
pub struct ScyllaConsumerGroupStore {
    session: Arc<Session>,
    producer_store: ScyllaProducerStore,
    create_static_consumer_group_ps: PreparedStatement,
    get_static_consumer_group_ps: PreparedStatement,
    update_static_consumer_group_ps: PreparedStatement,
    insert_consumer_shard_offset_ps_if_not_exists: PreparedStatement,
    update_consumer_shard_offset_ps: PreparedStatement,
    get_acc_update_shard_offset_ps: PreparedStatement,
    get_new_tx_shard_offset_ps: PreparedStatement,
    get_shard_offset_for_specific_group_member_ps: PreparedStatement,
}

fn assign_shards(ids: &[ConsumerId], num_shards: usize) -> BTreeMap<ConsumerId, Vec<ShardId>> {
    let mut ids = ids.to_vec();
    ids.sort();

    let num_parts_per_id = num_shards / ids.len();
    let shard_vec = (0..num_shards).map(|x| x as ShardId).collect::<Vec<_>>();
    let chunk_it = shard_vec
        .chunks(num_parts_per_id)
        .map(|chunk| chunk.to_vec());

    ids.into_iter().zip(chunk_it).collect()
}

impl ScyllaConsumerGroupStore {
    pub async fn new(
        session: Arc<Session>,
        producer_store: ScyllaProducerStore,
    ) -> anyhow::Result<Self> {
        let create_static_consumer_group_ps = session.prepare(CREATE_STATIC_CONSUMER_GROUP).await?;

        let mut get_static_consumer_group_ps = session.prepare(GET_STATIC_CONSUMER_GROUP).await?;
        get_static_consumer_group_ps.set_consistency(Consistency::Serial);

        let update_static_consumer_group_ps = session.prepare(UPDATE_STATIC_CONSUMER_GROUP).await?;

        let insert_consumer_shard_offset_ps_if_not_exists =
            session.prepare(INSERT_STATIC_GROUP_MEMBER_OFFSETS).await?;
        let update_consumer_shard_offset_ps =
            session.prepare(UPDATE_CONSUMER_SHARD_OFFSET_V2).await?;

        let mut get_acc_update_shard_offset_ps =
            session.prepare(GET_ACC_UPDATE_SHARD_OFFSET).await?;
        let mut get_new_tx_shard_offset_ps = session.prepare(GET_NEW_TX_SHARD_OFFSET).await?;

        let get_shard_offset_for_specific_group_member_ps = session
            .prepare(GET_SHARD_OFFSET_FOR_SPECIFIC_GROUP_MEMBER)
            .await?;

        get_acc_update_shard_offset_ps.set_consistency(Consistency::Serial);
        get_new_tx_shard_offset_ps.set_consistency(Consistency::Serial);

        let this = ScyllaConsumerGroupStore {
            session: Arc::clone(&session),
            create_static_consumer_group_ps,
            get_static_consumer_group_ps,
            producer_store,
            update_static_consumer_group_ps,
            insert_consumer_shard_offset_ps_if_not_exists,
            update_consumer_shard_offset_ps,
            get_acc_update_shard_offset_ps,
            get_new_tx_shard_offset_ps,
            get_shard_offset_for_specific_group_member_ps,
        };
        Ok(this)
    }

    pub async fn get_shard_offset_map(
        &self,
        consumer_group_id: &ConsumerGroupId,
        consumer_id: &ConsumerId,
        producer_id: &ProducerId,
        blockchain_event_type: BlockchainEventType,
    ) -> anyhow::Result<(i64, ShardOffsetMap)> {
        let ps = match blockchain_event_type {
            BlockchainEventType::AccountUpdate => &self.get_acc_update_shard_offset_ps,
            BlockchainEventType::NewTransaction => &self.get_new_tx_shard_offset_ps,
        };
        let bind_values = (consumer_group_id, consumer_id, producer_id);

        let row = self
            .session
            .execute(ps, bind_values)
            .await?
            .first_row_typed::<(i64, ShardOffsetMap)>()?;

        Ok(row)
    }

    pub async fn update_consumer_group_producer(
        &self,
        consumer_group_id: &ConsumerGroupId,
        producer_id: &ProducerId,
        revision: i64,
    ) -> anyhow::Result<()> {
        let bind_values = (producer_id, revision, consumer_group_id, revision);
        let lwt_result = self
            .session
            .execute(&self.update_static_consumer_group_ps, bind_values)
            .await?
            .first_row_typed::<LwtResult>()?;
        anyhow::ensure!(
            lwt_result == LwtResult(true),
            "failed to update consumer group producer"
        );
        Ok(())
    }

    pub async fn get_consumer_group_info(
        &self,
        consumer_group_id: &ConsumerGroupId,
    ) -> anyhow::Result<Option<ConsumerGroupInfo>> {
        self.session
            .execute(&self.get_static_consumer_group_ps, (consumer_group_id,))
            .await?
            .maybe_first_row_typed::<ConsumerGroupInfo>()
            .map_err(anyhow::Error::new)
    }

    pub async fn get_lowest_common_slot_number(
        &self,
        consumer_group_id: &ConsumerGroupId,
        max_revision_opt: Option<i64>,
    ) -> anyhow::Result<(Slot, i64)> {
        let consumer_group_info = self
            .get_consumer_group_info(consumer_group_id)
            .await?
            .ok_or(anyhow::anyhow!("consumer group id not found"))?;
        if let Some(max_revision) = max_revision_opt {
            let remote_revision = consumer_group_info.revision;
            anyhow::ensure!(max_revision >= remote_revision, StaleRevision(max_revision));
        }
        let producer_id = consumer_group_info
            .producer_id
            .expect("cannot compute LCS of unused consumer group");
        let instance_id_in_clause = consumer_group_info
            .consumer_id_shard_assignments
            .keys()
            .cloned()
            .collect::<Vec<_>>();

        let subscribed_events = consumer_group_info.subscribed_event_types;
        let rows = self
            .session
            .execute(
                &self.get_shard_offset_for_specific_group_member_ps,
                (consumer_group_id, instance_id_in_clause, producer_id),
            )
            .await?
            .rows_typed::<(ConsumerId, i64, ShardOffsetMap, ShardOffsetMap)>()?
            .collect::<Result<Vec<_>, _>>()?;
        let shard_max_revision = rows
            .iter()
            .map(|(_, revision, _, _)| revision)
            .cloned()
            .max()
            .unwrap_or(0);
        let min_slot = rows
            .iter()
            .map(|(_, _, acc_offset, tx_offset)| {
                let min1 = if subscribed_events.contains(&BlockchainEventType::AccountUpdate) {
                    acc_offset
                        .iter()
                        .map(|(_, (_, slot))| *slot)
                        .min()
                        .unwrap_or(i64::MAX)
                } else {
                    i64::MAX
                };
                let min2 = if subscribed_events.contains(&BlockchainEventType::NewTransaction) {
                    tx_offset
                        .iter()
                        .map(|(_, (_, slot))| *slot)
                        .min()
                        .unwrap_or(i64::MAX)
                } else {
                    i64::MAX
                };
                std::cmp::min(min1, min2)
            })
            .min()
            .unwrap_or(i64::MAX);
        if let Some(max_revision) = max_revision_opt {
            anyhow::ensure!(
                max_revision >= shard_max_revision,
                StaleRevision(max_revision)
            );
        }

        anyhow::ensure!(min_slot < i64::MAX, "found not shard offset map content");
        Ok((min_slot, shard_max_revision))
    }

    /// Sets the shard offset for the static members of a consumer group.
    ///
    /// This function updates the shard offset for each consumer instance in the
    /// consumer group. It ensures that the consumer group is more up-to-date than
    /// the current operation, and that the producer ID and execution ID match the
    /// consumer group information.
    ///
    /// # Arguments
    /// * `consumer_group_id` - The ID of the consumer group.
    /// * `producer_id` - The ID of the producer.
    /// * `shard_offset_map` - A map of shard IDs to their corresponding shard offset and slot.
    /// * `current_revision` - The current revision of the consumer group.
    ///
    /// # Errors
    /// Returns an error if the consumer group does not exist, the consumer group is
    /// more up-to-date than the current operation, or the producer ID or execution
    /// ID does not match the consumer group information.
    pub async fn set_static_group_members_shard_offset(
        &self,
        consumer_group_id: &ConsumerGroupId,
        producer_id: &ProducerId,
        shard_offset_map: &BTreeMap<ShardId, (ShardOffset, Slot)>,
        current_revision: i64,
    ) -> anyhow::Result<()> {
        let cg_info = self
            .get_consumer_group_info(consumer_group_id)
            .await?
            .ok_or(anyhow::anyhow!("consumer group does not exists"))?;

        anyhow::ensure!(
            cg_info.revision <= current_revision,
            "consumer group is more up to date then current operation"
        );
        for (consumer_id, shard_ids) in cg_info.consumer_id_shard_assignments.iter() {
            let my_shard_offset_map = shard_ids
                .iter()
                .cloned()
                .map(|shard_id| {
                    (
                        shard_id,
                        shard_offset_map
                            .get(&shard_id)
                            .expect("missing shard offset")
                            .clone(),
                    )
                })
                .collect::<BTreeMap<_, _>>();

            let values = (
                consumer_group_id.clone(),
                consumer_id,
                producer_id,
                &my_shard_offset_map,
                &my_shard_offset_map,
                current_revision,
            );
            let lwt_result = self
                .session
                .execute(&self.insert_consumer_shard_offset_ps_if_not_exists, values)
                .await?
                .single_row_typed::<LwtResult>()?;
            if lwt_result == LwtResult(false) {
                let values2 = (
                    &my_shard_offset_map,
                    &my_shard_offset_map,
                    current_revision,
                    consumer_group_id.clone(),
                    consumer_id,
                    &producer_id,
                    current_revision,
                );
                let lwt_result2 = self
                    .session
                    .execute(&self.update_consumer_shard_offset_ps, values2)
                    .await?
                    .single_row_typed::<LwtResult>()?;

                anyhow::ensure!(
                    lwt_result2 == LwtResult(true),
                    "failed to update {consumer_id} shard offset"
                );
            }
        }
        Ok(())
    }

    async fn create_static_group_members(
        &self,
        consumer_group_info: &ConsumerGroupInfo,
        seek_loc: SeekLocation,
    ) -> anyhow::Result<()> {
        let producer_id = consumer_group_info
            .producer_id
            .expect("missing producer id during static group membership registration");

        let shard_offset_map = self
            .producer_store
            .compute_offset(producer_id, seek_loc)
            .await?;

        info!("Shard offset has been computed successfully");
        self.set_static_group_members_shard_offset(
            &consumer_group_info.consumer_group_id,
            &producer_id,
            &shard_offset_map,
            consumer_group_info.revision,
        )
        .await
    }

    pub async fn create_static_consumer_group(
        &self,
        consumer_ids: &[ConsumerId],
        commitment_level: CommitmentLevel,
        subscribed_blockchain_event_types: &[BlockchainEventType],
        initial_offset: SeekLocation,
        remote_ip_addr: Option<IpAddr>,
        translation_strategy: Option<TranslationStrategy>,
    ) -> anyhow::Result<ConsumerGroupInfo> {
        let consumer_group_id = Uuid::new_v4();
        let shard_assignments = assign_shards(consumer_ids, NUM_SHARDS);

        let maybe_slot_range = if let SeekLocation::SlotApprox(slot_ranges) = &initial_offset
        {
            Some(slot_ranges.clone())
        } else {
            None
        };

        let producer_id = self
            .producer_store
            .get_producer_id_with_least_assigned_consumer(maybe_slot_range, commitment_level)
            .await?;
        info!("create_static_consumer_group, computed producer_id={producer_id}");
        self.session
            .execute(
                &self.create_static_consumer_group_ps,
                (
                    consumer_group_id.as_bytes(),
                    ConsumerGroupType::Static,
                    producer_id,
                    commitment_level,
                    subscribed_blockchain_event_types,
                    &shard_assignments,
                    remote_ip_addr,
                    0_i64,
                    &translation_strategy,
                ),
            )
            .await?;

        info!("created consumer group row -- {consumer_group_id:?}");
        let static_consumer_group_info = ConsumerGroupInfo {
            consumer_group_id: consumer_group_id.into_bytes(),
            consumer_id_shard_assignments: shard_assignments,
            producer_id: Some(producer_id),
            commitment_level,
            revision: 0,
            subscribed_event_types: subscribed_blockchain_event_types.to_vec(),
            group_type: ConsumerGroupType::Static,
            last_access_ip_address: remote_ip_addr,
            translation_strategy,
        };

        self.create_static_group_members(&static_consumer_group_info, initial_offset)
            .await?;

        info!("created consumer group static members -- {consumer_group_id:?}");
        Ok(static_consumer_group_info)
    }
}

pub struct ConsumerGroupManagerV2 {
    etcd: etcd_client::Client,
    session: Arc<Session>,
    producer_queries: ScyllaProducerStore,
}