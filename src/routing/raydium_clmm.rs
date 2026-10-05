use async_trait::async_trait;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_clmm_raydium::{InitializedTick, SwapPool, compute_swap_full};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::{
    domain::{DexKind, PreparedRoute, SizedTrade},
    error::{CopyTraderError, Result},
    routing::{PoolDescriptor, RouteContext, RouteDexAdapter, program_accounts_config},
};

use super::raydium_cpmm::{append_native_close, apply_slippage, prepare_user_account};

const PROGRAM_ID: Pubkey = DexKind::RaydiumClmm.program_id();
const TOKEN_2022_PROGRAM: Pubkey =
    solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const MEMO_PROGRAM: Pubkey = solana_sdk::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const POOL_LEN: u64 = 1544;
const TICK_ARRAY_LEN: u64 = 10_240;
const BITMAP_EXTENSION_LEN: u64 = 1_832;
const MINT_0_OFFSET: usize = 73;
const MINT_1_OFFSET: usize = 105;
const SWAP_V2: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];

pub struct RaydiumClmmAdapter;

#[async_trait]
impl RouteDexAdapter for RaydiumClmmAdapter {
    fn kind(&self) -> DexKind {
        DexKind::RaydiumClmm
    }

    async fn discover(&self, rpc: &RpcClient, mints: &[Pubkey]) -> Result<Vec<PoolDescriptor>> {
        let mut result = Vec::new();
        for (index, mint_a) in mints.iter().enumerate() {
            for mint_b in mints.iter().skip(index + 1) {
                for (mint_0, mint_1) in [(*mint_a, *mint_b), (*mint_b, *mint_a)] {
                    let config = program_accounts_config(vec![
                        RpcFilterType::DataSize(POOL_LEN),
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            MINT_0_OFFSET,
                            mint_0.as_ref(),
                        )),
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            MINT_1_OFFSET,
                            mint_1.as_ref(),
                        )),
                    ]);
                    #[allow(deprecated)]
                    let pools = rpc
                        .get_program_accounts_with_config(&PROGRAM_ID, config)
                        .await
                        .map_err(|error| {
                            CopyTraderError::Execution(format!(
                                "Raydium CLMM discovery failed: {error}"
                            ))
                        })?;
                    for (address, account) in pools {
                        let state = PoolState::decode(&account.data)?;
                        result.push(PoolDescriptor {
                            dex: DexKind::RaydiumClmm,
                            address,
                            mint_a: state.mint_0,
                            mint_b: state.mint_1,
                            liquidity_hint: state.liquidity,
                        });
                    }
                }
            }
        }
        result.sort_unstable_by_key(|pool| pool.address);
        result.dedup_by_key(|pool| pool.address);
        Ok(result)
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
        let state = PoolState::decode(&pool_account.data)?;
        if state.status & 16 != 0 {
            return Err(CopyTraderError::Unsupported(
                "Raydium CLMM pool has swaps disabled".to_owned(),
            ));
        }
        let input_mint = trade.intent.input_asset.routing_mint();
        let output_mint = trade.intent.output_asset.routing_mint();
        let zero_for_one = input_mint == state.mint_0 && output_mint == state.mint_1;
        if !zero_for_one && !(input_mint == state.mint_1 && output_mint == state.mint_0) {
            return Err(CopyTraderError::Unsupported(
                "Raydium CLMM pool does not match trade direction".to_owned(),
            ));
        }
        let (config_account, mint_0_account, mint_1_account) = tokio::try_join!(
            rpc.get_account(&state.amm_config),
            rpc.get_account(&state.mint_0),
            rpc.get_account(&state.mint_1),
        )
        .map_err(|error| {
            CopyTraderError::Execution(format!("cannot fetch Raydium CLMM state: {error}"))
        })?;
        let fee_rate = u32_at(&config_account.data, 47)?;
        let (mut tick_arrays, bitmap_extension) = tokio::try_join!(
            fetch_tick_arrays(rpc, &pool.address),
            fetch_bitmap_extension(rpc, &pool.address),
        )
        .map_err(|error| {
            CopyTraderError::Execution(format!("cannot fetch Raydium CLMM tick state: {error}"))
        })?;
        let ticks_per_array = i32::from(state.tick_spacing)
            .checked_mul(60)
            .ok_or_else(|| CopyTraderError::Decode("CLMM tick spacing overflow".to_owned()))?;
        let current_start = state.tick_current.div_euclid(ticks_per_array) * ticks_per_array;
        tick_arrays.sort_unstable_by_key(|array| array.start_tick);
        tick_arrays.retain(|array| {
            if zero_for_one {
                array.start_tick <= current_start
            } else {
                array.start_tick >= current_start
            }
        });
        if zero_for_one {
            tick_arrays.reverse();
        }
        tick_arrays.truncate(4);
        if tick_arrays.is_empty() {
            return Err(CopyTraderError::Unsupported(
                "Raydium has no initialized tick arrays in the swap direction".to_owned(),
            ));
        }
        let (expected_output, minimum_output) = if let Some(outputs) = context.source_outputs {
            outputs
        } else {
            let mut initialized_ticks = tick_arrays
                .iter()
                .flat_map(|array| array.initialized_ticks.iter().copied())
                .collect::<Vec<_>>();
            initialized_ticks.sort_unstable_by_key(|tick| tick.tick);
            initialized_ticks.dedup_by_key(|tick| tick.tick);
            let quote = compute_swap_full(
                &SwapPool {
                    sqrt_price_x64: state.sqrt_price_x64,
                    liquidity: state.liquidity,
                    tick_current: state.tick_current,
                    tick_spacing: state.tick_spacing,
                    fee_rate_pips: fee_rate,
                },
                &initialized_ticks,
                trade.input_amount,
                0,
                true,
                zero_for_one,
            )
            .map_err(|error| {
                CopyTraderError::Execution(format!("Raydium CLMM quote failed: {error:?}"))
            })?;
            if quote.amount_in != trade.input_amount {
                return Err(CopyTraderError::Unsupported(
                    "Raydium tick window cannot fill exact input".to_owned(),
                ));
            }
            let expected_output = quote.amount_out;
            let minimum_output = apply_slippage(expected_output, context.slippage_bps)?;
            (expected_output, minimum_output)
        };
        let (input_vault, output_vault, input_program, output_program) = if zero_for_one {
            (
                state.vault_0,
                state.vault_1,
                mint_0_account.owner,
                mint_1_account.owner,
            )
        } else {
            (
                state.vault_1,
                state.vault_0,
                mint_1_account.owner,
                mint_0_account.owner,
            )
        };
        let mut instructions = Vec::new();
        let mut signers = Vec::new();
        let input_account = prepare_user_account(
            rpc,
            &mut instructions,
            &mut signers,
            context.copier,
            trade.intent.input_asset,
            input_mint,
            input_program,
            trade.input_amount,
        )
        .await?;
        let output_account = prepare_user_account(
            rpc,
            &mut instructions,
            &mut signers,
            context.copier,
            trade.intent.output_asset,
            output_mint,
            output_program,
            0,
        )
        .await?;
        let mut accounts = vec![
            AccountMeta::new_readonly(context.copier, true),
            AccountMeta::new_readonly(state.amm_config, false),
            AccountMeta::new(pool.address, false),
            AccountMeta::new(input_account, false),
            AccountMeta::new(output_account, false),
            AccountMeta::new(input_vault, false),
            AccountMeta::new(output_vault, false),
            AccountMeta::new(state.observation, false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(TOKEN_2022_PROGRAM, false),
            AccountMeta::new_readonly(MEMO_PROGRAM, false),
            AccountMeta::new_readonly(input_mint, false),
            AccountMeta::new_readonly(output_mint, false),
        ];
        if let Some(extension) = bitmap_extension {
            accounts.push(AccountMeta::new(extension, false));
        }
        accounts.extend(
            tick_arrays
                .iter()
                .map(|array| AccountMeta::new(array.address, false)),
        );
        let mut data = Vec::with_capacity(41);
        data.extend_from_slice(&SWAP_V2);
        data.extend_from_slice(&trade.input_amount.to_le_bytes());
        data.extend_from_slice(&minimum_output.to_le_bytes());
        data.extend_from_slice(&0_u128.to_le_bytes());
        data.push(1);
        instructions.push(Instruction {
            program_id: PROGRAM_ID,
            accounts,
            data,
        });
        append_native_close(
            &mut instructions,
            trade.intent.input_asset,
            input_account,
            context.copier,
        )?;
        append_native_close(
            &mut instructions,
            trade.intent.output_asset,
            output_account,
            context.copier,
        )?;
        let mut market_accounts = vec![
            pool.address,
            state.amm_config,
            input_vault,
            output_vault,
            state.observation,
        ];
        if let Some(extension) = bitmap_extension {
            market_accounts.push(extension);
        }
        market_accounts.extend(tick_arrays.into_iter().map(|array| array.address));
        Ok(PreparedRoute {
            dex: DexKind::RaydiumClmm,
            pool: pool.address,
            instructions,
            additional_signers: signers,
            market_accounts,
            expected_output,
            minimum_output,
            compute_unit_limit: 500_000,
        })
    }
}

