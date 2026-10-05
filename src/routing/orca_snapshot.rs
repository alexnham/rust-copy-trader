use std::collections::HashMap;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use orca_whirlpools_client::{
    ORACLE_DISCRIMINATOR, Oracle, TickArray, Whirlpool, get_oracle_address, get_tick_array_address,
};
use serde_json::{Value, json};
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    nonblocking::rpc_client::RpcClient,
    rpc_config::RpcAccountInfoConfig,
    rpc_request::RpcRequest,
};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::{
    rpc_client::RpcClientConfig,
    rpc_sender::{RpcSender, RpcTransportStats},
};
use solana_sdk::{account::Account, pubkey::Pubkey};

use crate::{
    domain::DexKind,
    error::{CopyTraderError, Result},
    token::accounts::associated_token_address,
};

// One attempt's reads only: the SDK retains its quote math and token-account handling.
pub(super) async fn quote_client(
    rpc: &RpcClient,
    address: Pubkey,
    pool_account: &Account,
    pool: &Whirlpool,
    owner: Pubkey,
    slot: u64,
) -> Result<RpcClient> {
    let span = i32::from(pool.tick_spacing)
        .checked_mul(88)
        .filter(|span| *span > 0)
        .ok_or_else(|| CopyTraderError::Decode("invalid Orca tick spacing".to_owned()))?;
    let start = pool
        .tick_current_index
        .div_euclid(span)
        .checked_mul(span)
        .ok_or_else(|| CopyTraderError::Decode("Orca tick index overflow".to_owned()))?;
    let mut addresses = Vec::new();
    for offset in [0_i32, 1, 2, -1, -2] {
        let index = offset
            .checked_mul(span)
            .and_then(|offset| start.checked_add(offset))
            .ok_or_else(|| CopyTraderError::Decode("Orca tick index overflow".to_owned()))?;
        addresses.push(
            get_tick_array_address(&address, index, Some(DexKind::OrcaWhirlpool.program_id()))
                .map_err(|e| CopyTraderError::Decode(e.to_string()))?
                .0,
        );
    }
    addresses.extend([pool.token_mint_a, pool.token_mint_b]);
    let adaptive = pool.tick_spacing != u16::from_le_bytes(pool.fee_tier_index_seed);
    let oracle = get_oracle_address(&address, Some(DexKind::OrcaWhirlpool.program_id()))
        .map_err(|e| CopyTraderError::Decode(e.to_string()))?
        .0;
    if adaptive {
        addresses.push(oracle);
    }
    // Fetch both possible ATAs to avoid an extra round trip to discover mint owners.
    for mint in [pool.token_mint_a, pool.token_mint_b] {
        for program in [spl_token::id(), spl_token_2022::id()] {
            addresses.push(associated_token_address(&owner, &mint, &program));
        }
    }
    let (accounts, epoch, rent) = tokio::try_join!(
        rpc.get_multiple_ui_accounts_with_config(
            &addresses,
            RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::processed()),
                min_context_slot: Some(slot),
                ..RpcAccountInfoConfig::default()
            }
        ),
        rpc.get_epoch_info(),
        rpc.get_minimum_balance_for_rent_exemption(165),
    )
    .map_err(|e| CopyTraderError::Execution(format!("Orca snapshot failed: {e}")))?;
    if accounts.context.slot < slot || accounts.value.len() != addresses.len() {
        return Err(CopyTraderError::Execution(
            "invalid Orca snapshot response".to_owned(),
        ));
    }
    let mut values = HashMap::new();
    for (index, (key, account)) in addresses.into_iter().zip(accounts.value).enumerate() {
        let account = account
            .map(|account| {
                account.decode::<Account>().ok_or_else(|| {
                    CopyTraderError::Decode("undecodable Orca snapshot account".to_owned())
                })
            })
            .transpose()?;
        if (index < 5 || adaptive && key == oracle)
            && account
                .as_ref()
                .is_some_and(|a| a.owner != DexKind::OrcaWhirlpool.program_id() || a.executable)
        {
            return Err(CopyTraderError::Decode(
                "invalid Orca state account owner".to_owned(),
            ));
        }
        if let Some(account) = &account {
            if index < 5 {
                let ticks = TickArray::from_bytes(&account.data)
                    .map_err(|e| CopyTraderError::Decode(format!("invalid Orca ticks: {e}")))?;
                let (tick_pool, tick_start) = match ticks {
                    TickArray::FixedTickArray(ticks) => (ticks.whirlpool, ticks.start_tick_index),
                    TickArray::DynamicTickArray(ticks) => (ticks.whirlpool, ticks.start_tick_index),
                };
                let expected_key = get_tick_array_address(
                    &address,
                    tick_start,
                    Some(DexKind::OrcaWhirlpool.program_id()),
                )
                .map_err(|e| CopyTraderError::Decode(e.to_string()))?
                .0;
                if tick_pool != address || expected_key != key {
                    return Err(CopyTraderError::Decode(
                        "Orca tick array identity mismatch".to_owned(),
                    ));
                }
            } else if adaptive && key == oracle {
                let state = Oracle::from_bytes(&account.data)
                    .map_err(|e| CopyTraderError::Decode(format!("invalid Orca oracle: {e}")))?;
                if state.discriminator != ORACLE_DISCRIMINATOR || state.whirlpool != address {
                    return Err(CopyTraderError::Decode(
                        "Orca oracle identity mismatch".to_owned(),
                    ));
                }
            } else if account.executable
                || ![spl_token::id(), spl_token_2022::id()].contains(&account.owner)
            {
                return Err(CopyTraderError::Decode(
                    "invalid Orca token account owner".to_owned(),
                ));
            }
        }
        values.insert(
            key.to_string(),
            account.as_ref().map(account_json).unwrap_or(Value::Null),
        );
    }
    values.insert(address.to_string(), account_json(pool_account));
    Ok(RpcClient::new_sender(
        Snapshot {
            accounts: values,
            slot: accounts.context.slot,
            epoch: serde_json::to_value(epoch)?,
            rent,
        },
        RpcClientConfig::with_commitment(CommitmentConfig::processed()),
    ))
}

