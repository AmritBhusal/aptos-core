// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counters,
    errors::*,
    output_delta_resolver::OutputDeltaResolver,
    scheduler::{DependencyStatus, Scheduler, SchedulerTask, TaskGuard, Version},
    task::{ExecutionStatus, ExecutorTask, Transaction, TransactionOutput},
    txn_last_input_output::TxnLastInputOutput,
    view::{LatestView, MVHashMapView},
};
use aptos_mvhashmap::{MVHashMap, MVHashMapError, MVHashMapOutput, TxnIndex};
use aptos_state_view::TStateView;
use num_cpus;
use once_cell::sync::Lazy;
use std::{collections::btree_map::BTreeMap, hint, marker::PhantomData};

pub static RAYON_EXEC_POOL: Lazy<rayon::ThreadPool> = Lazy::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_cpus::get())
        .thread_name(|index| format!("par_exec_{}", index))
        .build()
        .unwrap()
});

pub struct BlockExecutor<T, E, S> {
    // number of active concurrent tasks, corresponding to the maximum number of rayon
    // threads that may be concurrently participating in parallel execution.
    concurrency_level: usize,
    phantom: PhantomData<(T, E, S)>,
}

impl<T, E, S> BlockExecutor<T, E, S>
where
    T: Transaction,
    E: ExecutorTask<Txn = T>,
    S: TStateView<Key = T::Key>,
{
    /// The caller needs to ensure that concurrency_level > 1 (0 is illegal and 1 should
    /// be handled by sequential execution) and that concurrency_level <= num_cpus.
    pub fn new(concurrency_level: usize) -> Self {
        assert!(
            concurrency_level > 0 && concurrency_level <= num_cpus::get(),
            "Parallel execution concurrency level {} should be between 1 and number of CPUs",
            concurrency_level
        );
        Self {
            concurrency_level,
            phantom: PhantomData,
        }
    }

    fn execute<'a>(
        &self,
        version: Version,
        guard: TaskGuard<'a>,
        signature_verified_block: &[T],
        last_input_output: &TxnLastInputOutput<T::Key, E::Output, E::Error>,
        versioned_data_cache: &MVHashMap<T::Key, T::Value>,
        scheduler: &'a Scheduler,
        executor: &E,
        base_view: &S,
    ) -> SchedulerTask<'a> {
        let (idx_to_execute, incarnation) = version;
        let txn = &signature_verified_block[idx_to_execute];

        let speculative_view = MVHashMapView::new(versioned_data_cache, scheduler);

        // VM execution.
        let execute_result = executor.execute_transaction(
            &LatestView::<T, S>::new_mv_view(base_view, &speculative_view, idx_to_execute),
            txn,
            idx_to_execute,
            false,
        );
        let mut prev_modified_keys = last_input_output.modified_keys(idx_to_execute);

        // For tracking whether the recent execution wrote outside of the previous write/delta set.
        let mut updates_outside = false;
        let mut apply_updates = |output: &<E as ExecutorTask>::Output| {
            // First, apply writes.
            let write_version = (idx_to_execute, incarnation);
            for (k, v) in output.get_writes().into_iter() {
                if !prev_modified_keys.remove(&k) {
                    updates_outside = true;
                }
                versioned_data_cache.add_write(&k, write_version, v);
            }

            // Then, apply deltas.
            for (k, d) in output.get_deltas().into_iter() {
                if !prev_modified_keys.remove(&k) {
                    updates_outside = true;
                }
                versioned_data_cache.add_delta(&k, idx_to_execute, d);
            }
        };

        let result = match execute_result {
            // These statuses are the results of speculative execution, so even for
            // SkipRest (skip the rest of transactions) and Abort (abort execution with
            // user defined error), no immediate action is taken. Instead the statuses
            // are recorded and (final statuses) are analyzed when the block is executed.
            ExecutionStatus::Success(output) => {
                // Apply the writes/deltas to the versioned_data_cache.
                apply_updates(&output);
                ExecutionStatus::Success(output)
            }
            ExecutionStatus::SkipRest(output) => {
                // Apply the writes/deltas and record status indicating skip.
                apply_updates(&output);
                ExecutionStatus::SkipRest(output)
            }
            ExecutionStatus::Abort(err) => {
                // Record the status indicating abort.
                ExecutionStatus::Abort(Error::UserError(err))
            }
        };

        // Remove entries from previous write/delta set that were not overwritten.
        for k in prev_modified_keys {
            versioned_data_cache.delete(&k, idx_to_execute);
        }

        if last_input_output
            .record(idx_to_execute, speculative_view.take_reads(), result)
            .is_err()
        {
            // To terminate the commit thread.
            scheduler.set_commit_idx(0);
            // Optimization for module publishing fallback.
            // When there is module r/w intersection, can halt parallel execution
            // and fallback to sequential execution immediately.
            scheduler.halt();
            return SchedulerTask::NoTask;
        }
        scheduler.finish_execution(idx_to_execute, incarnation, updates_outside, guard)
    }

    fn validate<'a>(
        &self,
        version_to_validate: Version,
        guard: TaskGuard<'a>,
        last_input_output: &TxnLastInputOutput<T::Key, E::Output, E::Error>,
        versioned_data_cache: &MVHashMap<T::Key, T::Value>,
        scheduler: &'a Scheduler,
    ) -> SchedulerTask<'a> {
        use MVHashMapError::*;
        use MVHashMapOutput::*;

        let (idx_to_validate, incarnation) = version_to_validate;
        let read_set = last_input_output
            .read_set(idx_to_validate)
            .expect("Prior read-set must be recorded");

        let valid = read_set.iter().all(|r| {
            match versioned_data_cache.read(r.path(), idx_to_validate) {
                Ok(Version(version, _)) => r.validate_version(version),
                Ok(Resolved(value)) => r.validate_resolved(value),
                Err(Dependency(_)) => false, // Dependency implies a validation failure.
                Err(Unresolved(delta)) => r.validate_unresolved(delta),
                Err(NotFound) => r.validate_storage(),
                // We successfully validate when read (again) results in a delta application
                // failure. If the failure is speculative, a later validation will fail due to
                // a read without this error. However, if the failure is real, passing
                // validation here allows to avoid infinitely looping and instead panic when
                // materializing deltas as writes in the final output preparation state. Panic
                // is also preferrable as it allows testing for this scenario.
                Err(DeltaApplicationFailure) => r.validate_delta_application_failure(),
            }
        });

        let aborted = !valid && scheduler.try_abort(idx_to_validate, incarnation);

        if aborted {
            counters::SPECULATIVE_ABORT_COUNT.inc();

            // Not valid and successfully aborted, mark the latest write/delta sets as estimates.
            for k in last_input_output.modified_keys(idx_to_validate) {
                versioned_data_cache.mark_estimate(&k, idx_to_validate);
            }

            scheduler.finish_abort(idx_to_validate, incarnation, guard)
        } else {
            SchedulerTask::NoTask
        }
    }

    fn commit<'a>(
        &self,
        idx_to_commit: TxnIndex,
        last_input_output: &TxnLastInputOutput<T::Key, E::Output, E::Error>,
        versioned_data_cache: &MVHashMap<T::Key, T::Value>,
    ) -> bool {
        use MVHashMapError::*;
        use MVHashMapOutput::*;

        let read_set = last_input_output
            .read_set(idx_to_commit)
            .expect("Prior read-set must be recorded");

        let valid = read_set.iter().all(|r| {
            match versioned_data_cache.read(r.path(), idx_to_commit) {
                Ok(Version(version, _)) => r.validate_version(version),
                Ok(Resolved(value)) => r.validate_resolved(value),
                Err(Dependency(_)) => false, // Dependency implies a validation failure.
                Err(Unresolved(delta)) => r.validate_unresolved(delta),
                Err(NotFound) => r.validate_storage(),
                // We successfully validate when read (again) results in a delta application
                // failure. If the failure is speculative, a later validation will fail due to
                // a read without this error. However, if the failure is real, passing
                // validation here allows to avoid infinitely looping and instead panic when
                // materializing deltas as writes in the final output preparation state. Panic
                // is also preferrable as it allows testing for this scenario.
                Err(DeltaApplicationFailure) => r.validate_delta_application_failure(),
            }
        });

        valid
    }

    fn work_task_with_scope(
        &self,
        executor_arguments: &E::Argument,
        block: &[T],
        last_input_output: &TxnLastInputOutput<T::Key, E::Output, E::Error>,
        versioned_data_cache: &MVHashMap<T::Key, T::Value>,
        scheduler: &Scheduler,
        base_view: &S,
    ) {
        // Make executor for each task. TODO: fast concurrent executor.
        let executor = E::init(*executor_arguments);

        let mut scheduler_task = SchedulerTask::NoTask;

        let is_commit_thread = scheduler.is_commit_thread();

        if is_commit_thread {
            // The commit thread keeps validating the next transaction.
            // If validated, it increment the local commit index.
            let mut local_commit_idx = 0;
            let mut local_commit_gas = 0;

            while local_commit_idx < scheduler.commit_idx()
                && local_commit_gas < scheduler.per_block_gas_limit()
            {
                if !scheduler.ready_for_commit(local_commit_idx) {
                    // Avoid pointlessly spinning, and give priority to other threads that may
                    // be working to finish the remaining tasks.
                    hint::spin_loop();
                    continue;
                }

                if self.commit(local_commit_idx, last_input_output, versioned_data_cache) {
                    // Read the gas from the execution output.
                    let txn_gas = match last_input_output.write_set(local_commit_idx).as_ref() {
                        ExecutionStatus::Success(t) => t.gas_used(),
                        ExecutionStatus::SkipRest(t) => t.gas_used(),
                        ExecutionStatus::Abort(_) => 0,
                    };
                    local_commit_gas += txn_gas;
                    local_commit_idx += 1;
                }
            }
            scheduler.set_commit_idx(local_commit_idx);
            scheduler.halt();
        } else {
            // Other threads perform execution/vaidation tasks.
            loop {
                scheduler_task = match scheduler_task {
                    SchedulerTask::ValidationTask(version_to_validate, guard) => self.validate(
                        version_to_validate,
                        guard,
                        last_input_output,
                        versioned_data_cache,
                        scheduler,
                    ),
                    SchedulerTask::ExecutionTask(version_to_execute, None, guard) => self.execute(
                        version_to_execute,
                        guard,
                        block,
                        last_input_output,
                        versioned_data_cache,
                        scheduler,
                        &executor,
                        base_view,
                    ),
                    SchedulerTask::ExecutionTask(_, Some(condvar), _guard) => {
                        let (lock, cvar) = &*condvar;
                        // Mark dependency resolved.
                        *lock.lock() = DependencyStatus::Resolved;
                        // Wake up the process waiting for dependency.
                        cvar.notify_one();

                        SchedulerTask::NoTask
                    }
                    SchedulerTask::NoTask => scheduler.next_task(),
                    SchedulerTask::Done => {
                        break;
                    }
                }
            }
        }
    }

    pub fn execute_transactions_parallel(
        &self,
        executor_initial_arguments: E::Argument,
        signature_verified_block: &Vec<T>,
        base_view: &S,
    ) -> Result<(Vec<E::Output>, OutputDeltaResolver<T::Key, T::Value>), E::Error> {
        assert!(self.concurrency_level > 1, "Must use sequential execution");

        let versioned_data_cache = MVHashMap::new();

        if signature_verified_block.is_empty() {
            return Ok((vec![], OutputDeltaResolver::new(versioned_data_cache)));
        }

        let num_txns = signature_verified_block.len();
        let last_input_output = TxnLastInputOutput::new(num_txns);
        let scheduler = Scheduler::new(num_txns);

        RAYON_EXEC_POOL.scope(|s| {
            for _ in 0..self.concurrency_level {
                s.spawn(|_| {
                    self.work_task_with_scope(
                        &executor_initial_arguments,
                        signature_verified_block,
                        &last_input_output,
                        &versioned_data_cache,
                        &scheduler,
                        base_view,
                    );
                });
            }
        });

        // TODO: for large block sizes and many cores, extract outputs in parallel.
        let num_txns = scheduler.commit_idx();
        let mut final_results = Vec::with_capacity(num_txns);

        let maybe_err = if last_input_output.module_publishing_may_race() {
            counters::MODULE_PUBLISHING_FALLBACK_COUNT.inc();
            Some(Error::ModulePathReadWrite)
        } else {
            let mut ret = None;
            for idx in 0..num_txns {
                match last_input_output.take_output(idx) {
                    ExecutionStatus::Success(t) => final_results.push(t),
                    ExecutionStatus::SkipRest(t) => {
                        final_results.push(t);
                        break;
                    }
                    ExecutionStatus::Abort(err) => {
                        ret = Some(err);
                        break;
                    }
                };
            }
            ret
        };

        RAYON_EXEC_POOL.spawn(move || {
            // Explicit async drops.
            drop(last_input_output);
            drop(scheduler);
        });

        match maybe_err {
            Some(err) => Err(err),
            None => {
                final_results.resize_with(num_txns, E::Output::skip_output);
                Ok((
                    final_results,
                    OutputDeltaResolver::new(versioned_data_cache),
                ))
            }
        }
    }

    pub fn execute_transactions_sequential(
        &self,
        executor_arguments: E::Argument,
        signature_verified_block: &[T],
        base_view: &S,
    ) -> Result<Vec<E::Output>, E::Error> {
        let num_txns = signature_verified_block.len();
        let executor = E::init(executor_arguments);
        let mut data_map = BTreeMap::new();

        let mut ret = Vec::with_capacity(num_txns);
        for (idx, txn) in signature_verified_block.iter().enumerate() {
            let res = executor.execute_transaction(
                &LatestView::<T, S>::new_btree_view(base_view, &data_map, idx),
                txn,
                idx,
                true,
            );

            let must_skip = matches!(res, ExecutionStatus::SkipRest(_));

            match res {
                ExecutionStatus::Success(output) | ExecutionStatus::SkipRest(output) => {
                    assert_eq!(
                        output.get_deltas().len(),
                        0,
                        "Sequential execution must materialize deltas"
                    );
                    // Apply the writes.
                    for (ap, write_op) in output.get_writes().into_iter() {
                        data_map.insert(ap, write_op);
                    }
                    ret.push(output);
                }
                ExecutionStatus::Abort(err) => {
                    // Record the status indicating abort.
                    return Err(Error::UserError(err));
                }
            }

            if must_skip {
                break;
            }
        }

        ret.resize_with(num_txns, E::Output::skip_output);
        Ok(ret)
    }
}
