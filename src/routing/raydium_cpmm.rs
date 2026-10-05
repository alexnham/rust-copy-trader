use async_trait::async_trait;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use solana_system_interface::instruction::create_account;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::{
    domain::{AssetId, DexKind, PreparedRoute, SizedTrade},
    error::{CopyTraderError, Result},
    routing::{PoolDescriptor, RouteContext, RouteDexAdapter, program_accounts_config},
    token::accounts::associated_token_address,
};

const PROGRAM_ID: Pubkey = DexKind::RaydiumCpmm.program_id();
const POOL_LEN: u64 = 637;
const MINT_0_OFFSET: usize = 168;
const MINT_1_OFFSET: usize = 200;
const SWAP_BASE_INPUT: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];
const FEE_DENOMINATOR: u128 = 1_000_000;

pub struct RaydiumCpmmAdapter;

#[async_trait]
impl RouteDexAdapter for RaydiumCpmmAdapter {
    fn kind(&self) -> DexKind {
        DexKind::RaydiumCpmm
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
                                "Raydium CPMM discovery failed: {error}"
                            ))
                        })?;
                    for (address, account) in pools {
                        let state = PoolState::decode(&account.data)?;
                        result.push(PoolDescriptor {
                            dex: DexKind::RaydiumCpmm,
                            address,
                            mint_a: state.mint_0,
                            mint_b: state.mint_1,
                            liquidity_hint: u128::from(state.lp_supply),
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
        if state.status & 4 != 0 {
            return Err(CopyTraderError::Unsupported(
                "Raydium CPMM pool has swaps disabled".to_owned(),
            ));
        }
        let input_mint = trade.intent.input_asset.routing_mint();
        let output_mint = trade.intent.output_asset.routing_mint();
        let zero_for_one = input_mint == state.mint_0 && output_mint == state.mint_1;
        if !zero_for_one && !(input_mint == state.mint_1 && output_mint == state.mint_0) {
            return Err(CopyTraderError::Unsupported(
                "Raydium CPMM pool does not match trade direction".to_owned(),
            ));
        }
        let (input_vault, output_vault, input_program, output_program) = if zero_for_one {
            (
                state.vault_0,
                state.vault_1,
                state.program_0,
                state.program_1,
            )
        } else {
            (
                state.vault_1,
                state.vault_0,
                state.program_1,
                state.program_0,
            )
        };
        let (expected_output, minimum_output) = if let Some(outputs) = context.source_outputs {
            outputs
        } else {
            let config_account = rpc.get_account(&state.amm_config).await.map_err(|error| {
                CopyTraderError::Execution(format!("cannot fetch Raydium CPMM config: {error}"))
            })?;
            let trade_fee_rate = decode_trade_fee(&config_account.data)?;
            let (input_vault_balance, output_vault_balance) = tokio::try_join!(
                token_balance(rpc, &input_vault),
                token_balance(rpc, &output_vault),
            )?;
            let (input_fees, output_fees) = if zero_for_one {
                (state.fees_0, state.fees_1)
            } else {
                (state.fees_1, state.fees_0)
            };
            let input_reserve = input_vault_balance.checked_sub(input_fees).ok_or_else(|| {
                CopyTraderError::Decode("Raydium input vault is below accrued fees".to_owned())
            })?;
            let output_reserve =
                output_vault_balance
                    .checked_sub(output_fees)
                    .ok_or_else(|| {
                        CopyTraderError::Decode(
                            "Raydium output vault is below accrued fees".to_owned(),
                        )
                    })?;
            let expected_output = constant_product_quote(
                trade.input_amount,
                input_reserve,
                output_reserve,
                trade_fee_rate,
            )?;
            let minimum_output = apply_slippage(expected_output, context.slippage_bps)?;
            (expected_output, minimum_output)
        };

        let mut instructions = Vec::new();
        let mut additional_signers = Vec::new();
        let input_account = prepare_user_account(
            rpc,
            &mut instructions,
            &mut additional_signers,
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
            &mut additional_signers,
            context.copier,
            trade.intent.output_asset,
            output_mint,
            output_program,
            0,
        )
        .await?;
        let authority =
            Pubkey::find_program_address(&[b"vault_and_lp_mint_auth_seed"], &PROGRAM_ID).0;
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&SWAP_BASE_INPUT);
        data.extend_from_slice(&trade.input_amount.to_le_bytes());
        data.extend_from_slice(&minimum_output.to_le_bytes());
        instructions.push(Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(context.copier, true),
                AccountMeta::new_readonly(authority, false),
                AccountMeta::new_readonly(state.amm_config, false),
                AccountMeta::new(pool.address, false),
                AccountMeta::new(input_account, false),
                AccountMeta::new(output_account, false),
                AccountMeta::new(input_vault, false),
                AccountMeta::new(output_vault, false),
                AccountMeta::new_readonly(input_program, false),
                AccountMeta::new_readonly(output_program, false),
                AccountMeta::new_readonly(input_mint, false),
                AccountMeta::new_readonly(output_mint, false),
                AccountMeta::new(state.observation, false),
            ],
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

        Ok(PreparedRoute {
            dex: DexKind::RaydiumCpmm,
            pool: pool.address,
            instructions,
            additional_signers,
            market_accounts: vec![
                pool.address,
                state.amm_config,
                input_vault,
                output_vault,
                state.observation,
            ],
            expected_output,
            minimum_output,
            compute_unit_limit: 220_000,
        })
    }
}