fn account_json(account: &Account) -> Value {
    json!({"lamports":account.lamports,"owner":account.owner.to_string(),
        "data":[STANDARD.encode(&account.data),"base64"],"executable":account.executable,
        "rentEpoch":account.rent_epoch,"space":account.data.len()})
}

struct Snapshot {
    accounts: HashMap<String, Value>,
    slot: u64,
    epoch: Value,
    rent: u64,
}

impl Snapshot {
    fn account(&self, key: &Value) -> std::result::Result<Value, ClientError> {
        key.as_str()
            .and_then(|key| self.accounts.get(key))
            .cloned()
            .ok_or_else(|| {
                ClientErrorKind::Custom("account absent from Orca snapshot".to_owned()).into()
            })
    }
}

#[async_trait]
impl RpcSender for Snapshot {
    async fn send(
        &self,
        request: RpcRequest,
        params: Value,
    ) -> std::result::Result<Value, ClientError> {
        match request {
            RpcRequest::GetAccountInfo => {
                Ok(json!({"context":{"slot":self.slot},"value":self.account(&params[0])?}))
            }
            RpcRequest::GetMultipleAccounts => {
                let keys = params[0].as_array().ok_or_else(|| {
                    ClientErrorKind::Custom("invalid snapshot account request".to_owned())
                })?;
                let values = keys
                    .iter()
                    .map(|key| self.account(key))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(json!({"context":{"slot":self.slot},"value":values}))
            }
            RpcRequest::GetEpochInfo => Ok(self.epoch.clone()),
            RpcRequest::GetMinimumBalanceForRentExemption if params[0] == 165 => {
                Ok(json!(self.rent))
            }
            _ => Err(ClientErrorKind::Custom(format!(
                "unsupported Orca snapshot request: {request}"
            ))
            .into()),
        }
    }
    fn get_transport_stats(&self) -> RpcTransportStats {
        RpcTransportStats::default()
    }
    fn url(&self) -> String {
        "orca-snapshot".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::HttpConfig,
        domain::{AssetId, SizedTrade, TradeIntent},
        http::HttpTransport,
        routing::{PoolDescriptor, RouteContext, RouteDexAdapter, orca::OrcaAdapter},
        test_rpc::{TestRpc, mint_account},
    };
    use orca_whirlpools::{SwapConfig, SwapQuote, SwapType, swap_instructions};
    use solana_sdk::signature::Signature;
    use std::time::{Duration, Instant};

