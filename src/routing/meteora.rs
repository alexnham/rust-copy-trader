use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use meteora_dlmm::{
    constants::{BINS_PER_ARRAY, OFF_ACTIVE_ID, OFF_BA_INDEX, OFF_TOKEN_X_MINT, OFF_TOKEN_Y_MINT},
    decode::PoolState,
    quote_with_mints,
    token2022::parse_mint,
};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
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

const PROGRAM_ID: Pubkey = DexKind::MeteoraDlmm.program_id();
const LB_PAIR_LEN: u64 = 904;
const BIN_ARRAY_LEN: u64 = 10_136;
const RESERVE_X_OFFSET: usize = 152;
const RESERVE_Y_OFFSET: usize = 184;
const ORACLE_OFFSET: usize = 552;
const SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

pub struct MeteoraAdapter;

#[async_trait]
impl RouteDexAdapter for MeteoraAdapter {
    fn kind(&self) -> DexKind {
        DexKind::MeteoraDlmm
    }

    async fn discover(&self, rpc: &RpcClient, mints: &[Pubkey]) -> Result<Vec<PoolDescriptor>> {
        let mut result = Vec::new();
        for (index, mint_a) in mints.iter().enumerate() {
            for mint_b in mints.iter().skip(index + 1) {
                for (mint_x, mint_y) in [(*mint_a, *mint_b), (*mint_b, *mint_a)] {
                    let config = program_accounts_config(vec![
                        RpcFilterType::DataSize(LB_PAIR_LEN),
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            OFF_TOKEN_X_MINT,
                            mint_x.as_ref(),
                        )),
                        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                            OFF_TOKEN_Y_MINT,
                            mint_y.as_ref(),
                        )),
                    ]);
                    #[allow(deprecated)]
                    let pools = rpc
                        .get_program_accounts_with_config(&PROGRAM_ID, config)
                        .await
                        .map_err(|error| {
                            CopyTraderError::Execution(format!(
                                "Meteora DLMM discovery failed: {error}"
                            ))
                        })?;
                    for (address, account) in pools {
                        result.push(PoolDescriptor {
                            dex: DexKind::MeteoraDlmm,
                            address,
                            mint_a: pubkey_at(&account.data, OFF_TOKEN_X_MINT)?,
                            mint_b: pubkey_at(&account.data, OFF_TOKEN_Y_MINT)?,
                            liquidity_hint: 0,
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
        let pair_account = if let Some(account) = pool_account {
            account
        } else {
            fetched = rpc.get_account(&pool.address).await.map_err(|error| {
                CopyTraderError::Execution(format!("cannot fetch pool: {error}"))
            })?;
            &fetched
        };
        let pair = PairKeys::decode(&pair_account.data)?;
        let input_mint = trade.intent.input_asset.routing_mint();
        let output_mint = trade.intent.output_asset.routing_mint();
        let swap_for_y = input_mint == pair.mint_x && output_mint == pair.mint_y;
        if !swap_for_y && !(input_mint == pair.mint_y && output_mint == pair.mint_x) {
            return Err(CopyTraderError::Unsupported(
                "Meteora DLMM pool does not match trade direction".to_owned(),
            ));
        }

        let (mint_x_account, mint_y_account, mut arrays) = tokio::try_join!(
            rpc.get_account(&pair.mint_x),
            rpc.get_account(&pair.mint_y),
            fetch_bin_arrays(rpc, &pool.address),
        )
        .map_err(|error| {
            CopyTraderError::Execution(format!("cannot fetch Meteora quote state: {error}"))
        })?;
        let mint_x = parse_mint(&mint_x_account.data, &mint_x_account.owner.to_bytes()).map_err(
            |unsupported| {
                CopyTraderError::Unsupported(format!(
                    "Meteora token X has unsupported extension {:?}",
                    unsupported.extension
                ))
            },
        )?;
        let mint_y = parse_mint(&mint_y_account.data, &mint_y_account.owner.to_bytes()).map_err(
            |unsupported| {
                CopyTraderError::Unsupported(format!(
                    "Meteora token Y has unsupported extension {:?}",
                    unsupported.extension
                ))
            },
        )?;
        let active_array =
            i64::from(i32_at(&pair_account.data, OFF_ACTIVE_ID)?).div_euclid(BINS_PER_ARRAY);
        arrays.sort_unstable_by_key(|array| array.index);
        arrays.retain(|array| {
            if swap_for_y {
                array.index <= active_array
            } else {
                array.index >= active_array
            }
        });
        if swap_for_y {
            arrays.reverse();
        }
        arrays.truncate(4);
        if arrays.is_empty() {
            return Err(CopyTraderError::Unsupported(
                "Meteora has no initialized bin arrays in the swap direction".to_owned(),
            ));
        }
        let (expected_output, minimum_output) = if let Some(outputs) = context.source_outputs {
            outputs
        } else {
            let array_data = arrays
                .iter()
                .map(|array| array.data.clone())
                .collect::<Vec<_>>();
            let mut state = PoolState::from_accounts(
                &pair_account.data,
                &array_data,
                mint_x.decimals,
                mint_y.decimals,
                Some(&pool.address.to_bytes()),
                false,
            )
            .map_err(|error| {
                CopyTraderError::Decode(format!("invalid Meteora state: {error:?}"))
            })?;
            let last_array = arrays.last().map(|array| array.index).ok_or_else(|| {
                CopyTraderError::Unsupported(
                    "Meteora has no initialized bin arrays in the swap direction".to_owned(),
                )
            })?;
            // Missing accounts between active_id and the selected arrays are empty,
            // but the quote engine still needs to know that the gap was inspected.
            state
                .loaded_arrays
                .extend(active_array.min(last_array)..=active_array.max(last_array));
            let timestamp = i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| {
                        CopyTraderError::Execution(format!("invalid system time: {error}"))
                    })?
                    .as_secs(),
            )
            .map_err(|_| CopyTraderError::Execution("system time exceeds i64".to_owned()))?;
            let quote = quote_with_mints(
                &state,
                u128::from(trade.input_amount),
                swap_for_y,
                &mint_x,
                &mint_y,
                timestamp,
                true,
                true,
            )
            .map_err(|error| {
                CopyTraderError::Execution(format!("Meteora quote failed: {error:?}"))
            })?;
            if !quote.complete || quote.remaining_in != 0 {
                return Err(CopyTraderError::Unsupported(
                    "Meteora bin window cannot fill exact input".to_owned(),
                ));
            }
            let expected_output = u64::try_from(quote.amount_out)
                .map_err(|_| CopyTraderError::Execution("Meteora output exceeds u64".to_owned()))?;
            let minimum_output = apply_slippage(expected_output, context.slippage_bps)?;
            (expected_output, minimum_output)
        };

        let (input_program, output_program) = if swap_for_y {
            (mint_x_account.owner, mint_y_account.owner)
        } else {
            (mint_y_account.owner, mint_x_account.owner)
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
        let event_authority = Pubkey::find_program_address(&[b"__event_authority"], &PROGRAM_ID).0;
        let mut accounts = vec![
            AccountMeta::new(pool.address, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new(pair.reserve_x, false),
            AccountMeta::new(pair.reserve_y, false),
            AccountMeta::new(input_account, false),
            AccountMeta::new(output_account, false),
            AccountMeta::new_readonly(pair.mint_x, false),
            AccountMeta::new_readonly(pair.mint_y, false),
            AccountMeta::new(pair.oracle, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(context.copier, true),
            AccountMeta::new_readonly(mint_x_account.owner, false),
            AccountMeta::new_readonly(mint_y_account.owner, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
        ];
        accounts.extend(
            arrays
                .iter()
                .map(|array| AccountMeta::new(array.address, false)),
        );
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&SWAP);
        data.extend_from_slice(&trade.input_amount.to_le_bytes());
        data.extend_from_slice(&minimum_output.to_le_bytes());
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
        let mut market_accounts = vec![pool.address, pair.reserve_x, pair.reserve_y, pair.oracle];
        market_accounts.extend(arrays.into_iter().map(|array| array.address));

        Ok(PreparedRoute {
            dex: DexKind::MeteoraDlmm,
            pool: pool.address,
            instructions,
            additional_signers: signers,
            market_accounts,
            expected_output,
            minimum_output,
            compute_unit_limit: 450_000,
        })
    }
}

struct PairKeys {
    mint_x: Pubkey,
    mint_y: Pubkey,
    reserve_x: Pubkey,
    reserve_y: Pubkey,
    oracle: Pubkey,
}

impl PairKeys {
    fn decode(data: &[u8]) -> Result<Self> {
        if data.len() != LB_PAIR_LEN as usize
            || data.get(..8) != Some(&[33, 11, 49, 98, 181, 101, 177, 13])
        {
            return Err(CopyTraderError::Decode(
                "invalid Meteora DLMM pair account".to_owned(),
            ));
        }
        Ok(Self {
            mint_x: pubkey_at(data, OFF_TOKEN_X_MINT)?,
            mint_y: pubkey_at(data, OFF_TOKEN_Y_MINT)?,
            reserve_x: pubkey_at(data, RESERVE_X_OFFSET)?,
            reserve_y: pubkey_at(data, RESERVE_Y_OFFSET)?,
            oracle: pubkey_at(data, ORACLE_OFFSET)?,
        })
    }
}

struct BinArrayAccount {
    address: Pubkey,
    index: i64,
    data: Vec<u8>,
}

async fn fetch_bin_arrays(
    rpc: &RpcClient,
    pool: &Pubkey,
) -> std::result::Result<Vec<BinArrayAccount>, solana_client::client_error::ClientError> {
    let config = program_accounts_config(vec![
        RpcFilterType::DataSize(BIN_ARRAY_LEN),
        RpcFilterType::Memcmp(Memcmp::new_base58_encoded(24, pool.as_ref())),
    ]);
    #[allow(deprecated)]
    let accounts = rpc
        .get_program_accounts_with_config(&PROGRAM_ID, config)
        .await?;
    Ok(accounts
        .into_iter()
        .filter_map(|(address, account)| {
            let index = i64_at(&account.data, OFF_BA_INDEX).ok()?;
            Some(BinArrayAccount {
                address,
                index,
                data: account.data,
            })
        })
        .collect())
}

fn pubkey_at(data: &[u8], offset: usize) -> Result<Pubkey> {
    let bytes = data
        .get(offset..offset + 32)
        .ok_or_else(|| CopyTraderError::Decode("truncated Meteora account".to_owned()))?;
    Pubkey::try_from(bytes)
        .map_err(|_| CopyTraderError::Decode("invalid Meteora pubkey".to_owned()))
}

fn i32_at(data: &[u8], offset: usize) -> Result<i32> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| CopyTraderError::Decode("truncated Meteora i32".to_owned()))?
        .try_into()
        .map_err(|_| CopyTraderError::Decode("invalid Meteora i32".to_owned()))?;
    Ok(i32::from_le_bytes(bytes))
}

fn i64_at(data: &[u8], offset: usize) -> Result<i64> {
    let bytes: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or_else(|| CopyTraderError::Decode("truncated Meteora i64".to_owned()))?
        .try_into()
        .map_err(|_| CopyTraderError::Decode("invalid Meteora i64".to_owned()))?;
    Ok(i64::from_le_bytes(bytes))
}
