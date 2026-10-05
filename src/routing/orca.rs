use async_trait::async_trait;
use orca_whirlpools::{SwapConfig, SwapQuote, SwapType, swap_instructions};
use orca_whirlpools_client::Whirlpool;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::pubkey::Pubkey;

use crate::{
    domain::{DexKind, PreparedRoute, SizedTrade},
    error::{CopyTraderError, Result},
    routing::{PoolDescriptor, RouteContext, RouteDexAdapter, program_accounts_config},
};

const PROGRAM_ID: Pubkey = DexKind::OrcaWhirlpool.program_id();
const MINT_A_OFFSET: usize = 101;
const MINT_B_OFFSET: usize = 181;

pub struct OrcaAdapter;

#[async_trait]
impl RouteDexAdapter for OrcaAdapter {
    fn kind(&self) -> DexKind {
        DexKind::OrcaWhirlpool
    }

    async fn discover(&self, rpc: &RpcClient, mints: &[Pubkey]) -> Result<Vec<PoolDescriptor>> {
        let mut pools = Vec::new();
        for (index, mint_a) in mints.iter().enumerate() {
            for mint_b in mints.iter().skip(index + 1) {
                for (left, right) in [(*mint_a, *mint_b), (*mint_b, *mint_a)] {
                    let config = program_accounts_config(vec![
                        RpcFilterType::DataSize(Whirlpool::LEN as u64),
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            MINT_A_OFFSET,
                            left.as_ref(),
                        )),
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            MINT_B_OFFSET,
                            right.as_ref(),
                        )),
                    ]);
                    #[allow(deprecated)]
                    let accounts = rpc
                        .get_program_accounts_with_config(&PROGRAM_ID, config)
                        .await
                        .map_err(|error| {
                            CopyTraderError::Execution(format!(
                                "Orca pool discovery failed: {error}"
                            ))
                        })?;
                    for (address, account) in accounts {
                        let pool = Whirlpool::from_bytes(&account.data).map_err(|error| {
                            CopyTraderError::Decode(format!("invalid Orca pool {address}: {error}"))
                        })?;
                        pools.push(PoolDescriptor {
                            dex: DexKind::OrcaWhirlpool,
                            address,
                            mint_a: pool.token_mint_a,
                            mint_b: pool.token_mint_b,
                            liquidity_hint: pool.liquidity,
                        });
                    }
                }
            }
        }
        pools.sort_unstable_by_key(|pool| pool.address);
        pools.dedup_by_key(|pool| pool.address);
        Ok(pools)
    }

    async fn prepare(
        &self,
        rpc: &RpcClient,
        pool: &PoolDescriptor,
        trade: &SizedTrade,
        context: &RouteContext,
        pool_account: Option<&solana_sdk::account::Account>,
    ) -> Result<PreparedRoute> {
        let fetched;
        let account = if let Some(account) = pool_account {
            account
        } else {
            fetched = super::source_pool::fetch_account(
                rpc,
                crate::domain::SourcePool {
                    dex: DexKind::OrcaWhirlpool,
                    address: pool.address,
                },
                trade.intent.slot,
            )
            .await?;
            &fetched
        };
        if account.owner != PROGRAM_ID
            || account.executable
            || account.data.len() != Whirlpool::LEN
            || account.data.get(..8) != Some(&[63, 149, 209, 12, 225, 128, 99, 9])
        {
            return Err(CopyTraderError::Decode(
                "invalid Orca pool account".to_owned(),
            ));
        }
        let state = Whirlpool::from_bytes(&account.data)
            .map_err(|e| CopyTraderError::Decode(format!("invalid Orca pool: {e}")))?;
        let input = trade.intent.input_asset.routing_mint();
        let output = trade.intent.output_asset.routing_mint();
        if input == output
            || !((input == state.token_mint_a && output == state.token_mint_b)
                || (input == state.token_mint_b && output == state.token_mint_a))
        {
            return Err(CopyTraderError::Decode(
                "Orca pool does not match trade pair".to_owned(),
            ));
        }
        if let Some(outputs) = context.source_outputs {
            return super::orca_direct::prepare(rpc, pool, trade, context, &state, outputs).await;
        }
        let snapshot = super::orca_snapshot::quote_client(
            rpc,
            pool.address,
            account,
            &state,
            context.copier,
            trade.intent.slot,
        )
        .await?;
        let result = swap_instructions(
            &snapshot,
            pool.address,
            trade.input_amount,
            trade.intent.input_asset.routing_mint(),
            SwapType::ExactIn,
            SwapConfig {
                slippage_tolerance_bps: Some(context.slippage_bps),
                signer: Some(context.copier),
                whirlpool_deployment: None,
            },
        )
        .await
        .map_err(|error| CopyTraderError::Execution(format!("Orca quote failed: {error}")))?;
        let SwapQuote::ExactIn(quote) = result.quote else {
            return Err(CopyTraderError::Execution(
                "Orca returned an exact-output quote".to_owned(),
            ));
        };
        Ok(PreparedRoute {
            dex: DexKind::OrcaWhirlpool,
            pool: pool.address,
            instructions: result.instructions,
            additional_signers: result.additional_signers,
            market_accounts: vec![pool.address],
            expected_output: quote.token_est_out,
            minimum_output: quote.token_min_out,
            compute_unit_limit: 500_000,
        })
    }
}