#[derive(Clone, Copy)]
struct PoolState {
    amm_config: Pubkey,
    vault_0: Pubkey,
    vault_1: Pubkey,
    mint_0: Pubkey,
    mint_1: Pubkey,
    program_0: Pubkey,
    program_1: Pubkey,
    observation: Pubkey,
    status: u8,
    lp_supply: u64,
    fees_0: u64,
    fees_1: u64,
}

impl PoolState {
    fn decode(data: &[u8]) -> Result<Self> {
        if data.len() != POOL_LEN as usize
            || data.get(..8) != Some(&[247, 237, 227, 245, 215, 195, 222, 70])
        {
            return Err(CopyTraderError::Decode(
                "invalid Raydium CPMM pool account".to_owned(),
            ));
        }
        Ok(Self {
            amm_config: pubkey_at(data, 8)?,
            vault_0: pubkey_at(data, 72)?,
            vault_1: pubkey_at(data, 104)?,
            mint_0: pubkey_at(data, MINT_0_OFFSET)?,
            mint_1: pubkey_at(data, MINT_1_OFFSET)?,
            program_0: pubkey_at(data, 232)?,
            program_1: pubkey_at(data, 264)?,
            observation: pubkey_at(data, 296)?,
            status: data[329],
            lp_supply: u64_at(data, 336)?,
            fees_0: checked_fee_sum(data, &[344, 360, 400])?,
            fees_1: checked_fee_sum(data, &[352, 368, 408])?,
        })
    }
}

fn checked_fee_sum(data: &[u8], offsets: &[usize]) -> Result<u64> {
    offsets.iter().try_fold(0_u64, |total, offset| {
        total
            .checked_add(u64_at(data, *offset)?)
            .ok_or_else(|| CopyTraderError::Decode("Raydium accrued fee overflow".to_owned()))
    })
}

fn decode_trade_fee(data: &[u8]) -> Result<u64> {
    if data.get(..8) != Some(&[218, 244, 33, 104, 203, 203, 43, 111]) {
        return Err(CopyTraderError::Decode(
            "invalid Raydium CPMM config account".to_owned(),
        ));
    }
    u64_at(data, 12)
}

fn pubkey_at(data: &[u8], offset: usize) -> Result<Pubkey> {
    let bytes = data
        .get(offset..offset + 32)
        .ok_or_else(|| CopyTraderError::Decode("truncated Raydium account".to_owned()))?;
    Pubkey::try_from(bytes)
        .map_err(|_| CopyTraderError::Decode("invalid Raydium pubkey".to_owned()))
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or_else(|| CopyTraderError::Decode("truncated Raydium integer".to_owned()))?
        .try_into()
        .map_err(|_| CopyTraderError::Decode("invalid Raydium integer".to_owned()))?;
    Ok(u64::from_le_bytes(bytes))
}

fn constant_product_quote(
    amount_in: u64,
    reserve_in: u64,
    reserve_out: u64,
    fee_rate: u64,
) -> Result<u64> {
    let fee_rate = u128::from(fee_rate);
    if fee_rate >= FEE_DENOMINATOR || reserve_in == 0 || reserve_out == 0 {
        return Err(CopyTraderError::Execution(
            "invalid Raydium CPMM fee or reserves".to_owned(),
        ));
    }
    let after_fee = u128::from(amount_in)
        .checked_mul(FEE_DENOMINATOR - fee_rate)
        .and_then(|value| value.checked_div(FEE_DENOMINATOR))
        .ok_or_else(|| CopyTraderError::Execution("CPMM fee math overflow".to_owned()))?;
    let numerator = after_fee
        .checked_mul(u128::from(reserve_out))
        .ok_or_else(|| CopyTraderError::Execution("CPMM quote overflow".to_owned()))?;
    let denominator = u128::from(reserve_in)
        .checked_add(after_fee)
        .ok_or_else(|| CopyTraderError::Execution("CPMM quote overflow".to_owned()))?;
    u64::try_from(numerator / denominator)
        .map_err(|_| CopyTraderError::Execution("CPMM quote exceeds u64".to_owned()))
}