    fn pool_account(a: Pubkey, b: Pubkey) -> Account {
        let mut data = vec![0; Whirlpool::LEN];
        data[..8].copy_from_slice(&[63, 149, 209, 12, 225, 128, 99, 9]);
        data[41..43].copy_from_slice(&64_u16.to_le_bytes());
        data[43..45].copy_from_slice(&64_u16.to_le_bytes());
        data[45..47].copy_from_slice(&3000_u16.to_le_bytes());
        data[49..65].copy_from_slice(&1_000_000_000_000_u128.to_le_bytes());
        data[65..81].copy_from_slice(&(1_u128 << 64).to_le_bytes());
        data[101..133].copy_from_slice(a.as_ref());
        data[133..165].copy_from_slice(Pubkey::new_unique().as_ref());
        data[181..213].copy_from_slice(b.as_ref());
        data[213..245].copy_from_slice(Pubkey::new_unique().as_ref());
        Account {
            data,
            owner: DexKind::OrcaWhirlpool.program_id(),
            lamports: 1,
            executable: false,
            rent_epoch: 0,
        }
    }

    #[tokio::test]
    async fn batched_quotes_match_sdk_and_remove_serial_network_waits() {
        for native in [false, true] {
            for reverse in [false, true] {
                let a = if native {
                    spl_token::native_mint::id()
                } else {
                    Pubkey::new_unique()
                };
                let b = Pubkey::new_unique();
                let owner = Pubkey::new_unique();
                let address = Pubkey::new_unique();
                let account = pool_account(a, b);
                let mut accounts = HashMap::new();
                accounts.insert(address.to_string(), account_json(&account));
                for mint in [a, b] {
                    let program = if reverse && mint == b {
                        spl_token_2022::id()
                    } else {
                        spl_token::id()
                    };
                    let mut mint_value = mint_account();
                    mint_value["owner"] = json!(program.to_string());
                    accounts.insert(mint.to_string(), mint_value);
                    let ata = associated_token_address(&owner, &mint, &program);
                    let mut data = vec![0; 165];
                    data[..32].copy_from_slice(mint.as_ref());
                    data[32..64].copy_from_slice(owner.as_ref());
                    data[64..72].copy_from_slice(&1_000_000_u64.to_le_bytes());
                    data[108] = 1;
                    accounts.insert(ata.to_string(), json!({"lamports":2039280,"owner":program.to_string(),"data":[STANDARD.encode(data),"base64"],"executable":false,"rentEpoch":0}));
                }
                let server = TestRpc::start(move |request| {
                    let value = match request["method"].as_str().expect("method") {
                        "getAccountInfo" => json!({"context":{"slot":42},"value":accounts.get(request["params"][0].as_str().unwrap()).cloned().unwrap_or(Value::Null)}),
                        "getMultipleAccounts" => {
                            let values: Vec<_> = request["params"][0].as_array().unwrap().iter().map(|key| accounts.get(key.as_str().unwrap()).cloned().unwrap_or(Value::Null)).collect();
                            json!({"context":{"slot":42},"value":values})
                        }
                        "getEpochInfo" => json!({"absoluteSlot":42,"blockHeight":42,"epoch":0,"slotIndex":42,"slotsInEpoch":432000,"transactionCount":0}),
                        "getMinimumBalanceForRentExemption" => json!(2039280),
                        other => panic!("unexpected request: {other}"),
                    };
                    json!({"test_delay_ms":30,"test_result":value})
                }).await;
                let rpc = HttpTransport::new(&HttpConfig::default())
                    .unwrap()
                    .solana_rpc(&server.url);
                let input = if reverse { b } else { a };
                let output = if reverse { a } else { b };
                let started = Instant::now();
                let baseline = swap_instructions(
                    &rpc,
                    address,
                    1000,
                    input,
                    SwapType::ExactIn,
                    SwapConfig {
                        slippage_tolerance_bps: Some(5000),
                        signer: Some(owner),
                        whirlpool_deployment: None,
                    },
                )
                .await
                .unwrap();
                let baseline_time = started.elapsed();
                let before = server.count("getMultipleAccounts");
                let pool_reads = server.count("getAccountInfo");
                let trade = SizedTrade {
                    input_amount: 1000,
                    intent: TradeIntent {
                        source_pool: None,
                        source_signature: Signature::default(),
                        slot: 42,
                        input_asset: if input == spl_token::native_mint::id() {
                            AssetId::NativeSol
                        } else {
                            AssetId::Token(input)
                        },
                        output_asset: if output == spl_token::native_mint::id() {
                            AssetId::NativeSol
                        } else {
                            AssetId::Token(output)
                        },
                        source_input_amount: 1000,
                        source_output_amount: 996,
                    },
                };
                let pool = PoolDescriptor {
                    dex: DexKind::OrcaWhirlpool,
                    address,
                    mint_a: a,
                    mint_b: b,
                    liquidity_hint: 0,
                };
                let started = Instant::now();
                let fast = OrcaAdapter
                    .prepare(
                        &rpc,
                        &pool,
                        &trade,
                        &RouteContext {
                            copier: owner,
                            slippage_bps: 5000,
                            source_outputs: None,
                        },
                        Some(&account),
                    )
                    .await
                    .unwrap();
                let fast_time = started.elapsed();
                let SwapQuote::ExactIn(quote) = baseline.quote else {
                    panic!("exact in")
                };
                assert_eq!(fast.expected_output, quote.token_est_out);
                assert_eq!(fast.minimum_output, quote.token_min_out);
                assert_eq!(fast.instructions.len(), baseline.instructions.len());
                let swap = fast
                    .instructions
                    .iter()
                    .find(|i| i.program_id == DexKind::OrcaWhirlpool.program_id())
                    .unwrap();
                let old_swap = baseline
                    .instructions
                    .iter()
                    .find(|i| i.program_id == DexKind::OrcaWhirlpool.program_id())
                    .unwrap();
                assert_eq!(swap.data, old_swap.data);
                if !native {
                    assert_eq!(fast.instructions, baseline.instructions);
                }
                assert_eq!(server.count("getMultipleAccounts") - before, 1);
                assert_eq!(
                    server.count("getAccountInfo"),
                    pool_reads,
                    "must reuse prefetched pool"
                );
                assert!(
                    fast_time + Duration::from_millis(60) < baseline_time,
                    "batch {fast_time:?}, SDK {baseline_time:?}"
                );
                assert!(
                    server
                        .peak_in_flight
                        .load(std::sync::atomic::Ordering::SeqCst)
                        >= 3
                );
                let reads = server.count("getMultipleAccounts");
                let epochs = server.count("getEpochInfo");
                let direct = OrcaAdapter
                    .prepare(
                        &rpc,
                        &pool,
                        &trade,
                        &RouteContext {
                            copier: owner,
                            slippage_bps: 5000,
                            source_outputs: Some((996, 498)),
                        },
                        Some(&account),
                    )
                    .await
                    .unwrap();
                assert_eq!(direct.expected_output, 996);
                assert_eq!(direct.minimum_output, 498);
                let direct_swap = direct
                    .instructions
                    .iter()
                    .find(|ix| ix.program_id == DexKind::OrcaWhirlpool.program_id())
                    .unwrap();
                assert_eq!(
                    u64::from_le_bytes(direct_swap.data[8..16].try_into().unwrap()),
                    1000
                );
                assert_eq!(
                    u64::from_le_bytes(direct_swap.data[16..24].try_into().unwrap()),
                    498
                );
                assert_eq!(&direct_swap.data[24..], &old_swap.data[24..]);
                for index in [0, 1, 2, 3, 4, 5, 6, 8, 10, 11, 12, 13, 14, 15, 16] {
                    assert_eq!(
                        direct_swap.accounts[index], old_swap.accounts[index],
                        "account {index}"
                    );
                }
                assert_eq!(server.count("getMultipleAccounts") - reads, 1);
                assert_eq!(server.count("getEpochInfo"), epochs, "no quote epoch read");
                assert_eq!(server.count("getAccountInfo"), pool_reads);
                eprintln!(
                    "Orca native={native} reverse={reverse}: SDK {baseline_time:?}, batched {fast_time:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn snapshots_include_adaptive_oracles_and_reject_invalid_responses() {
        for scenario in [
            "valid",
            "stale",
            "short",
            "tick_owner",
            "tick_data",
            "oracle_pool",
            "rpc_error",
        ] {
            let a = Pubkey::new_unique();
            let b = Pubkey::new_unique();
            let address = Pubkey::new_unique();
            let mut account = pool_account(a, b);
            account.data[43..45].copy_from_slice(&128_u16.to_le_bytes());
            let state = Whirlpool::from_bytes(&account.data).unwrap();
            let oracle_key =
                get_oracle_address(&address, Some(DexKind::OrcaWhirlpool.program_id()))
                    .unwrap()
                    .0;
            let mut oracle = Account {
                data: vec![0; Oracle::LEN],
                ..account.clone()
            };
            oracle.data[..8].copy_from_slice(&ORACLE_DISCRIMINATOR);
            oracle.data[8..40].copy_from_slice(if scenario == "oracle_pool" {
                a.as_ref()
            } else {
                address.as_ref()
            });
            let oracle_json = account_json(&oracle);
            let expected = oracle.clone();
            let server = TestRpc::start(move |request| {
                match request["method"].as_str().unwrap() {
                    "getMultipleAccounts" => {
                        assert_eq!(request["params"][1]["minContextSlot"], 42);
                        if scenario == "rpc_error" { return json!({"error":{"code":-32000,"message":"unavailable"}}); }
                        let mut values: Vec<_> = request["params"][0].as_array().unwrap().iter().map(|key| {
                            if key == &json!(oracle_key.to_string()) { oracle_json.clone() }
                            else if key == &json!(a.to_string()) || key == &json!(b.to_string()) { mint_account() }
                            else { Value::Null }
                        }).collect();
                        if scenario == "short" { values.pop(); }
                        if scenario == "tick_owner" || scenario == "tick_data" {
                            values[0] = mint_account();
                            if scenario == "tick_data" { values[0]["owner"] = json!(DexKind::OrcaWhirlpool.program_id().to_string()); }
                        }
                        json!({"context":{"slot":if scenario == "stale" {41} else {42}},"value":values})
                    }
                    "getEpochInfo" => json!({"absoluteSlot":42,"blockHeight":42,"epoch":0,"slotIndex":42,"slotsInEpoch":432000,"transactionCount":0}),
                    "getMinimumBalanceForRentExemption" => json!(2039280),
                    other => panic!("unexpected RPC: {other}"),
                }
            }).await;
            let rpc = HttpTransport::new(&HttpConfig::default())
                .unwrap()
                .solana_rpc(&server.url);
            let result =
                quote_client(&rpc, address, &account, &state, Pubkey::new_unique(), 42).await;
            if scenario == "valid" {
                assert_eq!(
                    result.unwrap().get_account(&oracle_key).await.unwrap(),
                    expected
                );
            } else {
                assert!(result.is_err(), "{scenario}");
            }
        }
    }

    #[tokio::test]
    async fn snapshot_rejects_unfetched_accounts_and_transaction_submission() {
        let snapshot = Snapshot {
            accounts: HashMap::new(),
            slot: 42,
            epoch: Value::Null,
            rent: 2039280,
        };
        assert!(
            snapshot
                .send(
                    RpcRequest::GetAccountInfo,
                    json!([Pubkey::new_unique().to_string()])
                )
                .await
                .is_err()
        );
        assert!(
            snapshot
                .send(RpcRequest::SendTransaction, json!(["anything"]))
                .await
                .is_err()
        );
    }
}
