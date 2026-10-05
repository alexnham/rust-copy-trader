use orca_whirlpools_client::{
    AccountsType, RemainingAccountsInfo, RemainingAccountsSlice, SwapV2, SwapV2InstructionArgs,
    Whirlpool, get_oracle_address, get_tick_array_address,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey};

use super::{
    PoolDescriptor, RouteContext,
    raydium_cpmm::{append_native_close, prepare_user_account},
};
use crate::{
    domain::{DexKind, PreparedRoute, SizedTrade},
    error::{CopyTraderError, Result},
};

pub(super) async fn prepare(
    rpc: &RpcClient,
    pool: &PoolDescriptor,
    trade: &SizedTrade,
    context: &RouteContext,
    state: &Whirlpool,
    (expected_output, minimum_output): (u64, u64),
) -> Result<PreparedRoute> {
    let mints = rpc
        .get_multiple_accounts(&[state.token_mint_a, state.token_mint_b])
        .await
        .map_err(|e| CopyTraderError::Execution(format!("cannot fetch Orca mint owners: {e}")))?;
    let program = |index: usize| -> Result<Pubkey> {
        let account = mints
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| CopyTraderError::Decode("Orca mint missing".to_owned()))?;
        if account.executable || ![spl_token::id(), spl_token_2022::id()].contains(&account.owner) {
            return Err(CopyTraderError::Decode(
                "invalid Orca mint owner".to_owned(),
            ));
        }
        Ok(account.owner)
    };
    let program_a = program(0)?;
    let program_b = program(1)?;
    let a_to_b = trade.intent.input_asset.routing_mint() == state.token_mint_a;
    let (input_program, output_program) = if a_to_b {
        (program_a, program_b)
    } else {
        (program_b, program_a)
    };
    let mut instructions = Vec::new();
    let mut additional_signers = Vec::new();
    let input = prepare_user_account(
        rpc,
        &mut instructions,
        &mut additional_signers,
        context.copier,
        trade.intent.input_asset,
        trade.intent.input_asset.routing_mint(),
        input_program,
        trade.input_amount,
    )
    .await?;
    let output = prepare_user_account(
        rpc,
        &mut instructions,
        &mut additional_signers,
        context.copier,
        trade.intent.output_asset,
        trade.intent.output_asset.routing_mint(),
        output_program,
        0,
    )
    .await?;
    let (owner_a, owner_b) = if a_to_b {
        (input, output)
    } else {
        (output, input)
    };
    let span = i32::from(state.tick_spacing)
        .checked_mul(88)
        .filter(|v| *v > 0)
        .ok_or_else(|| CopyTraderError::Decode("invalid Orca tick spacing".to_owned()))?;
    let start = state
        .tick_current_index
        .div_euclid(span)
        .checked_mul(span)
        .ok_or_else(|| CopyTraderError::Decode("Orca tick index overflow".to_owned()))?;
    let mut arrays = Vec::new();
    for offset in [0_i32, 1, 2, -1, -2] {
        let index = offset
            .checked_mul(span)
            .and_then(|offset| start.checked_add(offset))
            .ok_or_else(|| CopyTraderError::Decode("Orca tick index overflow".to_owned()))?;
        arrays.push(
            get_tick_array_address(
                &pool.address,
                index,
                Some(DexKind::OrcaWhirlpool.program_id()),
            )
            .map_err(|e| CopyTraderError::Decode(e.to_string()))?
            .0,
        );
    }
    let oracle = get_oracle_address(&pool.address, Some(DexKind::OrcaWhirlpool.program_id()))
        .map_err(|e| CopyTraderError::Decode(e.to_string()))?
        .0;
    instructions.push(
        SwapV2 {
            token_program_a: program_a,
            token_program_b: program_b,
            memo_program: Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"),
            token_authority: context.copier,
            whirlpool: pool.address,
            token_mint_a: state.token_mint_a,
            token_mint_b: state.token_mint_b,
            token_owner_account_a: owner_a,
            token_owner_account_b: owner_b,
            token_vault_a: state.token_vault_a,
            token_vault_b: state.token_vault_b,
            tick_array0: arrays[0],
            tick_array1: arrays[1],
            tick_array2: arrays[2],
            oracle,
        }
        .instruction_with_remaining_accounts(
            SwapV2InstructionArgs {
                amount: trade.input_amount,
                other_amount_threshold: minimum_output,
                sqrt_price_limit: 0,
                amount_specified_is_input: true,
                a_to_b,
                remaining_accounts_info: Some(RemainingAccountsInfo {
                    slices: vec![RemainingAccountsSlice {
                        accounts_type: AccountsType::SupplementalTickArrays,
                        length: 2,
                    }],
                }),
            },
            &[
                AccountMeta::new(arrays[3], false),
                AccountMeta::new(arrays[4], false),
            ],
        ),
    );
    append_native_close(
        &mut instructions,
        trade.intent.input_asset,
        input,
        context.copier,
    )?;
    append_native_close(
        &mut instructions,
        trade.intent.output_asset,
        output,
        context.copier,
    )?;
    Ok(PreparedRoute {
        dex: DexKind::OrcaWhirlpool,
        pool: pool.address,
        instructions,
        additional_signers,
        market_accounts: vec![pool.address],
        expected_output,
        minimum_output,
        compute_unit_limit: 500_000,
    })
}
