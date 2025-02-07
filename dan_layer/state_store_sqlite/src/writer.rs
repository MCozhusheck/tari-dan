//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use std::ops::Deref;

use diesel::{
    dsl,
    sql_types::Text,
    AsChangeset,
    ExpressionMethods,
    NullableExpressionMethods,
    OptionalExtension,
    QueryDsl,
    RunQueryDsl,
    SqliteConnection,
};
use indexmap::IndexMap;
use log::*;
use tari_dan_common_types::{shard::Shard, Epoch, NodeAddressable, NodeHeight};
use tari_dan_storage::{
    consensus_models::{
        Block,
        BlockDiff,
        BlockId,
        Decision,
        EpochCheckpoint,
        Evidence,
        ForeignProposal,
        ForeignReceiveCounters,
        ForeignSendCounters,
        HighQc,
        LastExecuted,
        LastProposed,
        LastSentVote,
        LastVoted,
        LeafBlock,
        LockedBlock,
        LockedSubstate,
        PendingShardStateTreeDiff,
        QcId,
        QuorumCertificate,
        SubstateRecord,
        TransactionAtom,
        TransactionExecution,
        TransactionPoolStage,
        TransactionPoolStatusUpdate,
        TransactionRecord,
        VersionedStateHashTreeDiff,
        Vote,
    },
    StateStoreReadTransaction,
    StateStoreWriteTransaction,
    StorageError,
};
use tari_engine_types::substate::SubstateId;
use tari_state_tree::{Node, NodeKey, StaleTreeNode, TreeNode, Version};
use tari_transaction::{TransactionId, VersionedSubstateId};
use tari_utilities::ByteArray;
use time::{OffsetDateTime, PrimitiveDateTime};

use crate::{
    error::SqliteStorageError,
    reader::SqliteStateStoreReadTransaction,
    serialization::{serialize_hex, serialize_json},
    sql_models,
    sqlite_transaction::SqliteTransaction,
};

const LOG_TARGET: &str = "tari::dan::storage";

pub struct SqliteStateStoreWriteTransaction<'a, TAddr> {
    /// None indicates if the transaction has been explicitly committed/rolled back
    transaction: Option<SqliteStateStoreReadTransaction<'a, TAddr>>,
}

impl<'a, TAddr: NodeAddressable> SqliteStateStoreWriteTransaction<'a, TAddr> {
    pub fn new(transaction: SqliteTransaction<'a>) -> Self {
        Self {
            transaction: Some(SqliteStateStoreReadTransaction::new(transaction)),
        }
    }

    pub fn connection(&mut self) -> &mut SqliteConnection {
        self.transaction.as_mut().unwrap().connection()
    }

    fn parked_blocks_remove(&mut self, block_id: &str) -> Result<Block, StorageError> {
        use crate::schema::parked_blocks;

        let block = parked_blocks::table
            .filter(parked_blocks::block_id.eq(&block_id))
            .first::<sql_models::ParkedBlock>(self.connection())
            .optional()
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "parked_blocks_remove",
                source: e,
            })?
            .ok_or_else(|| StorageError::NotFound {
                item: "parked_blocks".to_string(),
                key: block_id.to_string(),
            })?;

        diesel::delete(parked_blocks::table)
            .filter(parked_blocks::block_id.eq(&block_id))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "parked_blocks_remove",
                source: e,
            })?;

        block.try_into()
    }

    fn parked_blocks_insert(&mut self, block: &Block) -> Result<(), StorageError> {
        use crate::schema::{blocks, parked_blocks};

        // check if block exists in blocks table using count query
        let block_id = serialize_hex(block.id());

        let block_exists = blocks::table
            .count()
            .filter(blocks::block_id.eq(&block_id))
            .first::<i64>(self.connection())
            .map(|count| count > 0)
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "parked_blocks_insert",
                source: e,
            })?;

        if block_exists {
            return Err(StorageError::QueryError {
                reason: format!("Cannot park block {block_id} that already exists in blocks table"),
            });
        }

        // check if block already exists in parked_blocks
        let already_parked = parked_blocks::table
            .count()
            .filter(parked_blocks::block_id.eq(&block_id))
            .first::<i64>(self.connection())
            .map(|count| count > 0)
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "parked_blocks_insert",
                source: e,
            })?;

        if already_parked {
            return Ok(());
        }

        let insert = (
            parked_blocks::block_id.eq(&block_id),
            parked_blocks::parent_block_id.eq(serialize_hex(block.parent())),
            parked_blocks::network.eq(block.network().to_string()),
            parked_blocks::merkle_root.eq(block.merkle_root().to_string()),
            parked_blocks::height.eq(block.height().as_u64() as i64),
            parked_blocks::epoch.eq(block.epoch().as_u64() as i64),
            parked_blocks::shard_group.eq(block.shard_group().encode_as_u32() as i32),
            parked_blocks::proposed_by.eq(serialize_hex(block.proposed_by().as_bytes())),
            parked_blocks::command_count.eq(block.commands().len() as i64),
            parked_blocks::commands.eq(serialize_json(block.commands())?),
            parked_blocks::total_leader_fee.eq(block.total_leader_fee() as i64),
            parked_blocks::justify.eq(serialize_json(block.justify())?),
            parked_blocks::foreign_indexes.eq(serialize_json(block.foreign_indexes())?),
            parked_blocks::block_time.eq(block.block_time().map(|v| v as i64)),
            parked_blocks::signature.eq(block.signature().map(serialize_json).transpose()?),
            parked_blocks::timestamp.eq(block.timestamp() as i64),
            parked_blocks::base_layer_block_height.eq(block.base_layer_block_height() as i64),
            parked_blocks::base_layer_block_hash.eq(serialize_hex(block.base_layer_block_hash())),
        );

        diesel::insert_into(parked_blocks::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "parked_blocks_upsert",
                source: e,
            })?;

        Ok(())
    }
}

