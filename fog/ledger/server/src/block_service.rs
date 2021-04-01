// Copyright (c) 2018-2021 The MobileCoin Foundation

use fog_api::{
    external,
    ledger::{Block, BlockRequest, BlockResponse},
    ledger_grpc::FogBlockApi,
};
use grpcio::{RpcContext, RpcStatus, UnarySink};
use mc_common::logger::{log, Logger};
use mc_ledger_db::{self, Error as DbError, Ledger};
use mc_util_grpc::{rpc_database_err, rpc_logger, send_result, Authenticator};
use mc_util_metrics::SVC_COUNTERS;
use mc_watcher::watcher_db::WatcherDB;
use mc_watcher_api::TimestampResultCode;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct BlockService<L: Ledger + Clone> {
    ledger: L,
    watcher: WatcherDB,
    authenticator: Arc<dyn Authenticator + Send + Sync>,
    logger: Logger,
    watcher_timeout: Duration,
    polling_frequency: Duration,
    error_retry_frequency: Duration,
}

impl<L: Ledger + Clone> BlockService<L> {
    pub fn new(
        ledger: L,
        watcher: WatcherDB,
        authenticator: Arc<dyn Authenticator + Send + Sync>,
        logger: Logger,
        watcher_timeout: Duration,
        polling_frequency: Duration,
        error_retry_frequency: Duration,
    ) -> Self {
        Self {
            ledger,
            watcher,
            authenticator,
            logger,
            watcher_timeout,
            polling_frequency,
            error_retry_frequency,
        }
    }

    fn get_blocks_impl(&mut self, request: BlockRequest) -> Result<BlockResponse, RpcStatus> {
        mc_common::trace_time!(self.logger, "Get Blocks");

        let mut result = BlockResponse::new();

        result.num_blocks = self
            .ledger
            .num_blocks()
            .map_err(|err| rpc_database_err(err, &self.logger))?;
        result.global_txo_count = self
            .ledger
            .num_txos()
            .map_err(|err| rpc_database_err(err, &self.logger))?;

        for range in request.ranges.iter() {
            for block_idx in range.start_block..range.end_block {
                match self.get_block(block_idx) {
                    Ok(block) => result.blocks.push(block),
                    // TODO: signal internal error for some errors?
                    Err(err) => {
                        log::error!(self.logger, "DbError getting block {}: {}", block_idx, err)
                    }
                };
            }
        }

        Ok(result)
    }

    fn get_block(&mut self, block_index: u64) -> Result<Block, DbError> {
        let mut result = Block::new();
        let block_contents = self.ledger.get_block_contents(block_index)?;
        let block = self.ledger.get_block(block_index)?;
        for output in block_contents.outputs {
            result.outputs.push(external::TxOut::from(&output));
        }
        result.index = block_index;
        result.global_txo_count = block.cumulative_txo_count;

        // Timer that tracks how long we have had WatcherBehind error for,
        // if this exceeds watcher_timeout, we log a warning.
        let mut watcher_behind_timer = Instant::now();

        // Get the timestamp of the block_index if possible
        let (mut timestamp, mut ts_result): (Option<u64>, Option<TimestampResultCode>);
        loop {
            let timestamp_result_code = match self.watcher.get_block_timestamp(block_index) {
                Ok((ts, res)) => match res {
                    TimestampResultCode::WatcherBehind => {
                        if watcher_behind_timer.elapsed() > self.watcher_timeout {
                            log::warn!(self.logger, "watcher is still behind on block index = {} after waiting {} seconds, ingest will be blocked", block_index, self.watcher_timeout.as_secs());
                            watcher_behind_timer = Instant::now();
                        }
                        std::thread::sleep(self.polling_frequency);
                        (None, None)
                    }
                    TimestampResultCode::BlockIndexOutOfBounds => {
                        log::warn!(self.logger, "block index {} was out of bounds, we should not be scanning it, we will have junk timestamps for it", block_index);
                        (Some(u64::MAX), Some(res))
                    }
                    TimestampResultCode::Unavailable => {
                        log::crit!(self.logger, "watcher configuration is wrong and timestamps will not be available with this configuration. Ingest is blocked at block index {}", block_index);
                        std::thread::sleep(self.error_retry_frequency);
                        (None, None)
                    }
                    TimestampResultCode::WatcherDatabaseError => {
                        log::crit!(self.logger, "The watcher database has an error which prevents us from getting timestamps. Ingest is blocked at block index {}", block_index);
                        std::thread::sleep(self.error_retry_frequency);
                        (None, None)
                    }
                    TimestampResultCode::TimestampFound => (Some(ts), Some(res)),
                },
                Err(err) => {
                    log::error!(
                        self.logger,
                        "Could not obtain timestamp for block {} due to error {:?}",
                        block_index,
                        err
                    );
                    std::thread::sleep(self.error_retry_frequency);
                    (None, None)
                }
            };

            timestamp = timestamp_result_code.0;
            ts_result = timestamp_result_code.1;

            if timestamp.is_some() && ts_result.is_some() {
                // Found a timestamp. Break from the loop.
                break;
            }
        }

        result.timestamp = timestamp.unwrap();
        result.timestamp_result_code = ts_result.unwrap() as u32;

        Ok(result)
    }
}

impl<L: Ledger + Clone> FogBlockApi for BlockService<L> {
    fn get_blocks(
        &mut self,
        ctx: RpcContext,
        request: BlockRequest,
        sink: UnarySink<BlockResponse>,
    ) {
        let _timer = SVC_COUNTERS.req(&ctx);
        mc_common::logger::scoped_global_logger(&rpc_logger(&ctx, &self.logger), |logger| {
            if let Err(err) = self.authenticator.authenticate_rpc(&ctx) {
                return send_result(ctx, sink, err.into(), &logger);
            }

            send_result(ctx, sink, self.get_blocks_impl(request), &logger)
        })
    }
}