struct PoolState {
    amm_config: Pubkey,
    mint_0: Pubkey,
    mint_1: Pubkey,
    vault_0: Pubkey,
    vault_1: Pubkey,
    observation: Pubkey,
    tick_spacing: u16,
    liquidity: u128,
    sqrt_price_x64: u128,
    tick_current: i32,
    status: u8,
}

impl PoolState {
    fn decode(data: &[u8]) -> Result<Self> {
        if data.len() != POOL_LEN as usize {
            return Err(CopyTraderError::Decode(
                "invalid Raydium CLMM pool length".to_owned(),
            ));
        }
        Ok(Self {
            amm_config: pubkey_at(data, 9)?,
            mint_0: pubkey_at(data, MINT_0_OFFSET)?,
            mint_1: pubkey_at(data, MINT_1_OFFSET)?,
            vault_0: pubkey_at(data, 137)?,
            vault_1: pubkey_at(data, 169)?,
            observation: pubkey_at(data, 201)?,
            tick_spacing: u16_at(data, 235)?,
            liquidity: u128_at(data, 237)?,
            sqrt_price_x64: u128_at(data, 253)?,
            tick_current: i32_at(data, 269)?,
            status: data[389],
        })
    }
}

struct TickArray {
    address: Pubkey,
    start_tick: i32,
    initialized_ticks: Vec<InitializedTick>,
}