impl<'tx, TAddr: NodeAddressable + 'tx> StateStoreWriteTransaction for SqliteStateStoreWriteTransaction<'tx, TAddr> {
    type Addr = TAddr;

    fn commit(mut self) -> Result<(), StorageError> {
        // Take so that we mark this transaction as complete in the drop impl
        self.transaction.take().unwrap().commit()?;
        Ok(())
    }

    fn rollback(mut self) -> Result<(), StorageError> {
        // Take so that we mark this transaction as complete in the drop impl
        self.transaction.take().unwrap().rollback()?;
        Ok(())
    }

    fn blocks_insert(&mut self, block: &Block) -> Result<(), StorageError> {
        use crate::schema::blocks;

        let insert = (
            blocks::block_id.eq(serialize_hex(block.id())),
            blocks::parent_block_id.eq(serialize_hex(block.parent())),
            blocks::merkle_root.eq(block.merkle_root().to_string()),
            blocks::network.eq(block.network().to_string()),
            blocks::height.eq(block.height().as_u64() as i64),
            blocks::epoch.eq(block.epoch().as_u64() as i64),
            blocks::shard_group.eq(block.shard_group().encode_as_u32() as i32),
            blocks::proposed_by.eq(serialize_hex(block.proposed_by().as_bytes())),
            blocks::command_count.eq(block.commands().len() as i64),
            blocks::commands.eq(serialize_json(block.commands())?),
            blocks::total_leader_fee.eq(block.total_leader_fee() as i64),
            blocks::qc_id.eq(serialize_hex(block.justify().id())),
            blocks::is_dummy.eq(block.is_dummy()),
            blocks::is_processed.eq(block.is_processed()),
            blocks::signature.eq(block.signature().map(serialize_json).transpose()?),
            blocks::foreign_indexes.eq(serialize_json(block.foreign_indexes())?),
            blocks::timestamp.eq(block.timestamp() as i64),
            blocks::base_layer_block_height.eq(block.base_layer_block_height() as i64),
            blocks::base_layer_block_hash.eq(serialize_hex(block.base_layer_block_hash())),
        );

        diesel::insert_into(blocks::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "blocks_insert",
                source: e,
            })?;

        diesel::sql_query(
            r#"
            UPDATE blocks
            SET block_time = timestamp -
                             (SELECT timestamp
                              FROM blocks
                              WHERE block_id == ?)
            WHERE block_id = ?"#,
        )
        .bind::<Text, _>(serialize_hex(block.justify().block_id()))
        .bind::<Text, _>(serialize_hex(block.id()))
        .execute(self.connection())
        .map_err(|e| SqliteStorageError::DieselError {
            operation: "blocks_insert_set_delta_time",
            source: e,
        })?;

        Ok(())
    }

    fn blocks_set_flags(
        &mut self,
        block_id: &BlockId,
        is_committed: Option<bool>,
        is_processed: Option<bool>,
    ) -> Result<(), StorageError> {
        use crate::schema::blocks;

        #[derive(AsChangeset)]
        #[diesel(table_name = blocks)]
        struct Changes {
            is_committed: Option<bool>,
            is_processed: Option<bool>,
        }
        let changes = Changes {
            is_committed,
            is_processed,
        };

        diesel::update(blocks::table)
            .filter(blocks::block_id.eq(serialize_hex(block_id)))
            .set(changes)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "blocks_commit",
                source: e,
            })?;

        Ok(())
    }

    fn block_diffs_insert(&mut self, block_diff: &BlockDiff) -> Result<(), StorageError> {
        use crate::schema::block_diffs;

        let block_id = serialize_hex(block_diff.block_id);
        // We commit in chunks because we can hit the SQL variable limit
        for chunk in block_diff.changes.chunks(1000) {
            let values = chunk
                .iter()
                .map(|ch| {
                    Ok((
                        block_diffs::block_id.eq(&block_id),
                        block_diffs::transaction_id.eq(serialize_hex(ch.transaction_id())),
                        block_diffs::substate_id.eq(ch.versioned_substate_id().substate_id().to_string()),
                        block_diffs::version.eq(ch.versioned_substate_id().version() as i32),
                        block_diffs::shard.eq(ch.shard().as_u32() as i32),
                        block_diffs::change.eq(ch.as_change_string()),
                        block_diffs::state.eq(ch.substate().map(serialize_json).transpose()?),
                    ))
                })
                .collect::<Result<Vec<_>, StorageError>>()?;

            diesel::insert_into(block_diffs::table)
                .values(values)
                .execute(self.connection())
                .map(|_| ())
                .map_err(|e| SqliteStorageError::DieselError {
                    operation: "block_diffs_insert",
                    source: e,
                })?;
        }

        Ok(())
    }

    fn block_diffs_remove(&mut self, block_id: &BlockId) -> Result<(), StorageError> {
        use crate::schema::block_diffs;

        diesel::delete(block_diffs::table)
            .filter(block_diffs::block_id.eq(serialize_hex(block_id)))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "block_diffs_remove",
                source: e,
            })?;

        Ok(())
    }

    fn quorum_certificates_insert(&mut self, qc: &QuorumCertificate) -> Result<(), StorageError> {
        use crate::schema::quorum_certificates;

        let insert = (
            quorum_certificates::qc_id.eq(serialize_hex(qc.id())),
            quorum_certificates::block_id.eq(serialize_hex(qc.block_id())),
            quorum_certificates::json.eq(serialize_json(qc)?),
        );

        diesel::insert_into(quorum_certificates::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "quorum_certificates_insert",
                source: e,
            })?;

        Ok(())
    }

    fn last_sent_vote_set(&mut self, last_sent_vote: &LastSentVote) -> Result<(), StorageError> {
        use crate::schema::last_sent_vote;

        let insert = (
            last_sent_vote::epoch.eq(last_sent_vote.epoch.as_u64() as i64),
            last_sent_vote::block_id.eq(serialize_hex(last_sent_vote.block_id)),
            last_sent_vote::block_height.eq(last_sent_vote.block_height.as_u64() as i64),
            last_sent_vote::decision.eq(i32::from(last_sent_vote.decision.as_u8())),
            last_sent_vote::signature.eq(serialize_json(&last_sent_vote.signature)?),
        );

        diesel::insert_into(last_sent_vote::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "last_sent_vote_set",
                source: e,
            })?;

        Ok(())
    }

    fn last_voted_set(&mut self, last_voted: &LastVoted) -> Result<(), StorageError> {
        use crate::schema::last_voted;

        let insert = (
            last_voted::block_id.eq(serialize_hex(last_voted.block_id)),
            last_voted::height.eq(last_voted.height.as_u64() as i64),
            last_voted::epoch.eq(last_voted.epoch.as_u64() as i64),
        );

        diesel::insert_into(last_voted::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "last_voted_set",
                source: e,
            })?;

        Ok(())
    }

    fn last_votes_unset(&mut self, last_voted: &LastVoted) -> Result<(), StorageError> {
        use crate::schema::last_voted;

        diesel::delete(last_voted::table)
            .filter(last_voted::block_id.eq(serialize_hex(last_voted.block_id)))
            .filter(last_voted::height.eq(last_voted.height.as_u64() as i64))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "last_votes_unset",
                source: e,
            })?;

        Ok(())
    }

    fn last_executed_set(&mut self, last_exec: &LastExecuted) -> Result<(), StorageError> {
        use crate::schema::last_executed;

        let insert = (
            last_executed::block_id.eq(serialize_hex(last_exec.block_id)),
            last_executed::height.eq(last_exec.height.as_u64() as i64),
            last_executed::epoch.eq(last_exec.epoch.as_u64() as i64),
        );

        diesel::insert_into(last_executed::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "last_executed_set",
                source: e,
            })?;

        Ok(())
    }

    fn last_proposed_set(&mut self, last_proposed: &LastProposed) -> Result<(), StorageError> {
        use crate::schema::last_proposed;

        let insert = (
            last_proposed::block_id.eq(serialize_hex(last_proposed.block_id)),
            last_proposed::height.eq(last_proposed.height.as_u64() as i64),
            last_proposed::epoch.eq(last_proposed.epoch.as_u64() as i64),
        );

        diesel::insert_into(last_proposed::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "last_proposed_set",
                source: e,
            })?;

        Ok(())
    }

    fn last_proposed_unset(&mut self, last_proposed: &LastProposed) -> Result<(), StorageError> {
        use crate::schema::last_proposed;

        diesel::delete(last_proposed::table)
            .filter(last_proposed::block_id.eq(serialize_hex(last_proposed.block_id)))
            .filter(last_proposed::height.eq(last_proposed.height.as_u64() as i64))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "last_proposed_unset",
                source: e,
            })?;

        Ok(())
    }

    fn leaf_block_set(&mut self, leaf_node: &LeafBlock) -> Result<(), StorageError> {
        use crate::schema::leaf_blocks;

        let insert = (
            leaf_blocks::block_id.eq(serialize_hex(leaf_node.block_id)),
            leaf_blocks::block_height.eq(leaf_node.height.as_u64() as i64),
            leaf_blocks::epoch.eq(leaf_node.epoch.as_u64() as i64),
        );

        diesel::insert_into(leaf_blocks::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "leaf_block_set",
                source: e,
            })?;

        Ok(())
    }

    fn locked_block_set(&mut self, locked_block: &LockedBlock) -> Result<(), StorageError> {
        use crate::schema::locked_block;

        let insert = (
            locked_block::block_id.eq(serialize_hex(locked_block.block_id)),
            locked_block::height.eq(locked_block.height.as_u64() as i64),
            locked_block::epoch.eq(locked_block.epoch.as_u64() as i64),
        );

        diesel::insert_into(locked_block::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "locked_block_set",
                source: e,
            })?;

        Ok(())
    }

    fn high_qc_set(&mut self, high_qc: &HighQc) -> Result<(), StorageError> {
        use crate::schema::high_qcs;

        let insert = (
            high_qcs::block_id.eq(serialize_hex(high_qc.block_id)),
            high_qcs::block_height.eq(high_qc.block_height().as_u64() as i64),
            high_qcs::epoch.eq(high_qc.epoch().as_u64() as i64),
            high_qcs::qc_id.eq(serialize_hex(high_qc.qc_id)),
        );

        diesel::insert_into(high_qcs::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "high_qc_set",
                source: e,
            })?;

        Ok(())
    }

    fn foreign_proposal_upsert(&mut self, foreign_proposal: &ForeignProposal) -> Result<(), StorageError> {
        use crate::schema::foreign_proposals;

        let values = (
            foreign_proposals::shard_group.eq(foreign_proposal.shard_group.encode_as_u32() as i32),
            foreign_proposals::block_id.eq(serialize_hex(foreign_proposal.block_id)),
            foreign_proposals::state.eq(foreign_proposal.state.to_string()),
            foreign_proposals::proposed_height.eq(foreign_proposal.proposed_height.map(|h| h.as_u64() as i64)),
            foreign_proposals::transactions.eq(serialize_json(&foreign_proposal.transactions)?),
            foreign_proposals::base_layer_block_height.eq(foreign_proposal.base_layer_block_height as i64),
        );

        diesel::insert_into(foreign_proposals::table)
            .values(values.clone())
            .on_conflict((foreign_proposals::shard_group, foreign_proposals::block_id))
            .do_update()
            .set(values)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "foreign_proposal_set",
                source: e,
            })?;
        Ok(())
    }

    fn foreign_proposal_delete(&mut self, foreign_proposal: &ForeignProposal) -> Result<(), StorageError> {
        use crate::schema::foreign_proposals;

        diesel::delete(foreign_proposals::table)
            .filter(foreign_proposals::shard_group.eq(foreign_proposal.shard_group.encode_as_u32() as i32))
            .filter(foreign_proposals::block_id.eq(serialize_hex(foreign_proposal.block_id)))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "foreign_proposal_delete",
                source: e,
            })?;

        Ok(())
    }

    fn foreign_send_counters_set(
        &mut self,
        foreign_send_counter: &ForeignSendCounters,
        block_id: &BlockId,
    ) -> Result<(), StorageError> {
        use crate::schema::foreign_send_counters;

        let insert = (
            foreign_send_counters::block_id.eq(serialize_hex(block_id)),
            foreign_send_counters::counters.eq(serialize_json(&foreign_send_counter.counters)?),
        );

        diesel::insert_into(foreign_send_counters::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "foreign_send_counters_set",
                source: e,
            })?;

        Ok(())
    }

    fn foreign_receive_counters_set(
        &mut self,
        foreign_receive_counter: &ForeignReceiveCounters,
    ) -> Result<(), StorageError> {
        use crate::schema::foreign_receive_counters;

        let insert = (foreign_receive_counters::counters.eq(serialize_json(&foreign_receive_counter.counters)?),);

        diesel::insert_into(foreign_receive_counters::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "foreign_receive_counters_set",
                source: e,
            })?;

        Ok(())
    }

    fn transactions_insert(&mut self, tx_rec: &TransactionRecord) -> Result<(), StorageError> {
        use crate::schema::transactions;

        let transaction = tx_rec.transaction();
        let insert = (
            transactions::transaction_id.eq(serialize_hex(transaction.id())),
            transactions::fee_instructions.eq(serialize_json(transaction.fee_instructions())?),
            transactions::instructions.eq(serialize_json(transaction.instructions())?),
            transactions::signatures.eq(serialize_json(transaction.signatures())?),
            transactions::inputs.eq(serialize_json(transaction.inputs())?),
            transactions::filled_inputs.eq(serialize_json(transaction.filled_inputs())?),
            transactions::resolved_inputs.eq(tx_rec.resolved_inputs().map(serialize_json).transpose()?),
            transactions::resulting_outputs.eq(serialize_json(tx_rec.resulting_outputs())?),
            transactions::result.eq(tx_rec.execution_result().map(serialize_json).transpose()?),
            transactions::execution_time_ms.eq(tx_rec
                .execution_time()
                .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))),
            transactions::final_decision.eq(tx_rec.final_decision().map(|d| d.to_string())),
            transactions::finalized_at.eq(tx_rec
                .finalized_time()
                .map(|t| {
                    let now = OffsetDateTime::now_utc().saturating_sub(t.try_into()?);
                    Ok(PrimitiveDateTime::new(now.date(), now.time()))
                })
                .transpose()
                .map_err(|e: time::error::ConversionRange| StorageError::QueryError {
                    reason: format!("Cannot convert finalize time into PrimitiveDateTime: {e}"),
                })?),
            transactions::abort_details.eq(tx_rec.abort_details()),
            transactions::min_epoch.eq(transaction.min_epoch().map(|e| e.as_u64() as i64)),
            transactions::max_epoch.eq(transaction.max_epoch().map(|e| e.as_u64() as i64)),
        );

        diesel::insert_into(transactions::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transactions_insert",
                source: e,
            })?;

        Ok(())
    }

    fn transactions_update(&mut self, transaction_rec: &TransactionRecord) -> Result<(), StorageError> {
        use crate::schema::transactions;

        let transaction = transaction_rec.transaction();

        #[derive(AsChangeset)]
        #[diesel(table_name = transactions)]
        struct Changes {
            result: Option<String>,
            filled_inputs: String,
            resulting_outputs: String,
            resolved_inputs: Option<String>,
            execution_time_ms: Option<i64>,
            final_decision: Option<String>,
            finalized_at: Option<PrimitiveDateTime>,
            abort_details: Option<String>,
        }

        let change_set = Changes {
            result: transaction_rec.execution_result().map(serialize_json).transpose()?,
            filled_inputs: serialize_json(transaction.filled_inputs())?,
            resulting_outputs: serialize_json(transaction_rec.resulting_outputs())?,
            resolved_inputs: transaction_rec.resolved_inputs().map(serialize_json).transpose()?,
            execution_time_ms: transaction_rec
                .execution_time()
                .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),

            final_decision: transaction_rec.final_decision().map(|d| d.to_string()),
            finalized_at: transaction_rec.final_decision().map(|_| {
                let now = OffsetDateTime::now_utc();
                PrimitiveDateTime::new(now.date(), now.time())
            }),
            abort_details: transaction_rec.abort_details.clone(),
        };

        let num_affected = diesel::update(transactions::table)
            .filter(transactions::transaction_id.eq(serialize_hex(transaction.id())))
            .set(change_set)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transactions_update",
                source: e,
            })?;

        if num_affected == 0 {
            return Err(StorageError::NotFound {
                item: "transaction".to_string(),
                key: transaction.id().to_string(),
            });
        }

        Ok(())
    }

    fn transactions_save_all<'a, I: IntoIterator<Item = &'a TransactionRecord>>(
        &mut self,
        txs: I,
    ) -> Result<(), StorageError> {
        use crate::schema::transactions;

        let insert = txs
            .into_iter()
            .map(|rec| {
                let transaction = rec.transaction();
                Ok((
                    transactions::transaction_id.eq(serialize_hex(transaction.id())),
                    transactions::fee_instructions.eq(serialize_json(transaction.fee_instructions())?),
                    transactions::instructions.eq(serialize_json(transaction.instructions())?),
                    transactions::signatures.eq(serialize_json(transaction.signatures())?),
                    transactions::inputs.eq(serialize_json(transaction.inputs())?),
                    transactions::resolved_inputs.eq(rec.resolved_inputs().map(serialize_json).transpose()?),
                    transactions::filled_inputs.eq(serialize_json(transaction.filled_inputs())?),
                    transactions::resulting_outputs.eq(serialize_json(rec.resulting_outputs())?),
                    transactions::result.eq(rec.execution_result().map(serialize_json).transpose()?),
                ))
            })
            .collect::<Result<Vec<_>, StorageError>>()?;

        diesel::insert_or_ignore_into(transactions::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transactions_insert",
                source: e,
            })?;

        Ok(())
    }

    fn transactions_finalize_all<'a, I: IntoIterator<Item = &'a TransactionAtom>>(
        &mut self,
        block_id: BlockId,
        transactions: I,
    ) -> Result<(), StorageError> {
        use crate::schema::transactions;

        let changes = transactions
            .into_iter()
            .map(|atom| {
                // TODO(perf): 2n queries, query is slow
                let exec = self.transaction_executions_get_pending_for_block(&atom.id, &block_id)?;
                // .optional()?;

                // let exec = match exec {
                //     Some(exec) => exec,
                //     None => {
                //         // Executed in the mempool.
                //         // TODO: this is kinda hacky. Either the mempool should add a block_id=null execution or we
                //         // should remove mempool execution
                //         let transaction = self.transactions_get(&atom.id)?;
                //         let executed = ExecutedTransaction::try_from(transaction)?;
                //         executed.into_execution_for_block(block_id)
                //     },
                // };

                Ok((
                    transactions::transaction_id.eq(serialize_hex(atom.id())),
                    (
                        transactions::resolved_inputs.eq(serialize_json(&exec.resolved_inputs())?),
                        transactions::resulting_outputs.eq(serialize_json(&exec.resulting_outputs())?),
                        transactions::result.eq(serialize_json(&exec.result())?),
                        transactions::execution_time_ms.eq(exec.execution_time().as_millis() as i64),
                        transactions::final_decision.eq(atom.decision.to_string()),
                        transactions::finalized_at.eq(now()),
                    ),
                ))
            })
            .collect::<Result<Vec<_>, StorageError>>()?;

        for (predicate, change) in changes {
            diesel::update(transactions::table)
                .filter(predicate)
                .set(change)
                .execute(self.connection())
                .map_err(|e| SqliteStorageError::DieselError {
                    operation: "transactions_finalize_all",
                    source: e,
                })?;
        }

        Ok(())
    }

    fn transaction_executions_insert_or_ignore(
        &mut self,
        transaction_execution: &TransactionExecution,
    ) -> Result<(), StorageError> {
        use crate::schema::transaction_executions;

        let insert = (
            transaction_executions::block_id.eq(serialize_hex(transaction_execution.block_id())),
            transaction_executions::transaction_id.eq(serialize_hex(transaction_execution.transaction_id())),
            transaction_executions::result.eq(serialize_json(&transaction_execution.result())?),
            transaction_executions::resolved_inputs.eq(serialize_json(&transaction_execution.resolved_inputs())?),
            transaction_executions::resulting_outputs.eq(serialize_json(&transaction_execution.resulting_outputs())?),
            transaction_executions::execution_time_ms
                .eq(i64::try_from(transaction_execution.execution_time().as_millis()).unwrap_or(i64::MAX)),
        );

        diesel::insert_or_ignore_into(transaction_executions::table)
            .values(insert)
            .on_conflict_do_nothing()
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_executions_insert",
                source: e,
            })?;

        Ok(())
    }

    fn transaction_pool_insert_new(
        &mut self,
        transaction_id: TransactionId,
        decision: Decision,
    ) -> Result<(), StorageError> {
        use crate::schema::transaction_pool;

        let insert = (
            transaction_pool::transaction_id.eq(serialize_hex(transaction_id)),
            transaction_pool::original_decision.eq(decision.to_string()),
            transaction_pool::stage.eq(TransactionPoolStage::New.to_string()),
            transaction_pool::is_ready.eq(true),
        );

        diesel::insert_into(transaction_pool::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_insert",
                source: e,
            })?;

        Ok(())
    }

    fn transaction_pool_set_atom(&mut self, transaction: TransactionAtom) -> Result<(), StorageError> {
        use crate::schema::transaction_pool;

        let transaction_id = serialize_hex(transaction.id);

        let change_set = (
            transaction_pool::original_decision.eq(transaction.decision.to_string()),
            transaction_pool::transaction_fee.eq(transaction.transaction_fee as i64),
            transaction_pool::evidence.eq(serialize_json(&transaction.evidence)?),
            transaction_pool::leader_fee.eq(transaction.leader_fee.as_ref().map(|f| f.fee as i64)),
            transaction_pool::global_exhaust_burn
                .eq(transaction.leader_fee.as_ref().map(|f| f.global_exhaust_burn as i64)),
            transaction_pool::updated_at.eq(now()),
        );

        let num_affected = diesel::update(transaction_pool::table)
            .filter(transaction_pool::transaction_id.eq(&transaction_id))
            .set(change_set)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_set_atom",
                source: e,
            })?;

        if num_affected == 0 {
            return Err(StorageError::NotFound {
                item: "transaction".to_string(),
                key: transaction.id.to_string(),
            });
        }

        Ok(())
    }

    fn transaction_pool_add_pending_update(
        &mut self,
        update: &TransactionPoolStatusUpdate,
    ) -> Result<(), StorageError> {
        use crate::schema::{transaction_pool, transaction_pool_state_updates};

        let transaction_id = serialize_hex(update.transaction_id());
        let block_id = serialize_hex(update.block_id());
        let values = (
            transaction_pool_state_updates::block_id.eq(&block_id),
            transaction_pool_state_updates::block_height.eq(update.block_height().as_u64() as i64),
            transaction_pool_state_updates::transaction_id.eq(&transaction_id),
            transaction_pool_state_updates::evidence.eq(serialize_json(update.evidence())?),
            transaction_pool_state_updates::stage.eq(update.stage().to_string()),
            transaction_pool_state_updates::local_decision.eq(update.local_decision().to_string()),
            transaction_pool_state_updates::is_ready.eq(update.is_ready()),
        );

        // Check if update exists for block and transaction
        let count = transaction_pool_state_updates::table
            .count()
            .filter(transaction_pool_state_updates::block_id.eq(&block_id))
            .filter(transaction_pool_state_updates::transaction_id.eq(&transaction_id))
            .first::<i64>(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_add_pending_update",
                source: e,
            })?;

        if count == 0 {
            diesel::insert_into(transaction_pool_state_updates::table)
                .values(values)
                .execute(self.connection())
                .map_err(|e| SqliteStorageError::DieselError {
                    operation: "transaction_pool_add_pending_update",
                    source: e,
                })?;
        } else {
            diesel::update(transaction_pool_state_updates::table)
                .filter(transaction_pool_state_updates::block_id.eq(&block_id))
                .filter(transaction_pool_state_updates::transaction_id.eq(&transaction_id))
                .set(values)
                .execute(self.connection())
                .map_err(|e| SqliteStorageError::DieselError {
                    operation: "transaction_pool_add_pending_update",
                    source: e,
                })?;
        }

        // Set is_ready to the last value we set here. Bit of a hack to get has_uncommitted_transactions to return a
        // more accurate value without querying the updates table
        diesel::update(transaction_pool::table)
            .filter(transaction_pool::transaction_id.eq(&transaction_id))
            .set((
                transaction_pool::is_ready.eq(update.is_ready()),
                transaction_pool::pending_stage.eq(update.stage().to_string()),
            ))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_add_pending_update",
                source: e,
            })?;

        Ok(())
    }

    fn transaction_pool_update(
        &mut self,
        transaction_id: &TransactionId,
        local_decision: Option<Decision>,
        remote_decision: Option<Decision>,
        remote_evidence: Option<&Evidence>,
    ) -> Result<(), StorageError> {
        use crate::schema::transaction_pool;

        let transaction_id = serialize_hex(transaction_id);

        #[derive(AsChangeset)]
        #[diesel(table_name = transaction_pool)]
        struct Changes {
            remote_evidence: Option<String>,
            local_decision: Option<Option<String>>,
            remote_decision: Option<Option<String>>,
            updated_at: PrimitiveDateTime,
        }

        let change_set = Changes {
            remote_evidence: remote_evidence.map(serialize_json).transpose()?,
            local_decision: local_decision.map(|d| d.to_string()).map(Some),
            remote_decision: remote_decision.map(|d| d.to_string()).map(Some),
            updated_at: now(),
        };

        let num_affected = diesel::update(transaction_pool::table)
            .filter(transaction_pool::transaction_id.eq(&transaction_id))
            .set(change_set)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_set_remote_decision",
                source: e,
            })?;

        if num_affected == 0 {
            return Err(StorageError::NotFound {
                item: "transaction".to_string(),
                key: transaction_id,
            });
        }

        Ok(())
    }

    fn transaction_pool_remove(&mut self, transaction_id: &TransactionId) -> Result<(), StorageError> {
        use crate::schema::{transaction_pool, transaction_pool_state_updates};

        let transaction_id = serialize_hex(transaction_id);
        let num_affected = diesel::delete(transaction_pool::table)
            .filter(transaction_pool::transaction_id.eq(&transaction_id))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_remove",
                source: e,
            })?;

        if num_affected == 0 {
            return Err(StorageError::NotFound {
                item: "transaction".to_string(),
                key: transaction_id,
            });
        }

        diesel::delete(transaction_pool_state_updates::table)
            .filter(transaction_pool_state_updates::transaction_id.eq(transaction_id))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_remove",
                source: e,
            })?;

        Ok(())
    }

    fn transaction_pool_remove_all<'a, I: IntoIterator<Item = &'a TransactionId>>(
        &mut self,
        transaction_ids: I,
    ) -> Result<Vec<TransactionAtom>, StorageError> {
        use crate::schema::{transaction_pool, transaction_pool_state_updates};

        let transaction_ids = transaction_ids.into_iter().map(serialize_hex).collect::<Vec<_>>();

        let txs = diesel::delete(transaction_pool::table)
            .filter(transaction_pool::transaction_id.eq_any(&transaction_ids))
            .returning(transaction_pool::all_columns)
            .get_results::<sql_models::TransactionPoolRecord>(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_remove_all",
                source: e,
            })?;

        if txs.len() != transaction_ids.len() {
            return Err(SqliteStorageError::NotAllTransactionsFound {
                operation: "transaction_pool_remove_all",
                details: format!(
                    "Found {} transactions, but {} were queried",
                    txs.len(),
                    transaction_ids.len()
                ),
            }
            .into());
        }

        diesel::delete(transaction_pool_state_updates::table)
            .filter(transaction_pool_state_updates::transaction_id.eq_any(&transaction_ids))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_remove_all",
                source: e,
            })?;

        txs.into_iter()
            .map(|tx| tx.try_convert(None).map(|t| t.into_local_transaction_atom()))
            .collect()
    }

    fn transaction_pool_set_all_transitions<'a, I: IntoIterator<Item = &'a TransactionId>>(
        &mut self,
        locked_block: &LockedBlock,
        new_locked_block: &LockedBlock,
        tx_ids: I,
    ) -> Result<(), StorageError> {
        use crate::schema::{transaction_pool, transaction_pool_state_updates};

        let tx_ids = tx_ids.into_iter().map(serialize_hex).collect::<Vec<_>>();

        let count = transaction_pool::table
            .count()
            .filter(transaction_pool::transaction_id.eq_any(&tx_ids))
            .get_result::<i64>(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_set_all_transitions",
                source: e,
            })?;

        if count != tx_ids.len() as i64 {
            return Err(SqliteStorageError::NotAllTransactionsFound {
                operation: "transaction_pool_set_all_transitions",
                details: format!("Found {} transactions, but {} were queried", count, tx_ids.len()),
            }
            .into());
        }

        let updates = self.get_transaction_atom_state_updates_between_blocks(
            locked_block.block_id(),
            new_locked_block.block_id(),
            tx_ids.iter().map(|s| s.as_str()),
        )?;

        debug!(
            target: LOG_TARGET,
            "transaction_pool_set_all_transitions: locked_block={}, new_locked_block={}, {} transactions, {} updates", locked_block, new_locked_block, tx_ids.len(), updates.len()
        );

        diesel::delete(transaction_pool_state_updates::table)
            .filter(transaction_pool_state_updates::transaction_id.eq_any(&tx_ids))
            .filter(transaction_pool_state_updates::block_height.le(new_locked_block.height().as_u64() as i64))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "transaction_pool_set_all_transitions",
                source: e,
            })?;

        for update in updates.into_values() {
            diesel::update(transaction_pool::table)
                .filter(transaction_pool::transaction_id.eq(&update.transaction_id))
                .set((
                    transaction_pool::stage.eq(update.stage),
                    transaction_pool::local_decision.eq(update.local_decision),
                    transaction_pool::evidence.eq(update.evidence),
                    transaction_pool::is_ready.eq(update.is_ready),
                    transaction_pool::updated_at.eq(now()),
                ))
                .execute(self.connection())
                .map_err(|e| SqliteStorageError::DieselError {
                    operation: "transaction_pool_set_all_transitions",
                    source: e,
                })?;
        }

        Ok(())
    }

    fn missing_transactions_insert<
        'a,
        IMissing: IntoIterator<Item = &'a TransactionId>,
        IAwaiting: IntoIterator<Item = &'a TransactionId>,
    >(
        &mut self,
        block: &Block,
        missing_transaction_ids: IMissing,
        awaiting_transaction_ids: IAwaiting,
    ) -> Result<(), StorageError> {
        use crate::schema::missing_transactions;

        let missing_transaction_ids = missing_transaction_ids.into_iter().map(serialize_hex);
        let awaiting_transaction_ids = awaiting_transaction_ids.into_iter().map(serialize_hex);
        let block_id_hex = serialize_hex(block.id());

        self.parked_blocks_insert(block)?;

        let values = missing_transaction_ids
            .map(|tx_id| {
                (
                    missing_transactions::block_id.eq(&block_id_hex),
                    missing_transactions::block_height.eq(block.height().as_u64() as i64),
                    missing_transactions::transaction_id.eq(tx_id),
                    missing_transactions::is_awaiting_execution.eq(false),
                )
            })
            .chain(awaiting_transaction_ids.map(|tx_id| {
                (
                    missing_transactions::block_id.eq(&block_id_hex),
                    missing_transactions::block_height.eq(block.height().as_u64() as i64),
                    missing_transactions::transaction_id.eq(tx_id),
                    missing_transactions::is_awaiting_execution.eq(true),
                )
            }))
            .collect::<Vec<_>>();

        diesel::insert_into(missing_transactions::table)
            .values(values)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "missing_transactions_insert",
                source: e,
            })?;

        Ok(())
    }

    fn missing_transactions_remove(
        &mut self,
        current_height: NodeHeight,
        transaction_id: &TransactionId,
    ) -> Result<Option<Block>, StorageError> {
        use crate::schema::missing_transactions;

        let transaction_id = serialize_hex(transaction_id);
        let block_id = missing_transactions::table
            .select(missing_transactions::block_id)
            .filter(missing_transactions::transaction_id.eq(&transaction_id))
            .filter(missing_transactions::block_height.eq(current_height.as_u64() as i64))
            .first::<String>(self.connection())
            .optional()
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "missing_transactions_remove",
                source: e,
            })?;
        let Some(block_id) = block_id else {
            return Ok(None);
        };

        diesel::delete(missing_transactions::table)
            .filter(missing_transactions::transaction_id.eq(&transaction_id))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "missing_transactions_remove",
                source: e,
            })?;
        let missing_transactions = missing_transactions::table
            .select(missing_transactions::transaction_id)
            .filter(missing_transactions::block_id.eq(&block_id))
            .get_results::<String>(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "missing_transactions_remove",
                source: e,
            })?;

        if missing_transactions.is_empty() {
            // delete all entries that are for previous heights
            diesel::delete(missing_transactions::table)
                .filter(missing_transactions::block_height.lt(current_height.as_u64() as i64))
                .execute(self.connection())
                .map_err(|e| SqliteStorageError::DieselError {
                    operation: "missing_transactions_remove",
                    source: e,
                })?;
            let block = self.parked_blocks_remove(&block_id)?;
            return Ok(Some(block));
        }

        Ok(None)
    }

    fn votes_insert(&mut self, vote: &Vote) -> Result<(), StorageError> {
        use crate::schema::votes;

        let insert = (
            votes::hash.eq(serialize_hex(vote.calculate_hash())),
            votes::epoch.eq(vote.epoch.as_u64() as i64),
            votes::block_id.eq(serialize_hex(vote.block_id)),
            votes::sender_leaf_hash.eq(serialize_hex(vote.sender_leaf_hash)),
            votes::decision.eq(i32::from(vote.decision.as_u8())),
            votes::signature.eq(serialize_json(&vote.signature)?),
        );

        diesel::insert_into(votes::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "votes_insert",
                source: e,
            })?;

        Ok(())
    }

    fn substate_locks_insert_all<I: IntoIterator<Item = (SubstateId, Vec<LockedSubstate>)>>(
        &mut self,
        block_id: BlockId,
        locks: I,
    ) -> Result<(), StorageError> {
        use crate::schema::substate_locks;

        let mut iter = locks.into_iter();
        const CHUNK_SIZE: usize = 100;
        // We have to break up into multiple queries because we can hit max SQL variable limit
        loop {
            let locks = iter
                .by_ref()
                .take(CHUNK_SIZE)
                .flat_map(|(id, locks)| {
                    locks.into_iter().map(move |lock| {
                        (
                            substate_locks::block_id.eq(serialize_hex(block_id)),
                            substate_locks::substate_id.eq(id.to_string()),
                            substate_locks::version.eq(lock.version() as i32),
                            substate_locks::transaction_id.eq(serialize_hex(lock.transaction_id())),
                            substate_locks::lock.eq(lock.substate_lock().to_string()),
                            substate_locks::is_local_only.eq(lock.is_local_only()),
                        )
                    })
                })
                .collect::<Vec<_>>();

            let count = locks.len();
            if count == 0 {
                break;
            }

            diesel::insert_into(substate_locks::table)
                .values(locks)
                .execute(self.connection())
                .map_err(|e| SqliteStorageError::DieselError {
                    operation: "substate_locks_insert_all",
                    source: e,
                })?;

            if count < CHUNK_SIZE {
                break;
            }
        }

        Ok(())
    }

    fn substate_locks_remove_many_for_transactions<'a, I: IntoIterator<Item = &'a TransactionId>>(
        &mut self,
        transaction_ids: I,
    ) -> Result<(), StorageError> {
        use crate::schema::substate_locks;

        diesel::delete(substate_locks::table)
            .filter(substate_locks::transaction_id.eq_any(transaction_ids.into_iter().map(serialize_hex)))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "substate_locks_release_all_by_substates",
                source: e,
            })?;

        Ok(())
    }

    fn substates_create(&mut self, substate: SubstateRecord) -> Result<(), StorageError> {
        use crate::schema::{state_transitions, substates};

        if substate.is_destroyed() {
            return Err(StorageError::QueryError {
                reason: format!(
                    "calling substates_create with a destroyed SubstateRecord is not valid. substate_id = {}",
                    substate.substate_id
                ),
            });
        }

        let values = (
            substates::address.eq(serialize_hex(substate.to_substate_address())),
            substates::substate_id.eq(substate.substate_id.to_string()),
            substates::version.eq(substate.version as i32),
            substates::data.eq(serialize_json(&substate.substate_value)?),
            substates::state_hash.eq(serialize_hex(substate.state_hash)),
            substates::created_by_transaction.eq(serialize_hex(substate.created_by_transaction)),
            substates::created_justify.eq(serialize_hex(substate.created_justify)),
            substates::created_block.eq(serialize_hex(substate.created_block)),
            substates::created_height.eq(substate.created_height.as_u64() as i64),
            substates::created_at_epoch.eq(substate.created_at_epoch.as_u64() as i64),
            substates::created_by_shard.eq(substate.created_by_shard.as_u32() as i32),
        );

        diesel::insert_into(substates::table)
            .values(values)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "substates_create",
                source: e,
            })?;

        let seq = state_transitions::table
            .select(dsl::max(state_transitions::seq))
            .filter(state_transitions::shard.eq(substate.created_by_shard.as_u32() as i32))
            .first::<Option<i64>>(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "substates_create",
                source: e,
            })?;
        let next_seq = seq.map(|s| s + 1).unwrap_or(0);

        // This means that we MUST do the state tree updates before inserting substates
        let version = self.state_tree_versions_get_latest(substate.created_by_shard)?;
        let values = (
            state_transitions::seq.eq(next_seq),
            state_transitions::epoch.eq(substate.created_at_epoch.as_u64() as i64),
            state_transitions::shard.eq(substate.created_by_shard.as_u32() as i32),
            state_transitions::substate_address.eq(serialize_hex(substate.to_substate_address())),
            state_transitions::substate_id.eq(substate.substate_id.to_string()),
            state_transitions::version.eq(substate.version as i32),
            state_transitions::transition.eq("UP"),
            state_transitions::state_hash.eq(serialize_hex(substate.state_hash)),
            state_transitions::state_version.eq(version.unwrap_or(0) as i64),
        );

        diesel::insert_into(state_transitions::table)
            .values(values)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "substates_create",
                source: e,
            })?;

        Ok(())
    }

    fn substates_down(
        &mut self,
        versioned_substate_id: VersionedSubstateId,
        shard: Shard,
        epoch: Epoch,
        destroyed_block_height: NodeHeight,
        destroyed_transaction_id: &TransactionId,
        destroyed_qc_id: &QcId,
    ) -> Result<(), StorageError> {
        use crate::schema::{state_transitions, substates};

        let changes = (
            substates::destroyed_at.eq(diesel::dsl::now),
            substates::destroyed_by_transaction.eq(Some(serialize_hex(destroyed_transaction_id))),
            substates::destroyed_by_block.eq(Some(destroyed_block_height.as_u64() as i64)),
            substates::destroyed_at_epoch.eq(Some(epoch.as_u64() as i64)),
            substates::destroyed_by_shard.eq(Some(shard.as_u32() as i32)),
            substates::destroyed_justify.eq(Some(serialize_hex(destroyed_qc_id))),
        );

        let address = versioned_substate_id.to_substate_address();

        diesel::update(substates::table)
            .filter(substates::address.eq(serialize_hex(address)))
            .set(changes)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "substates_down",
                source: e,
            })?;

        let seq = state_transitions::table
            .select(dsl::max(state_transitions::seq))
            .filter(state_transitions::shard.eq(shard.as_u32() as i32))
            .first::<Option<i64>>(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "substates_create",
                source: e,
            })?;
        let next_seq = seq.map(|s| s + 1).unwrap_or(0);

        let version = self.state_tree_versions_get_latest(shard)?;
        let values = (
            state_transitions::seq.eq(next_seq),
            state_transitions::epoch.eq(epoch.as_u64() as i64),
            state_transitions::shard.eq(shard.as_u32() as i32),
            state_transitions::substate_address.eq(serialize_hex(address)),
            state_transitions::substate_id.eq(versioned_substate_id.substate_id.to_string()),
            state_transitions::version.eq(versioned_substate_id.version as i32),
            state_transitions::transition.eq("DOWN"),
            state_transitions::state_version.eq(version.unwrap_or(0) as i64),
        );

        diesel::insert_into(state_transitions::table)
            .values(values)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "substates_down",
                source: e,
            })?;

        Ok(())
    }

    fn pending_state_tree_diffs_remove_by_block(
        &mut self,
        block_id: &BlockId,
    ) -> Result<IndexMap<Shard, Vec<PendingShardStateTreeDiff>>, StorageError> {
        use crate::schema::pending_state_tree_diffs;

        let diff_recs = pending_state_tree_diffs::table
            .filter(pending_state_tree_diffs::block_id.eq(serialize_hex(block_id)))
            .order_by(pending_state_tree_diffs::block_height.asc())
            .get_results::<sql_models::PendingStateTreeDiff>(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "pending_state_tree_diffs_remove_by_block",
                source: e,
            })?;

        diesel::delete(pending_state_tree_diffs::table)
            .filter(pending_state_tree_diffs::id.eq_any(diff_recs.iter().map(|d| d.id)))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "pending_state_tree_diffs_remove_by_block",
                source: e,
            })?;

        let mut diffs = IndexMap::new();
        for diff in diff_recs {
            let shard = Shard::from(diff.shard as u32);
            let diff = PendingShardStateTreeDiff::try_from(diff)?;
            diffs.entry(shard).or_insert_with(Vec::new).push(diff);
        }

        Ok(diffs)
    }

    fn pending_state_tree_diffs_insert(
        &mut self,
        block_id: BlockId,
        shard: Shard,
        diff: VersionedStateHashTreeDiff,
    ) -> Result<(), StorageError> {
        use crate::schema::{blocks, pending_state_tree_diffs};

        let insert = (
            pending_state_tree_diffs::block_id.eq(serialize_hex(block_id)),
            pending_state_tree_diffs::shard.eq(shard.as_u32() as i32),
            pending_state_tree_diffs::block_height.eq(blocks::table
                .select(blocks::height)
                .filter(blocks::block_id.eq(serialize_hex(block_id)))
                .single_value()
                .assume_not_null()),
            pending_state_tree_diffs::version.eq(diff.version as i64),
            pending_state_tree_diffs::diff_json.eq(serialize_json(&diff.diff)?),
        );

        diesel::insert_into(pending_state_tree_diffs::table)
            .values(insert)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "pending_state_tree_diffs_insert",
                source: e,
            })?;

        Ok(())
    }

    fn state_tree_nodes_insert(&mut self, shard: Shard, key: NodeKey, node: Node<Version>) -> Result<(), StorageError> {
        use crate::schema::state_tree;

        let node = TreeNode::new_latest(node);
        let node = serde_json::to_string(&node).map_err(|e| StorageError::QueryError {
            reason: format!("Failed to serialize node: {}", e),
        })?;

        let values = (
            state_tree::shard.eq(shard.as_u32() as i32),
            state_tree::key.eq(key.to_string()),
            state_tree::node.eq(node),
        );
        diesel::insert_into(state_tree::table)
            .values(&values)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "state_tree_nodes_insert",
                source: e,
            })?;

        Ok(())
    }

    fn state_tree_nodes_record_stale_tree_node(
        &mut self,
        shard: Shard,
        node: StaleTreeNode,
    ) -> Result<(), StorageError> {
        use crate::schema::state_tree;

        let key = node.as_node_key();
        let num_effected = diesel::update(state_tree::table)
            .filter(state_tree::shard.eq(shard.as_u32() as i32))
            .filter(state_tree::key.eq(key.to_string()))
            .set(state_tree::is_stale.eq(true))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "state_tree_nodes_mark_stale_tree_node",
                source: e,
            })?;

        if num_effected == 0 {
            return Err(StorageError::NotFound {
                item: "state_tree_node".to_string(),
                key: key.to_string(),
            });
        }

        Ok(())
    }

    fn state_tree_shard_versions_set(&mut self, shard: Shard, version: Version) -> Result<(), StorageError> {
        use crate::schema::state_tree_shard_versions;

        let values = (
            state_tree_shard_versions::shard.eq(shard.as_u32() as i32),
            state_tree_shard_versions::version.eq(version as i64),
        );

        diesel::insert_into(state_tree_shard_versions::table)
            .values(&values)
            .on_conflict(state_tree_shard_versions::shard)
            .do_update()
            .set(state_tree_shard_versions::version.eq(version as i64))
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "state_tree_shard_versions_increment",
                source: e,
            })?;

        Ok(())
    }

    fn epoch_checkpoint_save(&mut self, checkpoint: &EpochCheckpoint) -> Result<(), StorageError> {
        use crate::schema::epoch_checkpoints;

        let values = (
            epoch_checkpoints::epoch.eq(checkpoint.block().epoch().as_u64() as i64),
            epoch_checkpoints::commit_block.eq(serialize_json(checkpoint.block())?),
            epoch_checkpoints::qcs.eq(serialize_json(checkpoint.qcs())?),
            epoch_checkpoints::shard_roots.eq(serialize_json(checkpoint.shard_roots())?),
        );

        diesel::insert_into(epoch_checkpoints::table)
            .values(values)
            .execute(self.connection())
            .map_err(|e| SqliteStorageError::DieselError {
                operation: "epoch_checkpoint_save",
                source: e,
            })?;

        Ok(())
    }
}

impl<'a, TAddr> Deref for SqliteStateStoreWriteTransaction<'a, TAddr> {
    type Target = SqliteStateStoreReadTransaction<'a, TAddr>;

    fn deref(&self) -> &Self::Target {
        self.transaction.as_ref().unwrap()
    }
}

impl<TAddr> Drop for SqliteStateStoreWriteTransaction<'_, TAddr> {
    fn drop(&mut self) {
        if self.transaction.is_some() {
            warn!(
                target: LOG_TARGET,
                "Shard store write transaction was not committed/rolled back"
            );
        }
    }
}

fn now() -> PrimitiveDateTime {
    let now = time::OffsetDateTime::now_utc();
    PrimitiveDateTime::new(now.date(), now.time())
}
