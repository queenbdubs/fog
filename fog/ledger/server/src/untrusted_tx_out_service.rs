// Copyright (c) 2018-2021 The MobileCoin Foundation

use core::convert::TryFrom;

use fog_api::{
    ledger::{TxOutRequest, TxOutResponse, TxOutResult, TxOutResultCode},
    ledger_grpc::FogUntrustedTxOutApi,
};
use grpcio::{RpcContext, RpcStatus, UnarySink};
use mc_common::logger::{log, Logger};
use mc_crypto_keys::CompressedRistrettoPublic;
use mc_ledger_db::{self, Error as DbError, Ledger};
use mc_util_grpc::{rpc_internal_error, rpc_logger, send_result, Authenticator};
use mc_util_metrics::SVC_COUNTERS;
use mc_watcher::watcher_db::WatcherDB;
use mc_watcher_api::TimestampResultCode;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct UntrustedTxOutService<L: Ledger + Clone> {
    ledger: L,
    watcher: WatcherDB,
    authenticator: Arc<dyn Authenticator + Send + Sync>,
    logger: Logger,
    watcher_timeout: Duration,
    polling_frequency: Duration,
    error_retry_frequency: Duration,
}

impl<L: Ledger + Clone> UntrustedTxOutService<L> {
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

    fn get_tx_outs_impl(&mut self, request: TxOutRequest) -> Result<TxOutResponse, RpcStatus> {
        mc_common::trace_time!(self.logger, "Get Blocks");

        let mut response = TxOutResponse::new();

        response.num_blocks = self
            .ledger
            .num_blocks()
            .map_err(|err| rpc_internal_error("Database error", err, &self.logger))?;
        response.global_txo_count = self
            .ledger
            .num_txos()
            .map_err(|err| rpc_internal_error("Database error", err, &self.logger))?;

        for tx_out_pubkey_proto in request.tx_out_pubkeys.iter() {
            response.results.push(
                match CompressedRistrettoPublic::try_from(tx_out_pubkey_proto) {
                    Ok(tx_out_pubkey) => match self.get_tx_out(&tx_out_pubkey) {
                        Ok(result) => result,
                        Err(err) => {
                            log::error!(
                                self.logger,
                                "DbError getting tx_out {}: {}",
                                tx_out_pubkey,
                                err
                            );
                            let mut result = TxOutResult::new();
                            result.set_tx_out_pubkey(tx_out_pubkey_proto.clone());
                            result.result_code = TxOutResultCode::DatabaseError;
                            result
                        }
                    },
                    Err(err) => {
                        log::error!(
                            self.logger,
                            "Request was not a valid pubkey {:?}: {}",
                            tx_out_pubkey_proto,
                            err
                        );
                        let mut result = TxOutResult::new();
                        result.set_tx_out_pubkey(tx_out_pubkey_proto.clone());
                        result.result_code = TxOutResultCode::MalformedRequest;
                        result
                    }
                },
            )
        }

        Ok(response)
    }

    fn get_tx_out(
        &mut self,
        tx_out_pubkey: &CompressedRistrettoPublic,
    ) -> Result<TxOutResult, DbError> {
        let mut result = TxOutResult::new();
        result.set_tx_out_pubkey(tx_out_pubkey.into());

        let tx_out_index = match self.ledger.get_tx_out_index_by_public_key(tx_out_pubkey) {
            Ok(index) => index,
            Err(DbError::NotFound) => {
                result.result_code = TxOutResultCode::NotFound;
                return Ok(result);
            }
            Err(err) => {
                return Err(err);
            }
        };

        result.result_code = TxOutResultCode::Found;
        result.tx_out_global_index = tx_out_index;

        let block_index = self
            .ledger
            .get_block_index_by_tx_out_index(tx_out_index)
            .map_err(|err| {
                log::error!(
                    self.logger,
                    "Unexpected error when getting block by tx out index {}: {}",
                    tx_out_index,
                    err
                );
                err
            })?;

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

        result.block_index = block_index;
        result.timestamp = timestamp.unwrap();
        result.timestamp_result_code = ts_result.unwrap() as u32;

        Ok(result)
    }
}

impl<L: Ledger + Clone> FogUntrustedTxOutApi for UntrustedTxOutService<L> {
    fn get_tx_outs(
        &mut self,
        ctx: RpcContext,
        request: TxOutRequest,
        sink: UnarySink<TxOutResponse>,
    ) {
        let _timer = SVC_COUNTERS.req(&ctx);
        mc_common::logger::scoped_global_logger(&rpc_logger(&ctx, &self.logger), |logger| {
            if let Err(err) = self.authenticator.authenticate_rpc(&ctx) {
                return send_result(ctx, sink, err.into(), &logger);
            }

            send_result(ctx, sink, self.get_tx_outs_impl(request), &logger)
        })
    }
}
