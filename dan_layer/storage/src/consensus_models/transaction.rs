//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use std::{collections::HashSet, ops::Deref, time::Duration};

use serde::Deserialize;
use tari_engine_types::commit_result::{ExecuteResult, FinalizeResult, RejectReason};
use tari_transaction::{Transaction, TransactionId, VersionedSubstateId};

use crate::{
    consensus_models::{BlockId, Decision, ExecutedTransaction, TransactionAtom, VersionedSubstateIdLockIntent},
    Ordering,
    StateStoreReadTransaction,
    StateStoreWriteTransaction,
    StorageError,
};

#[derive(Debug, Clone, Deserialize)]
pub struct TransactionRecord {
    pub transaction: Transaction,
    pub execution_result: Option<ExecuteResult>,
    pub execution_time: Option<Duration>,
    pub resulting_outputs: Vec<VersionedSubstateId>,
    pub resolved_inputs: Option<Vec<VersionedSubstateIdLockIntent>>,
    pub final_decision: Option<Decision>,
    pub finalized_time: Option<Duration>,
    pub abort_details: Option<String>,
}

impl TransactionRecord {
    pub fn new(transaction: Transaction) -> Self {
        Self {
            transaction,
            execution_result: None,
            resolved_inputs: None,
            execution_time: None,
            final_decision: None,
            finalized_time: None,
            resulting_outputs: Vec::new(),
            abort_details: None,
        }
    }

    pub fn load(
        transaction: Transaction,
        result: Option<ExecuteResult>,
        resolved_inputs: Option<Vec<VersionedSubstateIdLockIntent>>,
        execution_time: Option<Duration>,
        final_decision: Option<Decision>,
        finalized_time: Option<Duration>,
        resulting_outputs: Vec<VersionedSubstateId>,
        abort_details: Option<String>,
    ) -> Self {
        Self {
            transaction,
            resolved_inputs,
            execution_result: result,
            execution_time,
            final_decision,
            finalized_time,
            resulting_outputs,
            abort_details,
        }
    }

    pub fn id(&self) -> &TransactionId {
        self.transaction.id()
    }

    pub fn transaction(&self) -> &Transaction {
        &self.transaction
    }

    pub fn transaction_mut(&mut self) -> &mut Transaction {
        &mut self.transaction
    }

    pub fn into_transaction(self) -> Transaction {
        self.transaction
    }

    pub fn execution_result(&self) -> Option<&ExecuteResult> {
        self.execution_result.as_ref()
    }

    pub fn has_executed(&self) -> bool {
        self.execution_result.is_some()
    }

    pub fn resulting_outputs(&self) -> &[VersionedSubstateId] {
        &self.resulting_outputs
    }

    pub fn resolved_inputs(&self) -> Option<&[VersionedSubstateIdLockIntent]> {
        self.resolved_inputs.as_deref()
    }

    pub fn execution_decision(&self) -> Option<Decision> {
        self.execution_result().map(|r| Decision::from(&r.finalize.result))
    }

    pub fn current_decision(&self) -> Decision {
        self.final_decision
            .or_else(|| self.abort_details.as_ref().map(|_| Decision::Abort))
            .or_else(|| self.execution_decision())
            // We will choose to commit a transaction unless (1) we aborted it, (2) the execution has failed
            .unwrap_or(Decision::Commit)
    }

    pub fn final_decision(&self) -> Option<Decision> {
        self.final_decision
    }

    pub fn execution_time(&self) -> Option<Duration> {
        self.execution_time
    }

    pub fn finalized_time(&self) -> Option<Duration> {
        self.finalized_time
    }

    pub fn is_finalized(&self) -> bool {
        self.final_decision.is_some()
    }

    pub fn is_executed(&self) -> bool {
        self.execution_result.is_some()
    }

    pub fn abort_details(&self) -> Option<&String> {
        self.abort_details.as_ref()
    }

    pub fn set_abort<T: Into<String>>(&mut self, details: T) -> &mut Self {
        self.final_decision = Some(Decision::Abort);
        self.abort_details = Some(details.into());
        self
    }

    pub fn set_current_decision_to_abort<T: Into<String>>(&mut self, details: T) -> &mut Self {
        self.abort_details = Some(details.into());
        self
    }

    pub fn into_final_result(self) -> Option<ExecuteResult> {
        // TODO: This is hacky, result should be broken up into execution result, validation (mempool) result, finality
        //       result. These results are independent of each other.
        self.final_decision().and_then(|d| {
            if d.is_commit() {
                // Is is expected that the result is ACCEPT.
                // TODO: Handle (elsewhere) the edge-case where our execution failed but the committee decided to COMMIT
                // (fetch the state transitions from a peer?)
                self.execution_result
            } else {
                // Only use rejected results for the transaction. If execution ACCEPTed but the final decision is ABORT,
                // then use abort_details (which should have been set in this case).
                let finalize_result = self
                    .execution_result
                    .map(|r| r.finalize)
                    .filter(|f| !f.result.is_accept());
                Some(ExecuteResult {
                    finalize: finalize_result.unwrap_or_else(|| {
                        FinalizeResult::new_rejected(
                            self.transaction.id().into_array().into(),
                            RejectReason::ShardRejected(format!(
                                "Validators decided to abort: {}",
                                self.abort_details
                                    .as_deref()
                                    .unwrap_or("<invalid state, no abort details>")
                            )),
                        )
                    }),
                })
            }
        })
    }
}