pub(super) fn apply_slippage(amount: u64, slippage_bps: u16) -> Result<u64> {
    let retained = 10_000_u128
        .checked_sub(u128::from(slippage_bps))
        .ok_or_else(|| CopyTraderError::Execution("slippage underflow".to_owned()))?;
    let value = u128::from(amount)
        .checked_mul(retained)
        .and_then(|value| value.checked_div(10_000))
        .ok_or_else(|| CopyTraderError::Execution("slippage math overflow".to_owned()))?;
    u64::try_from(value)
        .map_err(|_| CopyTraderError::Execution("minimum output exceeds u64".to_owned()))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_user_account(
    rpc: &RpcClient,
    instructions: &mut Vec<Instruction>,
    signers: &mut Vec<Keypair>,
    owner: Pubkey,
    asset: AssetId,
    mint: Pubkey,
    token_program: Pubkey,
    native_amount: u64,
) -> Result<Pubkey> {
    if !matches!(asset, AssetId::NativeSol) {
        let account = associated_token_address(&owner, &mint, &token_program);
        instructions.push(create_associated_token_account_idempotent(
            &owner,
            &owner,
            &mint,
            &token_program,
        ));
        return Ok(account);
    }
    if token_program != spl_token::id() || mint != spl_token::native_mint::id() {
        return Err(CopyTraderError::Unsupported(
            "native SOL route does not use the classic WSOL mint".to_owned(),
        ));
    }
    let account = Keypair::new();
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(spl_token::state::Account::LEN)
        .await
        .map_err(|error| CopyTraderError::Execution(format!("cannot fetch WSOL rent: {error}")))?;
    let lamports = rent
        .checked_add(native_amount)
        .ok_or_else(|| CopyTraderError::Execution("WSOL funding overflow".to_owned()))?;
    instructions.push(create_account(
        &owner,
        &account.pubkey(),
        lamports,
        spl_token::state::Account::LEN as u64,
        &spl_token::id(),
    ));
    instructions.push(
        spl_token::instruction::initialize_account3(
            &spl_token::id(),
            &account.pubkey(),
            &mint,
            &owner,
        )
        .map_err(|error| {
            CopyTraderError::Execution(format!("cannot initialize temporary WSOL: {error}"))
        })?,
    );
    if native_amount > 0 {
        instructions.push(
            spl_token::instruction::sync_native(&spl_token::id(), &account.pubkey()).map_err(
                |error| CopyTraderError::Execution(format!("cannot sync temporary WSOL: {error}")),
            )?,
        );
    }
    let address = account.pubkey();
    signers.push(account);
    Ok(address)
}

pub(super) fn append_native_close(
    instructions: &mut Vec<Instruction>,
    asset: AssetId,
    account: Pubkey,
    owner: Pubkey,
) -> Result<()> {
    if matches!(asset, AssetId::NativeSol) {
        instructions.push(
            spl_token::instruction::close_account(&spl_token::id(), &account, &owner, &owner, &[])
                .map_err(|error| {
                    CopyTraderError::Execution(format!("cannot close temporary WSOL: {error}"))
                })?,
        );
    }
    Ok(())
}

async fn token_balance(rpc: &RpcClient, address: &Pubkey) -> Result<u64> {
    let value = rpc
        .get_token_account_balance(address)
        .await
        .map_err(|error| {
            CopyTraderError::Execution(format!("cannot read Raydium vault {address}: {error}"))
        })?;
    value
        .amount
        .parse::<u64>()
        .map_err(|error| CopyTraderError::Decode(format!("invalid Raydium vault balance: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_product_quote_uses_checked_integer_math() {
        assert_eq!(
            constant_product_quote(1_000, 10_000, 20_000, 3_000).ok(),
            Some(1_813)
        );
        assert!(constant_product_quote(1, 0, 1, 0).is_err());
        assert!(constant_product_quote(1, 1, 1, 1_000_000).is_err());
    }
}