async fn fetch_tick_arrays(
    rpc: &RpcClient,
    pool: &Pubkey,
) -> std::result::Result<Vec<TickArray>, solana_client::client_error::ClientError> {
    let config = program_accounts_config(vec![
        RpcFilterType::DataSize(TICK_ARRAY_LEN),
        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(8, pool.as_ref())),
    ]);
    #[allow(deprecated)]
    let accounts = rpc
        .get_program_accounts_with_config(&PROGRAM_ID, config)
        .await?;
    Ok(accounts
        .into_iter()
        .filter_map(|(address, account)| decode_tick_array(address, &account.data).ok())
        .collect())
}

async fn fetch_bitmap_extension(
    rpc: &RpcClient,
    pool: &Pubkey,
) -> std::result::Result<Option<Pubkey>, solana_client::client_error::ClientError> {
    let config = program_accounts_config(vec![
        RpcFilterType::DataSize(BITMAP_EXTENSION_LEN),
        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(8, pool.as_ref())),
    ]);
    #[allow(deprecated)]
    let accounts = rpc
        .get_program_accounts_with_config(&PROGRAM_ID, config)
        .await?;
    Ok(accounts.first().map(|(address, _)| *address))
}

fn decode_tick_array(address: Pubkey, data: &[u8]) -> Result<TickArray> {
    if data.len() != TICK_ARRAY_LEN as usize {
        return Err(CopyTraderError::Decode(
            "invalid Raydium tick array length".to_owned(),
        ));
    }
    let start_tick = i32_at(data, 40)?;
    let mut initialized_ticks = Vec::new();
    for index in 0..60_usize {
        let offset = 44 + index * 168;
        if u128_at(data, offset + 20)? == 0 {
            continue;
        }
        initialized_ticks.push(InitializedTick {
            tick: i32_at(data, offset)?,
            liquidity_net: i128_at(data, offset + 4)?,
        });
    }
    Ok(TickArray {
        address,
        start_tick,
        initialized_ticks,
    })
}

fn pubkey_at(data: &[u8], offset: usize) -> Result<Pubkey> {
    let bytes = data
        .get(offset..offset + 32)
        .ok_or_else(|| CopyTraderError::Decode("truncated Raydium CLMM account".to_owned()))?;
    Pubkey::try_from(bytes)
        .map_err(|_| CopyTraderError::Decode("invalid Raydium CLMM pubkey".to_owned()))
}

fn fixed_at<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
    data.get(offset..offset + N)
        .ok_or_else(|| CopyTraderError::Decode("truncated Raydium CLMM integer".to_owned()))?
        .try_into()
        .map_err(|_| CopyTraderError::Decode("invalid Raydium CLMM integer".to_owned()))
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(fixed_at(data, offset)?))
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(fixed_at(data, offset)?))
}

fn i32_at(data: &[u8], offset: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(fixed_at(data, offset)?))
}

fn u128_at(data: &[u8], offset: usize) -> Result<u128> {
    Ok(u128::from_le_bytes(fixed_at(data, offset)?))
}

fn i128_at(data: &[u8], offset: usize) -> Result<i128> {
    Ok(i128::from_le_bytes(fixed_at(data, offset)?))
}
