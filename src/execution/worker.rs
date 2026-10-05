use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use tokio::sync::mpsc::Receiver;
use tracing::{error, info, warn};

use crate::{
    config::AppConfig,
    decode::TransactionDecoder,
    domain::{AssetId, AttemptStatus, ObservedTransaction, SkipReason, TradeIntent},
    error::{CopyTraderError, Result},
    execution::{ExecutionBackend, confirm::wait_for_confirmation, sizing::SizingPolicy},
    routing::{PrefetchedSource, Router, RoutingInputs, RoutingTimings},
    signal::QueuedObservation,
    storage::{DatabaseTimings, Store, TimingWriter},
    surfpool::seed_token_balance,
    token::{
        accounts::associated_token_address,
        extensions::{MintInfo, TokenSafetyClient},
    },
};

pub struct ExecutionWorker {
    config: Arc<AppConfig>,
    signer: Arc<Keypair>,
    store: Store,
    backend: Arc<ExecutionBackend>,
    token_safety: TokenSafetyClient,
    decoder: TransactionDecoder,
    router: Arc<Router>,
    last_executed_slot: u64,
}

impl ExecutionWorker {
    pub fn new(
        config: Arc<AppConfig>,
        signer: Arc<Keypair>,
        store: Store,
        backend: Arc<ExecutionBackend>,
        token_safety: TokenSafetyClient,
        decoder: TransactionDecoder,
        router: Arc<Router>,
    ) -> Self {
        Self {
            config,
            signer,
            store,
            backend,
            token_safety,
            decoder,
            router,
            last_executed_slot: 0,
        }
    }

    pub async fn run(mut self, input: Receiver<QueuedObservation>) -> Result<()> {
        let writer = TimingWriter::new(self.store.clone());
        let result = self.run_inner(input, &writer).await;
        writer.finish().await;
        result
    }

