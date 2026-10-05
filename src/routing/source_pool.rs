use solana_account_decoder_client_types::UiAccountEncoding;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcAccountInfoConfig};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{account::Account, pubkey::Pubkey};

use crate::{
    domain::{DexKind, SourcePool},
    error::{CopyTraderError, Result},
    token::extensions::MintInfo,
};

use super::PoolDescriptor;

pub(super) async fn fetch_account(rpc: &RpcClient, hint: SourcePool, slot: u64) -> Result<Account> {
    let response = rpc
        .get_ui_account_with_config(
            &hint.address,
            RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::processed()),
                min_context_slot: Some(slot),
                ..RpcAccountInfoConfig::default()
            },
        )
        .await
        .map_err(|error| {
            CopyTraderError::Execution(format!("cannot fetch source pool: {error}"))
        })?;
    if response.context.slot < slot {
        return Err(CopyTraderError::Execution(
            "source pool response predates the observed slot".to_owned(),
        ));
    }
    let account = response
        .value
        .and_then(|account| account.decode::<Account>())
        .ok_or_else(|| {
            CopyTraderError::Decode("source pool is missing or undecodable".to_owned())
        })?;
    Ok(account)
}

pub(super) fn validate(
    hint: SourcePool,
    account: &Account,
    input: Pubkey,
    output: Pubkey,
    mint_infos: (&MintInfo, &MintInfo),
) -> Result<PoolDescriptor> {
    if account.owner != hint.dex.program_id() || account.executable {
        return Err(CopyTraderError::Decode(
            "invalid source pool owner".to_owned(),
        ));
    }
    let data = &account.data;
    let (mint_a, mint_b) = match hint.dex {
        DexKind::PumpSwap => {
            let pool =
                pump_rust_client::accounts::pump_amm::decode_pool(data).map_err(|error| {
                    CopyTraderError::Decode(format!("invalid source PumpSwap pool: {error}"))
                })?;
            (pool.base_mint, pool.quote_mint)
        }
        dex => {
            let (length, discriminator, offset_a, offset_b) = match dex {
                DexKind::RaydiumCpmm => (637, [247, 237, 227, 245, 215, 195, 222, 70], 168, 200),
                DexKind::RaydiumClmm => (1544, [247, 237, 227, 245, 215, 195, 222, 70], 73, 105),
                DexKind::OrcaWhirlpool => (653, [63, 149, 209, 12, 225, 128, 99, 9], 101, 181),
                DexKind::MeteoraDlmm => (904, [33, 11, 49, 98, 181, 101, 177, 13], 88, 120),
                DexKind::PumpSwap => unreachable!(),
            };
            if data.len() != length || data.get(..8) != Some(discriminator.as_slice()) {
                return Err(CopyTraderError::Decode(
                    "invalid source pool layout".to_owned(),
                ));
            }
            (pubkey_at(data, offset_a)?, pubkey_at(data, offset_b)?)
        }
    };
    if input == output
        || !((mint_a == input && mint_b == output) || (mint_a == output && mint_b == input))
    {
        return Err(CopyTraderError::Unsupported(
            "source pool does not match the observed pair".to_owned(),
        ));
    }
    if hint.dex == DexKind::RaydiumCpmm {
        let (info_a, info_b) = if mint_a == input {
            mint_infos
        } else {
            (mint_infos.1, mint_infos.0)
        };
        if pubkey_at(data, 232)? != info_a.token_program
            || pubkey_at(data, 264)? != info_b.token_program
        {
            return Err(CopyTraderError::Decode(
                "source pool token programs do not match inspected mints".to_owned(),
            ));
        }
    }
    Ok(PoolDescriptor {
        dex: hint.dex,
        address: hint.address,
        mint_a,
        mint_b,
        liquidity_hint: 0,
    })
}

fn pubkey_at(data: &[u8], offset: usize) -> Result<Pubkey> {
    let bytes = data
        .get(offset..)
        .and_then(|tail| tail.get(..32))
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| CopyTraderError::Decode("truncated source pool mint".to_owned()))?;
    Ok(Pubkey::new_from_array(bytes))
}