impl TransactionRecord {
    pub fn insert<TTx: StateStoreWriteTransaction>(&self, tx: &mut TTx) -> Result<(), StorageError> {
        tx.transactions_insert(self)
    }

    pub fn save<TTx>(&self, tx: &mut TTx) -> Result<(), StorageError>
    where
        TTx: StateStoreWriteTransaction + Deref,
        TTx::Target: StateStoreReadTransaction,
    {
        if !Self::exists(&**tx, self.transaction.id())? {
            self.insert(tx)?;
        }
        Ok(())
    }

    pub fn save_all<'a, TTx, I>(tx: &mut TTx, transactions: I) -> Result<(), StorageError>
    where
        TTx: StateStoreWriteTransaction + Deref,
        TTx::Target: StateStoreReadTransaction,
        I: IntoIterator<Item = &'a TransactionRecord>,
    {
        tx.transactions_save_all(transactions)
    }

    pub fn update<TTx: StateStoreWriteTransaction>(&self, tx: &mut TTx) -> Result<(), StorageError> {
        tx.transactions_update(self)
    }

    pub fn upsert<TTx>(&self, tx: &mut TTx) -> Result<(), StorageError>
    where
        TTx: StateStoreWriteTransaction + Deref,
        TTx::Target: StateStoreReadTransaction,
    {
        if TransactionRecord::exists(&**tx, self.id())? {
            self.update(tx)
        } else {
            self.insert(tx)
        }
    }

    pub fn get<TTx: StateStoreReadTransaction>(tx: &TTx, tx_id: &TransactionId) -> Result<Self, StorageError> {
        tx.transactions_get(tx_id)
    }

    pub fn exists<TTx: StateStoreReadTransaction + ?Sized>(
        tx: &TTx,
        tx_id: &TransactionId,
    ) -> Result<bool, StorageError> {
        tx.transactions_exists(tx_id)
    }

    pub fn exists_any<'a, TTx: StateStoreReadTransaction + ?Sized, I: IntoIterator<Item = &'a TransactionId>>(
        tx: &TTx,
        tx_ids: I,
    ) -> Result<bool, StorageError> {
        for tx_id in tx_ids {
            if tx.transactions_exists(tx_id)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn get_any<'a, TTx: StateStoreReadTransaction, I: IntoIterator<Item = &'a TransactionId>>(
        tx: &TTx,
        tx_ids: I,
    ) -> Result<(Vec<Self>, HashSet<TransactionId>), StorageError> {
        let mut tx_ids = tx_ids.into_iter().copied().collect::<HashSet<_>>();
        let recs = tx.transactions_get_any(tx_ids.iter())?;
        for rec in &recs {
            tx_ids.remove(rec.transaction.id());
        }

        Ok((recs, tx_ids))
    }

    pub fn get_missing<'a, TTx: StateStoreReadTransaction, I: IntoIterator<Item = &'a TransactionId>>(
        tx: &TTx,
        tx_ids: I,
    ) -> Result<HashSet<TransactionId>, StorageError> {
        // TODO(perf): optimise
        let (_, missing) = Self::get_any(tx, tx_ids)?;
        Ok(missing)
    }

    pub fn get_paginated<TTx: StateStoreReadTransaction>(
        tx: &TTx,
        limit: u64,
        offset: u64,
        ordering: Option<Ordering>,
    ) -> Result<Vec<Self>, StorageError> {
        tx.transactions_get_paginated(limit, offset, ordering)
    }

    pub fn finalize_all<'a, TTx, I>(tx: &mut TTx, block_id: BlockId, transactions: I) -> Result<(), StorageError>
    where
        TTx: StateStoreWriteTransaction + Deref,
        TTx::Target: StateStoreReadTransaction,
        I: IntoIterator<Item = &'a TransactionAtom>,
    {
        tx.transactions_finalize_all(block_id, transactions)
    }
}

impl From<ExecutedTransaction> for TransactionRecord {
    fn from(tx: ExecutedTransaction) -> Self {
        let execution_time = tx.execution_time();
        let final_decision = tx.final_decision();
        let finalized_time = tx.finalized_time();
        let abort_details = tx.abort_details().cloned();
        let (transaction, result, resolved_inputs, resulting_outputs) = tx.dissolve();

        Self {
            transaction,
            execution_result: Some(result),
            execution_time: Some(execution_time),
            resolved_inputs: Some(resolved_inputs),
            final_decision,
            finalized_time,
            resulting_outputs,
            abort_details,
        }
    }
}