    async fn run_inner(
        &mut self,
        mut input: Receiver<QueuedObservation>,
        writer: &TimingWriter,
    ) -> Result<()> {
        while let Some(queued) = input.recv().await {
            let source_signature = queued.observed.signature.to_string();
            let mut timings = CopyTimings::new(queued.received_at);
            timings.database = queued.database_timings;
            self.store = self.store.with_timings(timings.database.clone());
            let result = self.handle(queued.observed, &mut timings).await;
            timings.finish();
            if let Err(error) = result {
                self.store
                    .update_attempt(
                        &source_signature,
                        AttemptStatus::Failed,
                        Some(&error.to_string()),
                    )
                    .await?;
                error!(%error, %source_signature, "copy attempt failed");
            }
            let json = timings.json()?;
            info!(%source_signature, timings = %json, "copy timings");
            writer.enqueue(source_signature, json);
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        observed: ObservedTransaction,
        timings: &mut CopyTimings,
    ) -> Result<()> {
        let source_signature = observed.signature.to_string();
        if observed.slot < self.last_executed_slot {
            self.store
                .mark_skipped(&source_signature, SkipReason::OutOfOrder)
                .await?;
            return Ok(());
        }
        if self.is_stale(&observed)? {
            self.store
                .mark_skipped(&source_signature, SkipReason::StaleSignal)
                .await?;
            return Ok(());
        }
        let intent = match self.decoder.decode(&observed) {
            Ok(intent) => intent,
            Err(CopyTraderError::Unsupported(reason)) => {
                warn!(%source_signature, %reason, "source transaction has no unambiguous trade intent");
                self.store
                    .mark_skipped(&source_signature, SkipReason::AmbiguousAttribution)
                    .await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let input_mint = intent.input_asset.routing_mint();
        let output_mint = intent.output_asset.routing_mint();
        let Some(input_rule) = self.config.token_rule(input_mint) else {
            self.store.mark_intent(&source_signature, &intent).await?;
            self.store
                .mark_skipped(&source_signature, SkipReason::MintNotAllowed)
                .await?;
            return Ok(());
        };
        if self.config.token_rule(output_mint).is_none() {
            self.store.mark_intent(&source_signature, &intent).await?;
            self.store
                .mark_skipped(&source_signature, SkipReason::MintNotAllowed)
                .await?;
            return Ok(());
        }
        let mut routing_timings = RoutingTimings::default();
        let prefetched = self
            .router
            .prefetch_source(&intent, &routing_timings.stages);
        let (intent_write, reads) = tokio::join!(
            self.store.mark_intent(&source_signature, &intent),
            self.initial_reads(&intent, prefetched.as_ref()),
        );
        intent_write?;
        let InitialReads {
            input_info,
            output_info,
            native_balance,
        } = reads?;
        let sizing = SizingPolicy::new(&self.config.sizing);
        let sized = match sizing.size_trade(intent, &input_rule, input_info.decimals)? {
            Ok(sized) => sized,
            Err(reason) => {
                self.store.mark_skipped(&source_signature, reason).await?;
                return Ok(());
            }
        };
        let input_account = associated_token_address(
            &self.signer.pubkey(),
            &input_mint,
            &input_info.token_program,
        );
        if self.config.allows_all_tokens()
            && !matches!(sized.intent.input_asset, AssetId::NativeSol)
            && let (Some(surfpool), Some(surfpool_config)) =
                (self.backend.surfpool(), self.config.surfpool.as_ref())
        {
            seed_token_balance(
                &surfpool_config.run_id,
                &input_rule,
                &input_info,
                &self.store,
                surfpool,
                self.signer.as_ref(),
            )
            .await?;
        }
        let available = if let Some(balance) = native_balance {
            balance
        } else {
            self.asset_balance(sized.intent.input_asset, &input_account)
                .await?
        };
        if available < sized.input_amount {
            self.store
                .mark_skipped(&source_signature, SkipReason::InsufficientBalance)
                .await?;
            return Ok(());
        }
        if !self
            .store
            .reserve_attempt(
                &source_signature,
                self.backend.target(),
                sized.input_amount,
                0,
            )
            .await?
        {
            return Ok(());
        }

        timings.stage("route_ms");
        let started = Instant::now();
        let result = self
            .router
            .race_prepared(
                &sized,
                self.signer.as_ref(),
                self.backend.as_ref(),
                self.config.execution.slippage_bps,
                RoutingInputs {
                    mint_infos: (&input_info, &output_info),
                    source: prefetched,
                },
                &mut routing_timings,
            )
            .await;
        timings.finish();
        let route_latency_ms =
            CopyTimings::millis(started.elapsed()).saturating_sub(routing_timings.discovery_ms);
        timings.values.insert("route_ms", route_latency_ms);
        timings
            .values
            .insert("discovery_ms", routing_timings.discovery_ms);
        timings
            .values
            .insert("source_route_ms", routing_timings.source_route_ms);
        timings.values.extend(routing_timings.stages.snapshot());
        let winner = result?;
        timings.stage("post_route_ms");

        let output_account = associated_token_address(
            &self.signer.pubkey(),
            &output_mint,
            &output_info.token_program,
        );
        let output_before = if let Some(balance) = winner.output_balance_before {
            balance
        } else {
            self.asset_balance_or_zero(sized.intent.output_asset, &output_account)
                .await?
        };
        let signed = bincode::serialize(&winner.transaction).map_err(|error| {
            CopyTraderError::Execution(format!("failed to serialize signed transaction: {error}"))
        })?;
        let local_signature = winner
            .transaction
            .signatures
            .first()
            .copied()
            .ok_or_else(|| {
                CopyTraderError::Execution("signed transaction has no signature".to_owned())
            })?;
        timings
            .values
            .insert("db_pre_send_us", timings.database.total_us());
        timings.stage("sender_request_ms");
        timings.since_receipt("receipt_to_send_start_ms");
        let send_result = self.backend.send(&winner.transaction).await;
        timings.since_receipt("receipt_to_send_response_ms");
        timings.finish();
        timings.stage("post_send_journal_ms");
        if let Err(error) = self
            .store
            .mark_route(
                &source_signature,
                winner.route.dex.as_str(),
                &winner.route.pool.to_string(),
                winner.route.minimum_output,
                winner.route.expected_output,
                route_latency_ms,
            )
            .await
        {
            warn!(%error, %source_signature, %local_signature, "route journal write failed after submission");
        }
        if let Err(error) = self
            .store
            .persist_signed(
                &source_signature,
                &local_signature.to_string(),
                &signed,
                &winner.simulation_json,
            )
            .await
        {
            warn!(%error, %source_signature, %local_signature, "signed transaction journal write failed after submission");
        }
        timings.finish();
        match send_result {
            Ok(returned) if returned == local_signature => {}
            Ok(returned) => {
                self.store
                    .update_attempt(
                        &source_signature,
                        AttemptStatus::Unknown,
                        Some(&format!("backend returned unexpected signature {returned}")),
                    )
                    .await?;
                return Ok(());
            }
            Err(error) => {
                self.store
                    .update_attempt(
                        &source_signature,
                        AttemptStatus::Unknown,
                        Some(&error.to_string()),
                    )
                    .await?;
                return Ok(());
            }
        }
        timings.stage("confirmation_ms");
        let confirmation = match wait_for_confirmation(
            self.backend.rpc(),
            self.backend.label(),
            &local_signature,
            Duration::from_secs(self.config.execution.confirmation_timeout_seconds),
        )
        .await
        {
            Ok(confirmation) => confirmation,
            Err(error) => {
                self.store
                    .update_attempt(
                        &source_signature,
                        AttemptStatus::Unknown,
                        Some(&error.to_string()),
                    )
                    .await?;
                return Ok(());
            }
        };
        self.store
            .record_landed_slot(&source_signature, confirmation.slot)
            .await?;
        if let Some(error) = confirmation.error {
            self.store
                .update_attempt(&source_signature, AttemptStatus::Failed, Some(&error))
                .await?;
            return Ok(());
        }

        timings.stage("reconciliation_ms");
        let input_after = self
            .asset_balance(sized.intent.input_asset, &input_account)
            .await?;
        let spent = available.checked_sub(input_after).ok_or_else(|| {
            CopyTraderError::Execution("input balance increased after swap".to_owned())
        })?;
        if spent < sized.input_amount {
            self.store
                .update_attempt(
                    &source_signature,
                    AttemptStatus::Failed,
                    Some("landed input debit was below the prepared swap amount"),
                )
                .await?;
            return Ok(());
        }

        let output_after = self
            .asset_balance(sized.intent.output_asset, &output_account)
            .await?;
        let native_cost_adjustment = if matches!(sized.intent.output_asset, AssetId::NativeSol) {
            let fee = self
                .backend
                .rpc()
                .get_fee_for_message(&winner.transaction.message)
                .await
                .map_err(|error| {
                    CopyTraderError::Execution(format!(
                        "failed to calculate landed transaction fee: {error}"
                    ))
                })?;
            fee.checked_add(
                self.backend
                    .mainnet()
                    .map_or(0, |mainnet| mainnet.tip_lamports()),
            )
            .ok_or_else(|| CopyTraderError::Execution("landed fee overflow".to_owned()))?
        } else {
            0
        };
        let adjusted_output_after = output_after
            .checked_add(native_cost_adjustment)
            .ok_or_else(|| CopyTraderError::Execution("output balance overflow".to_owned()))?;
        let received = adjusted_output_after
            .checked_sub(output_before)
            .ok_or_else(|| {
                CopyTraderError::Execution("output balance decreased after swap".to_owned())
            })?;
        if received < winner.route.minimum_output {
            self.store
                .update_attempt(
                    &source_signature,
                    AttemptStatus::Failed,
                    Some("landed output did not satisfy route minimum"),
                )
                .await?;
            return Ok(());
        }
        self.store
            .update_attempt(&source_signature, AttemptStatus::Landed, None)
            .await?;
        self.last_executed_slot = observed.slot;
        info!(%source_signature, %local_signature, dex = winner.route.dex.as_str(), pool = %winner.route.pool, source_slot = observed.slot, landed_slot = confirmation.slot, route_latency_ms, "copy landed");
        Ok(())
    }

    fn is_stale(&self, observed: &ObservedTransaction) -> Result<bool> {
        let Some(block_time) = observed.block_time else {
            return Ok(false);
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                CopyTraderError::Execution(format!("system clock is before epoch: {error}"))
            })?
            .as_secs();
        let block_time = u64::try_from(block_time)
            .map_err(|_| CopyTraderError::Execution("source block time is negative".to_owned()))?;
        Ok(now.saturating_sub(block_time) > self.config.execution.max_signal_age_seconds)
    }

    async fn initial_reads(
        &self,
        intent: &TradeIntent,
        source: Option<&PrefetchedSource>,
    ) -> Result<InitialReads> {
        let preparation = async {
            tokio::try_join!(
                self.token_safety.inspect_pair(
                    intent.input_asset.routing_mint(),
                    intent.output_asset.routing_mint()
                ),
                async {
                    if matches!(intent.input_asset, AssetId::NativeSol) {
                        self.asset_balance(AssetId::NativeSol, &self.signer.pubkey())
                            .await
                            .map(Some)
                    } else {
                        Ok(None)
                    }
                }
            )
        };
        tokio::pin!(preparation);
        let ((input_info, output_info), native_balance) = if let Some(source) = source {
            tokio::select! {
                result = &mut preparation => result?,
                _ = source.account.clone() => preparation.await?,
            }
        } else {
            preparation.await?
        };
        Ok(InitialReads {
            input_info,
            output_info,
            native_balance,
        })
    }

    #[cfg(test)]
    async fn post_route_reads(
        &self,
        message: &solana_sdk::message::Message,
        output_asset: AssetId,
        output_account: &Pubkey,
    ) -> Result<(u64, u64, u64)> {
        let owner = self.signer.pubkey();
        let (fee, (available, output)) = tokio::try_join!(
            super::preflight::transaction_fee(self.backend.rpc(), message),
            super::preflight::read_balances(
                self.backend.rpc(),
                &owner,
                output_asset,
                output_account
            )
        )?;
        Ok((fee, available, output))
    }

    async fn asset_balance(&self, asset: AssetId, token_account: &Pubkey) -> Result<u64> {
        if matches!(asset, AssetId::NativeSol) {
            return self
                .backend
                .rpc()
                .get_balance(&self.signer.pubkey())
                .await
                .map_err(|error| {
                    CopyTraderError::Execution(format!("failed to read SOL balance: {error}"))
                });
        }
        self.backend
            .rpc()
            .get_token_account_balance(token_account)
            .await
            .map_err(|error| {
                CopyTraderError::Execution(format!("failed to read token balance: {error}"))
            })?
            .amount
            .parse::<u64>()
            .map_err(|error| CopyTraderError::Execution(format!("invalid token balance: {error}")))
    }

    async fn asset_balance_or_zero(&self, asset: AssetId, token_account: &Pubkey) -> Result<u64> {
        match self.asset_balance(asset, token_account).await {
            Ok(balance) => Ok(balance),
            Err(_) => Ok(0),
        }
    }
}

struct InitialReads {
    input_info: MintInfo,
    output_info: MintInfo,
    native_balance: Option<u64>,
}

struct CopyTimings {
    received_at: Instant,
    database: DatabaseTimings,
    active: Option<(&'static str, Instant)>,
    values: std::collections::BTreeMap<&'static str, u64>,
}

impl CopyTimings {
    fn new(received_at: Instant) -> Self {
        let mut timings = Self {
            received_at,
            database: DatabaseTimings::default(),
            active: None,
            values: Default::default(),
        };
        timings.since_receipt("ingestion_queue_ms");
        timings.stage("preparation_ms");
        timings
    }
    fn json(&self) -> Result<String> {
        let mut values = serde_json::to_value(&self.values)?;
        let database = self.database.snapshot();
        let total_us = database.values().fold(0_u64, |total, timing| {
            total.saturating_add(timing.elapsed_us)
        });
        values["database"] = serde_json::to_value(database)?;
        values["db_total_us"] = total_us.into();
        if let Some(before_send) = self.values.get("db_pre_send_us") {
            values["db_post_send_us"] = total_us.saturating_sub(*before_send).into();
        }
        serde_json::to_string(&values).map_err(Into::into)
    }

    fn millis(duration: Duration) -> u64 {
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    }
    fn since_receipt(&mut self, name: &'static str) {
        self.values
            .insert(name, Self::millis(self.received_at.elapsed()));
    }
    fn stage(&mut self, name: &'static str) {
        self.finish();
        self.active = Some((name, Instant::now()));
    }
    fn finish(&mut self) {
        if let Some((name, started)) = self.active.take() {
            self.values.insert(name, Self::millis(started.elapsed()));
        }
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    #[tokio::test]
    async fn initial_reads_overlap_and_do_not_wait_for_slow_or_failed_source() {
        use crate::{
            config::{HttpConfig, MainnetConfig},
            domain::{DexKind, SourcePool},
            http::HttpTransport,
            mainnet::MainnetClient,
            test_rpc::{TestRpc, mint_account},
        };
        use serde_json::json;
        for scenario in [
            "native",
            "token",
            "slow_source",
            "bad_source",
            "mint_failure",
            "balance_failure",
        ] {
            let server = TestRpc::start(move |request| {
                let value = match request["method"].as_str().unwrap() {
                    "getVersion" => return json!({"solana-core":"3.1.0","feature-set":1}),
                    "getMultipleAccounts" if scenario == "mint_failure" => {
                        return json!({"error":{"code":-32602,"message":"mint failure"}});
                    }
                    "getMultipleAccounts" => json!([mint_account(), mint_account()]),
                    "getBalance" if scenario == "balance_failure" => {
                        return json!({"error":{"code":-32602,"message":"balance failure"}});
                    }
                    "getBalance" => json!(1000000),
                    "getAccountInfo" => {
                        assert_eq!(request["params"][1]["minContextSlot"], 42);
                        if scenario == "bad_source" {
                            json!(null)
                        } else {
                            mint_account()
                        }
                    }
                    other => panic!("unexpected RPC {other}"),
                };
                let delay = if scenario == "slow_source" && request["method"] == "getAccountInfo" {
                    1000
                } else {
                    75
                };
                json!({"test_delay_ms":delay,"test_result":{"context":{"slot":42},"value":value}})
            })
            .await;
            let transport = HttpTransport::new(&HttpConfig::default()).unwrap();
            let config =
                Arc::new(AppConfig::load(std::path::Path::new("config.example.toml")).unwrap());
            let backend = Arc::new(ExecutionBackend::Mainnet(Arc::new(MainnetClient::new(
                server.url.clone(),
                &MainnetConfig {
                    source_direct: false,
                    fixed_priority_fee_micro_lamports: None,
                    sender_url: server.url.clone(),
                    tip_lamports: 5000,
                    priority_level: "High".to_owned(),
                    max_priority_fee_micro_lamports: 100,
                },
                transport.clone(),
            ))));
            let worker = ExecutionWorker::new(
                config.clone(),
                Arc::new(Keypair::new()),
                Store::connect("sqlite::memory:").await.unwrap(),
                backend,
                TokenSafetyClient::new(&server.url, &transport),
                TransactionDecoder::new(config.signal.wallet),
                Arc::new(Router::supported(
                    Arc::new(transport.solana_rpc(&server.url)),
                    4,
                    Duration::from_secs(2),
                )),
            );
            let intent = TradeIntent {
                source_pool: Some(SourcePool {
                    dex: DexKind::PumpSwap,
                    address: Pubkey::new_unique(),
                }),
                source_signature: solana_sdk::signature::Signature::default(),
                slot: 42,
                input_asset: if scenario == "token" {
                    AssetId::Token(Pubkey::new_unique())
                } else {
                    AssetId::NativeSol
                },
                output_asset: AssetId::Token(Pubkey::new_unique()),
                source_input_amount: 100,
                source_output_amount: 100,
            };
            let stages = crate::routing::RouteStages::default();
            let source = worker.router.prefetch_source(&intent, &stages);
            let result = tokio::time::timeout(
                Duration::from_millis(700),
                worker.initial_reads(&intent, source.as_ref()),
            )
            .await
            .expect("initial reads stalled on source");
            assert_eq!(
                result.is_err(),
                matches!(scenario, "mint_failure" | "balance_failure"),
                "{scenario}"
            );
            if let Ok(reads) = result {
                assert_eq!(
                    reads.native_balance,
                    if scenario == "token" {
                        None
                    } else {
                        Some(1000000)
                    }
                );
                assert!(
                    server
                        .peak_in_flight
                        .load(std::sync::atomic::Ordering::SeqCst)
                        >= if scenario == "token" { 2 } else { 3 }
                );
                assert_eq!(
                    server.count("getBalance"),
                    if scenario == "token" { 0 } else { 1 }
                );
            }
            if scenario == "slow_source" {
                assert!(!stages.snapshot().contains_key("source_pool_read_ms"));
            }
            drop(source);
            assert_eq!(server.count("sendTransaction"), 0);
        }
    }

    #[tokio::test]
    async fn post_route_reads_overlap_and_preserve_fee_errors() {
        use crate::{
            config::{HttpConfig, MainnetConfig},
            http::HttpTransport,
            mainnet::MainnetClient,
            test_rpc::TestRpc,
        };
        use serde_json::json;
        for fail_fee in [false, true] {
            let server = TestRpc::start(move |request| {
                let value = match request["method"].as_str().expect("method") {
                    "getVersion" => return json!({"solana-core":"3.1.0","feature-set":1}),
                    "getFeeForMessage" if fail_fee => return json!({"error":{"code":-32602,"message":"fee unavailable"}}),
                    "getFeeForMessage" => json!(5000),
                    "getBalance" => json!(1000000),
                    "getTokenAccountBalance" => json!({"amount":"123","decimals":6,"uiAmount":0.000123,"uiAmountString":"0.000123"}),
                    other => panic!("unexpected method {other}"),
                };
                json!({"test_delay_ms":75,"test_result":{"context":{"slot":42},"value":value}})
            }).await;
            let transport = HttpTransport::new(&HttpConfig::default()).expect("transport");
            let mainnet_config = MainnetConfig {
                source_direct: false,
                fixed_priority_fee_micro_lamports: None,
                sender_url: server.url.clone(),
                tip_lamports: 5000,
                priority_level: "High".to_owned(),
                max_priority_fee_micro_lamports: 100,
            };
            let backend = Arc::new(ExecutionBackend::Mainnet(Arc::new(MainnetClient::new(
                server.url.clone(),
                &mainnet_config,
                transport.clone(),
            ))));
            let config = Arc::new(
                AppConfig::load(std::path::Path::new("config.example.toml")).expect("config"),
            );
            let worker = ExecutionWorker::new(
                config.clone(),
                Arc::new(Keypair::new()),
                Store::connect("sqlite::memory:").await.expect("store"),
                backend,
                TokenSafetyClient::new(&server.url, &transport),
                TransactionDecoder::new(config.signal.wallet),
                Arc::new(Router::supported(
                    Arc::new(transport.solana_rpc(&server.url)),
                    4,
                    Duration::from_secs(2),
                )),
            );
            let message = solana_sdk::message::Message::default();
            let mint = Pubkey::new_unique();
            let account = Pubkey::new_unique();
            let started = Instant::now();
            let result = worker
                .post_route_reads(&message, AssetId::Token(mint), &account)
                .await;
            let parallel = started.elapsed();
            assert_eq!(result.is_err(), fail_fee);
            assert_eq!(server.count("sendTransaction"), 0);
            if !fail_fee {
                assert_eq!(result.expect("reads"), (5000, 1000000, 123));
                assert_eq!(
                    server
                        .peak_in_flight
                        .load(std::sync::atomic::Ordering::SeqCst),
                    3
                );
                let started = Instant::now();
                worker
                    .backend
                    .rpc()
                    .get_fee_for_message(&message)
                    .await
                    .expect("fee");
                worker
                    .backend
                    .rpc()
                    .get_balance(&worker.signer.pubkey())
                    .await
                    .expect("SOL");
                worker
                    .asset_balance_or_zero(AssetId::Token(mint), &account)
                    .await
                    .expect("output");
                let sequential = started.elapsed();
                assert!(
                    parallel < sequential,
                    "parallel={parallel:?}, sequential={sequential:?}"
                );
                eprintln!(
                    "controlled 75ms RPC fixture: sequential post-route={sequential:?}, concurrent={parallel:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn queued_observation_keeps_ingestion_metrics_through_a_skip() {
        use crate::{
            config::HttpConfig,
            domain::{SignalOrigin, TransactionMeta},
            http::HttpTransport,
            surfpool::SurfpoolClient,
        };
        let base = Store::connect("sqlite::memory:").await.expect("store");
        let database_timings = DatabaseTimings::default();
        let observed = ObservedTransaction {
            signature: solana_sdk::signature::Signature::default(),
            slot: 42,
            block_time: Some(0),
            origin: SignalOrigin::Live,
            transaction: solana_sdk::transaction::VersionedTransaction::default(),
            meta: TransactionMeta::default(),
            raw_payload: "{}".to_owned(),
            received_bytes: 2,
        };
        base.with_timings(database_timings.clone())
            .record_observation(&observed)
            .await
            .expect("observation");
        let config =
            Arc::new(AppConfig::load(std::path::Path::new("config.example.toml")).expect("config"));
        let endpoint = url::Url::parse("http://127.0.0.1:1").expect("unused endpoint");
        let transport = HttpTransport::new(&HttpConfig::default()).expect("transport");
        let backend = Arc::new(ExecutionBackend::Surfpool(Arc::new(SurfpoolClient::new(
            endpoint.clone(),
            transport.clone(),
        ))));
        let worker = ExecutionWorker::new(
            config.clone(),
            Arc::new(Keypair::new()),
            base.clone(),
            backend,
            TokenSafetyClient::new(&endpoint, &transport),
            TransactionDecoder::new(config.signal.wallet),
            Arc::new(Router::supported(
                Arc::new(transport.solana_rpc(&endpoint)),
                4,
                Duration::from_secs(2),
            )),
        );
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .send(QueuedObservation {
                observed,
                received_at: Instant::now() - Duration::from_millis(10),
                database_timings: database_timings.clone(),
            })
            .await
            .expect("enqueue");
        drop(sender);
        worker.run(receiver).await.expect("worker");
        let rows = base.status(1).await.expect("rows");
        assert_eq!(rows[0].source_status, "skipped");
        let saved: serde_json::Value =
            serde_json::from_str(rows[0].timings_json.as_deref().expect("timings")).expect("JSON");
        assert_eq!(saved["database"]["record_observation"]["calls"], 1);
        assert_eq!(saved["database"]["mark_skipped"]["calls"], 1);
        assert!(saved["database"].get("record_timings").is_none());
        assert!(!database_timings.snapshot().contains_key("record_timings"));
        assert!(saved["ingestion_queue_ms"].as_u64().expect("delay") >= 10);
    }

    #[tokio::test]
    async fn timing_write_is_measured_in_log_snapshot_without_a_second_write() {
        let base = Store::connect("sqlite::memory:").await.expect("store");
        base.record_recovered_signature("source", 42)
            .await
            .expect("source");
        let timings = CopyTimings::new(Instant::now());
        let store = base.with_timings(timings.database.clone());
        store
            .mark_skipped("source", SkipReason::StaleSignal)
            .await
            .expect("skip");
        let saved = timings.json().expect("JSON");
        store
            .record_timings("source", &saved)
            .await
            .expect("timing write");
        let log: serde_json::Value =
            serde_json::from_str(&timings.json().expect("JSON")).expect("log");
        assert_eq!(log["database"]["record_timings"]["calls"], 1);
        assert!(log["database"]["mark_skipped"]["elapsed_us"].is_u64());
        assert!(log.get("db_pre_send_us").is_none());
        assert!(log.get("db_post_send_us").is_none());
        let rows = base.status(1).await.expect("rows");
        assert_eq!(rows[0].timings_json.as_deref(), Some(saved.as_str()));
        let stored: serde_json::Value = serde_json::from_str(&saved).expect("stored");
        assert!(stored["database"].get("record_timings").is_none());
    }

    #[test]
    fn timings_include_queue_and_failed_stage_but_omit_unreached_stages() {
        let mut timings = CopyTimings::new(Instant::now() - Duration::from_millis(50));
        timings.stage("route_ms");
        timings.finish();
        assert!(timings.values["ingestion_queue_ms"] >= 50);
        assert!(timings.values.contains_key("preparation_ms"));
        assert!(timings.values.contains_key("route_ms"));
        assert!(!timings.values.contains_key("confirmation_ms"));
        assert!(!timings.values.contains_key("receipt_to_send_start_ms"));
        timings.since_receipt("receipt_to_send_start_ms");
        timings.since_receipt("receipt_to_send_response_ms");
        assert!(
            timings.values["receipt_to_send_response_ms"]
                >= timings.values["receipt_to_send_start_ms"]
        );
        let before = timings.values.clone();
        timings.finish();
        assert_eq!(timings.values, before);
    }
}
