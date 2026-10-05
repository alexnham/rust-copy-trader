use async_trait::async_trait;
use pump_rust_client::{
    AmmQuoteSource, PumpSdk, TradeTxWithVenueParams, TradeVenue,
    accounts::{
        decode_fee_config,
        pump_amm::{decode_global_config, decode_pool},
    },
    pda,
};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{account::Account, pubkey::Pubkey};
use spl_token_2022::{
    extension::StateWithExtensionsOwned,
    state::{Account as TokenAccount, Mint},
};

use crate::{
    domain::{DexKind, NATIVE_MINT, PreparedRoute, SizedTrade},
    error::{CopyTraderError, Result},
    routing::{
        PoolDescriptor, RouteContext, RouteDexAdapter,
        program_accounts::{get_program_accounts_v2, program_accounts_config},
    },
    token::accounts::associated_token_address,
};

const PROGRAM_ID: Pubkey = DexKind::PumpSwap.program_id();
const BASE_MINT_OFFSET: usize = 43;
const QUOTE_MINT_OFFSET: usize = 75;

pub struct PumpSwapAdapter;

#[async_trait]
impl RouteDexAdapter for PumpSwapAdapter {
    fn kind(&self) -> DexKind {
        DexKind::PumpSwap
    }

    async fn discover(&self, rpc: &RpcClient, mints: &[Pubkey]) -> Result<Vec<PoolDescriptor>> {
        let mut pools = Vec::new();
        for (index, mint_a) in mints.iter().enumerate() {
            for mint_b in mints.iter().skip(index + 1) {
                for (base_mint, quote_mint) in [(*mint_a, *mint_b), (*mint_b, *mint_a)] {
                    let config = program_accounts_config(vec![
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            BASE_MINT_OFFSET,
                            base_mint.as_ref(),
                        )),
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            QUOTE_MINT_OFFSET,
                            quote_mint.as_ref(),
                        )),
                    ]);
                    let accounts = match get_program_accounts_v2(rpc, &PROGRAM_ID, &config).await {
                        Ok(accounts) => accounts,
                        Err(error) => {
                            if !error.is_method_not_found() {
                                return Err(CopyTraderError::Execution(format!(
                                    "PumpSwap pool discovery failed: {error}"
                                )));
                            }
                            #[allow(deprecated)]
                            rpc.get_program_accounts_with_config(&PROGRAM_ID, config)
                                .await
                                .map_err(|error| {
                                    CopyTraderError::Execution(format!(
                                        "PumpSwap pool discovery failed: {error}"
                                    ))
                                })?
                        }
                    };
                    for (address, account) in accounts {
                        let state = decode_pool(&account.data).map_err(|error| {
                            CopyTraderError::Decode(format!(
                                "invalid PumpSwap pool {address}: {error}"
                            ))
                        })?;
                        pools.push(PoolDescriptor {
                            dex: DexKind::PumpSwap,
                            address,
                            mint_a: state.base_mint,
                            mint_b: state.quote_mint,
                            liquidity_hint: 0,
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
        let pool_account = if let Some(account) = pool_account {
            account
        } else {
            fetched = rpc.get_account(&pool.address).await.map_err(|error| {
                CopyTraderError::Execution(format!("cannot fetch pool: {error}"))
            })?;
            &fetched
        };
        let pool_state = decode_pool(&pool_account.data).map_err(|error| {
            CopyTraderError::Decode(format!("invalid PumpSwap pool {}: {error}", pool.address))
        })?;
        let global_address = pda::pump_amm::global_config().0;
        let fee_address = pda::pump_amm::fee_config().0;
        let base_mint = pool_state.base_mint;
        let quote_mint = pool_state.quote_mint;
        let base_vault = pool_state.pool_base_token_account;
        let quote_vault = pool_state.pool_quote_token_account;
        let native_mint = Pubkey::from_str_const(NATIVE_MINT);
        let mut addresses = vec![
            global_address,
            fee_address,
            base_mint,
            quote_mint,
            base_vault,
            quote_vault,
        ];
        if quote_mint == native_mint {
            addresses.push(associated_token_address(
                &context.copier,
                &quote_mint,
                &spl_token::id(),
            ));
        }
        let accounts = rpc
            .get_multiple_accounts(&addresses)
            .await
            .map_err(|error| {
                CopyTraderError::Execution(format!("cannot fetch PumpSwap state: {error}"))
            })?;
        let global_account = required_account(&accounts, 0, "global config")?;
        let fee_account = required_account(&accounts, 1, "fee config")?;
        let base_mint_account = required_account(&accounts, 2, "base mint")?;
        let quote_mint_account = required_account(&accounts, 3, "quote mint")?;
        let global = decode_global_config(&global_account.data).map_err(|error| {
            CopyTraderError::Decode(format!("invalid PumpSwap global config: {error}"))
        })?;
        let fee_config = decode_fee_config(&fee_account.data).map_err(|error| {
            CopyTraderError::Decode(format!("invalid PumpSwap fee config: {error}"))
        })?;
        if base_vault
            != associated_token_address(&pool.address, &base_mint, &base_mint_account.owner)
            || quote_vault
                != associated_token_address(&pool.address, &quote_mint, &quote_mint_account.owner)
        {
            return Err(CopyTraderError::Decode(
                "PumpSwap vault addresses do not match pool mints".to_owned(),
            ));
        }
        let base_supply = mint_supply(base_mint_account)?;
        mint_supply(quote_mint_account)?;
        let base_balance = token_balance(
            required_account(&accounts, 4, "base vault")?,
            base_mint,
            base_mint_account.owner,
            pool.address,
        )?;
        let quote_balance = token_balance(
            required_account(&accounts, 5, "quote vault")?,
            quote_mint,
            quote_mint_account.owner,
            pool.address,
        )?;

        let input_mint = trade.intent.input_asset.routing_mint();
        let output_mint = trade.intent.output_asset.routing_mint();
        let is_buy = input_mint == quote_mint && output_mint == base_mint;
        let is_sell = input_mint == base_mint && output_mint == quote_mint;
        if !is_buy && !is_sell {
            return Err(CopyTraderError::Unsupported(
                "PumpSwap pool does not match trade direction".to_owned(),
            ));
        }

        // PumpSwap requires the user's canonical quote ATA. To avoid closing a
        // persistent WSOL ATA, only use its official wrap/unwrap sequence when
        // that ATA does not already exist.
        if quote_mint == native_mint {
            if quote_mint_account.owner != spl_token::id() {
                return Err(CopyTraderError::Decode(
                    "invalid WSOL token program".to_owned(),
                ));
            }
            let wsol = accounts.get(6).ok_or_else(|| {
                CopyTraderError::Execution("missing WSOL account response".to_owned())
            })?;
            if wsol.is_some() {
                return Err(CopyTraderError::Unsupported(
                    "PumpSwap native route would close the copier's persistent WSOL ATA".to_owned(),
                ));
            }
        }

        let sdk = PumpSdk::new();
        let (expected_output, minimum_output) = if let Some(outputs) = context.source_outputs {
            outputs
        } else {
            let quote_source = AmmQuoteSource::Pool {
                pool: &pool_state,
                base_reserve: base_balance,
                quote_reserve: quote_balance,
                base_mint_supply: base_supply,
            };
            let quote = if is_buy {
                sdk.buy_quote_amm_sol_in(
                    &global,
                    &fee_config,
                    quote_source,
                    trade.input_amount,
                    context.slippage_bps,
                )
            } else {
                sdk.sell_quote_amm(
                    &global,
                    &fee_config,
                    quote_source,
                    trade.input_amount,
                    context.slippage_bps,
                )
            }
            .map_err(|error| {
                CopyTraderError::Execution(format!("PumpSwap quote failed: {error}"))
            })?;
            (quote.amount, quote.min_out)
        };

        let instructions = sdk
            .trade_tx_instructions_with_venue(TradeTxWithVenueParams {
                mint: base_mint,
                base_token_program: base_mint_account.owner,
                quote_token_program: quote_mint_account.owner,
                user: context.copier,
                is_buy,
                venue: TradeVenue::Amm {
                    pool: pool.address,
                    amm_global: &global,
                    pool_state: &pool_state,
                },
                base_amount: if is_buy {
                    minimum_output
                } else {
                    trade.input_amount
                },
                sol_amount_threshold: if is_buy {
                    trade.input_amount
                } else {
                    minimum_output
                },
            })
            .ok_or_else(|| {
                CopyTraderError::Execution("PumpSwap has no configured fee recipient".to_owned())
            })?;
        let mut market_accounts = vec![
            pool.address,
            global_address,
            fee_address,
            base_mint,
            quote_mint,
            base_vault,
            quote_vault,
        ];
        market_accounts.extend(
            instructions
                .iter()
                .flat_map(|instruction| instruction.accounts.iter())
                .filter(|meta| meta.is_writable && meta.pubkey != context.copier)
                .map(|meta| meta.pubkey),
        );
        market_accounts.sort_unstable();
        market_accounts.dedup();

        Ok(PreparedRoute {
            dex: DexKind::PumpSwap,
            pool: pool.address,
            instructions,
            additional_signers: Vec::new(),
            market_accounts,
            expected_output,
            minimum_output,
            compute_unit_limit: 350_000,
        })
    }
}

fn required_account<'a>(
    accounts: &'a [Option<solana_sdk::account::Account>],
    index: usize,
    label: &str,
) -> Result<&'a solana_sdk::account::Account> {
    accounts
        .get(index)
        .and_then(Option::as_ref)
        .ok_or_else(|| CopyTraderError::Execution(format!("missing PumpSwap {label}")))
}

fn token_balance(
    account: &Account,
    mint: Pubkey,
    program: Pubkey,
    authority: Pubkey,
) -> Result<u64> {
    if account.owner != program || account.executable {
        return Err(CopyTraderError::Decode(
            "invalid PumpSwap vault owner".to_owned(),
        ));
    }
    let state = StateWithExtensionsOwned::<TokenAccount>::unpack(account.data.clone())
        .map_err(|error| CopyTraderError::Decode(format!("invalid PumpSwap vault: {error}")))?;
    if state.base.mint != mint || state.base.owner != authority {
        return Err(CopyTraderError::Decode(
            "invalid PumpSwap vault mint or authority".to_owned(),
        ));
    }
    Ok(state.base.amount)
}

fn mint_supply(account: &Account) -> Result<u64> {
    if (account.owner != spl_token::id() && account.owner != spl_token_2022::id())
        || account.executable
    {
        return Err(CopyTraderError::Decode(
            "invalid PumpSwap mint owner".to_owned(),
        ));
    }
    let state = StateWithExtensionsOwned::<Mint>::unpack(account.data.clone())
        .map_err(|error| CopyTraderError::Decode(format!("invalid PumpSwap mint: {error}")))?;
    Ok(state.base.supply)
}
