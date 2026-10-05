use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use solana_sdk::{account::Account, hash::Hash, signature::Signature};

use super::*;
use crate::{
    config::{HttpConfig, MainnetConfig},
    domain::{SourcePool, TradeIntent},
    http::HttpTransport,
    mainnet::MainnetClient,
    test_rpc::TestRpc,
};

fn pool_account(dex: DexKind, input: Pubkey, output: Pubkey) -> Account {
    let (length, discriminator, a, b) = match dex {
        DexKind::RaydiumCpmm => (637, [247, 237, 227, 245, 215, 195, 222, 70], 168, 200),
        DexKind::RaydiumClmm => (1544, [247, 237, 227, 245, 215, 195, 222, 70], 73, 105),
        DexKind::OrcaWhirlpool => (653, [63, 149, 209, 12, 225, 128, 99, 9], 101, 181),
        DexKind::MeteoraDlmm => (904, [33, 11, 49, 98, 181, 101, 177, 13], 88, 120),
        DexKind::PumpSwap => (400, [241, 154, 109, 4, 17, 177, 109, 188], 43, 75),
    };
    let mut data = vec![0; length];
    data[..8].copy_from_slice(&discriminator);
    data[a..a + 32].copy_from_slice(input.as_ref());
    data[b..b + 32].copy_from_slice(output.as_ref());
    if dex == DexKind::RaydiumCpmm {
        for offset in [8, 72, 104, 296] {
            data[offset..offset + 32].copy_from_slice(Pubkey::new_unique().as_ref());
        }
        for offset in [232, 264] {
            data[offset..offset + 32].copy_from_slice(spl_token::id().as_ref());
        }
    }
    Account {
        data,
        owner: dex.program_id(),
        lamports: 1,
        executable: false,
        rent_epoch: 0,
    }
}

fn account_json(account: &Account) -> Value {
    json!({"lamports":account.lamports,"owner":account.owner.to_string(),
        "data":[STANDARD.encode(&account.data),"base64"],"executable":account.executable,"rentEpoch":0})
}

fn mint_info() -> MintInfo {
    MintInfo {
        decimals: 6,
        token_program: spl_token::id(),
        has_transfer_fee: false,
    }
}

#[test]
fn validates_pool_identity_pair_and_layout_for_every_dex() {
    let input = Pubkey::new_unique();
    let output = Pubkey::new_unique();
    let info = mint_info();
    for dex in [
        DexKind::RaydiumCpmm,
        DexKind::RaydiumClmm,
        DexKind::OrcaWhirlpool,
        DexKind::MeteoraDlmm,
        DexKind::PumpSwap,
    ] {
        let hint = SourcePool {
            dex,
            address: Pubkey::new_unique(),
        };
        let account = pool_account(dex, input, output);
        assert!(
            source_pool::validate(hint, &account, input, output, (&info, &info)).is_ok(),
            "{dex:?}"
        );
        assert!(source_pool::validate(hint, &account, output, input, (&info, &info)).is_ok());
        assert!(
            source_pool::validate(hint, &account, input, Pubkey::new_unique(), (&info, &info))
                .is_err()
        );
        for mutation in 0..4 {
            let mut bad = account.clone();
            match mutation {
                0 => bad.owner = Pubkey::new_unique(),
                1 => bad.executable = true,
                2 => bad.data[0] ^= 255,
                _ => bad.data.truncate(8),
            }
            assert!(source_pool::validate(hint, &bad, input, output, (&info, &info)).is_err());
        }
        if dex == DexKind::RaydiumCpmm {
            let mut bad = account;
            bad.data[232..264].fill(0);
            assert!(source_pool::validate(hint, &bad, input, output, (&info, &info)).is_err());
        }
    }
}

#[tokio::test]
async fn source_pool_uses_real_adapter_and_falls_back_without_sending() {
    for scenario in [
        "direct",
        "fast",
        "bad_owner",
        "wrong_pair",
        "simulation",
        "slow",
        "cached",
        "no_hint",
        "stale",
        "missing",
    ] {
        let input = Pubkey::new_unique();
        let output = Pubkey::new_unique();
        let source = Pubkey::new_unique();
        let alternative = Pubkey::new_unique();
        let valid = pool_account(DexKind::RaydiumCpmm, input, output);
        let config_address = Pubkey::new_from_array(valid.data[8..40].try_into().unwrap());
        let mut config = Account {
            data: vec![0; 20],
            owner: DexKind::RaydiumCpmm.program_id(),
            ..Account::default()
        };
        config.data[..8].copy_from_slice(&[218, 244, 33, 104, 203, 203, 43, 111]);
        config.data[12..20].copy_from_slice(&3000_u64.to_le_bytes());
        let mut source_account = valid.clone();
        if scenario == "bad_owner" || scenario == "cached" {
            source_account.owner = Pubkey::new_unique();
        }
        if scenario == "wrong_pair" {
            source_account.data[168..200].fill(0);
        }
        let server = TestRpc::start(move |request| {
            match request["method"].as_str().unwrap() {
                "getVersion" => json!({"solana-core":"3.1.0","feature-set":1}),
                "getAccountInfo" => {
                    let address = request["params"][0].as_str().unwrap();
                    if address == source.to_string() && scenario == "missing" {
                        return json!({"context":{"slot":42},"value":null});
                    }
                    let account = if address == source.to_string() { &source_account }
                        else if address == alternative.to_string() { &valid }
                        else { assert_eq!(address, config_address.to_string()); &config };
                    let result = json!({"context":{"slot":if scenario == "stale" && address == source.to_string() {41} else {42}},"value":account_json(account)});
                    if scenario == "slow" && address == source.to_string() {
                        return json!({"test_delay_ms":1200,"test_result":result});
                    }
                    result
                }
                "getMultipleAccounts" => json!({"context":{"slot":42},"value":[null,null]}),
                "getBlockHeight" => json!(100),
                "getLatestBlockhash" => json!({"context":{"slot":42},"value":{"blockhash":Hash::new_unique().to_string(),"lastValidBlockHeight":1000}}),
                "getTokenAccountBalance" => json!({"context":{"slot":42},"value":{"amount":"1000000000","decimals":6,"uiAmount":1000.0,"uiAmountString":"1000"}}),
                "getFeeForMessage" => json!({"context":{"slot":42},"value":5000}),
                "getBalance" => json!({"context":{"slot":42},"value":1000000000}),
                "getPriorityFeeEstimate" => json!({"priorityFeeEstimate":10}),
                "simulateTransaction" => {
                    let bytes = STANDARD.decode(request["params"][0].as_str().unwrap()).unwrap();
                    let tx: Transaction = bincode::deserialize(&bytes).unwrap();
                    assert!(tx.verify().is_ok());
                    assert_eq!(request["params"][1]["sigVerify"], true);
                    let source_route = tx.message.account_keys.contains(&source);
                    json!({"context":{"slot":42},"value":{"err":if scenario == "simulation" && source_route {json!("AccountNotFound")} else {Value::Null},"logs":[],"accounts":null,"unitsConsumed":100}})
                }
                "getProgramAccounts" => {
                    if request["params"][0] == DexKind::RaydiumCpmm.program_id().to_string() {
                        json!([{"pubkey":alternative.to_string(),"account":account_json(&valid)}])
                    } else { json!([]) }
                }
                "getProgramAccountsV2" => json!({"accounts":[]}),
                other => panic!("unexpected RPC {other}"),
            }
        }).await;
        let transport = HttpTransport::new(&HttpConfig::default()).unwrap();
        let backend = ExecutionBackend::Mainnet(Arc::new(MainnetClient::new(
            server.url.clone(),
            &MainnetConfig {
                source_direct: scenario == "fast",
                fixed_priority_fee_micro_lamports: Some(77),
                sender_url: server.url.clone(),
                tip_lamports: 5000,
                priority_level: "high".to_owned(),
                max_priority_fee_micro_lamports: 100,
            },
            transport.clone(),
        )));
        let router = Router::supported(
            Arc::new(transport.solana_rpc(&server.url)),
            4,
            Duration::from_secs(2),
        );
        if scenario == "cached" {
            router
                .catalog
                .remember(
                    PoolDescriptor {
                        dex: DexKind::RaydiumCpmm,
                        address: alternative,
                        mint_a: input,
                        mint_b: output,
                        liquidity_hint: 1,
                    },
                    4,
                )
                .await;
        }
        let trade = SizedTrade {
            intent: TradeIntent {
                source_pool: (scenario != "no_hint").then_some(SourcePool {
                    dex: DexKind::RaydiumCpmm,
                    address: source,
                }),
                source_signature: Signature::default(),
                slot: 42,
                input_asset: AssetId::Token(input),
                output_asset: AssetId::Token(output),
                source_input_amount: 1_000_000,
                source_output_amount: 900_000,
            },
            input_amount: 10_000,
        };
        let info = mint_info();
        let signer = Keypair::new();
        let mut timings = RoutingTimings::default();
        let prefetched = router.prefetch_source(&trade.intent, &timings.stages);
        let winner = router
            .race_prepared(
                &trade,
                &signer,
                &backend,
                100,
                RoutingInputs {
                    mint_infos: (&info, &info),
                    source: prefetched,
                },
                &mut timings,
            )
            .await
            .unwrap();
        assert_eq!(
            winner.route.pool,
            if scenario == "direct" || scenario == "fast" {
                source
            } else {
                alternative
            },
            "{scenario}"
        );
        let swap = winner
            .route
            .instructions
            .iter()
            .find(|ix| ix.program_id == DexKind::RaydiumCpmm.program_id())
            .unwrap();
        assert_eq!(
            u64::from_le_bytes(swap.data[8..16].try_into().unwrap()),
            trade.input_amount
        );
        assert_eq!(swap.accounts[0].pubkey, signer.pubkey());
        assert!(winner.route.expected_output < trade.input_amount);
        assert_eq!(server.count("sendTransaction"), 0);
        assert_eq!(server.count("getMultipleAccounts"), 1, "{scenario}");
        assert_eq!(server.count("getLatestBlockhash"), 1, "{scenario}");
        if scenario == "direct" || scenario == "cached" || scenario == "fast" {
            assert_eq!(server.count("getProgramAccounts"), 0);
            assert_eq!(server.count("getProgramAccountsV2"), 0);
            assert_eq!(timings.discovery_ms, 0);
        } else {
            assert!(server.count("getProgramAccounts") > 0, "{scenario}");
        }
        if scenario == "direct" {
            assert_eq!(server.count("simulateTransaction"), 1);
            assert_eq!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request["method"] == "getAccountInfo"
                        && request["params"][0] == source.to_string())
                    .count(),
                1
            );
            assert!(router.catalog.contains_pair(input, output).await);
            {
                let requests = server.requests.lock().unwrap();
                let validation = requests
                    .iter()
                    .find(|request| request["method"] == "getAccountInfo")
                    .unwrap();
                assert_eq!(validation["params"][1]["minContextSlot"], 42);
            }
            assert_outgoing_swap_decodes_to_same_pool(
                &winner,
                &signer,
                input,
                output,
                trade.input_amount,
            );
            // A later observation without a hint reuses the learned pool, with fresh state.
            let mut cached_trade = trade.clone();
            cached_trade.intent.source_pool = None;
            router
                .race(&cached_trade, &signer, &backend, 100, (&info, &info))
                .await
                .unwrap();
            assert_eq!(server.count("getProgramAccounts"), 0);
            assert_eq!(server.count("simulateTransaction"), 2);
        }
        if scenario == "fast" {
            assert!(winner.transaction.verify().is_ok());
            assert_eq!(winner.route.expected_output, 9000);
            assert_eq!(winner.route.minimum_output, 8910);
            assert_eq!(
                u64::from_le_bytes(swap.data[16..24].try_into().unwrap()),
                8910
            );
            for method in [
                "simulateTransaction",
                "getPriorityFeeEstimate",
                "getFeeForMessage",
                "getProgramAccounts",
                "getProgramAccountsV2",
            ] {
                assert_eq!(server.count(method), 0, "{method}");
            }
            assert_eq!(
                server.count("getAccountInfo"),
                1,
                "only the source pool, no quote config"
            );
            let price = winner
                .route
                .instructions
                .iter()
                .find(|ix| {
                    ix.data.first() == Some(&3)
                        && ix.program_id == solana_compute_budget_interface::id()
                })
                .unwrap();
            assert_eq!(u64::from_le_bytes(price.data[1..9].try_into().unwrap()), 77);
            let stages = timings.stages.snapshot();
            assert!(stages.contains_key("route_instruction_build_ms"));
            assert!(!stages.contains_key("route_quote_ms"));
            assert!(!stages.contains_key("route_simulation_ms"));
            assert!(!stages.contains_key("route_fee_estimation_ms"));
            let before = server.count("getAccountInfo");
            let mut no_source = trade.clone();
            no_source.intent.source_pool = None;
            assert!(
                router
                    .race(&no_source, &signer, &backend, 5000, (&info, &info))
                    .await
                    .is_err()
            );
            assert_eq!(server.count("getAccountInfo"), before);
            let mut invalid_source = trade.clone();
            invalid_source.intent.source_pool.as_mut().unwrap().dex = DexKind::OrcaWhirlpool;
            assert!(
                router
                    .race(&invalid_source, &signer, &backend, 5000, (&info, &info))
                    .await
                    .is_err()
            );
            assert_eq!(server.count("getProgramAccounts"), 0);
            assert_eq!(server.count("getProgramAccountsV2"), 0);
        }
        if scenario == "simulation" {
            assert_eq!(server.count("simulateTransaction"), 2);
        }
        if scenario == "slow" {
            assert!(timings.source_route_ms >= 900);
        }
    }
}

fn assert_outgoing_swap_decodes_to_same_pool(
    winner: &ExecutableRoute,
    signer: &Keypair,
    input: Pubkey,
    output: Pubkey,
    amount: u64,
) {
    use crate::{
        decode::TransactionDecoder,
        domain::{
            ObservedTransaction, SignalOrigin, TransactionMeta, UiTokenAmount, UiTokenBalance,
        },
    };
    let balance = |mint: Pubkey, amount: u64| UiTokenBalance {
        account_index: 1,
        mint: mint.to_string(),
        owner: Some(signer.pubkey().to_string()),
        program_id: Some(spl_token::id().to_string()),
        ui_token_amount: UiTokenAmount {
            amount: amount.to_string(),
            decimals: 6,
        },
    };
    let observed = ObservedTransaction {
        signature: winner.transaction.signatures[0],
        slot: 42,
        block_time: None,
        origin: SignalOrigin::Live,
        transaction: winner.transaction.clone().into(),
        meta: TransactionMeta {
            pre_token_balances: Some(vec![balance(input, amount), balance(output, 0)]),
            post_token_balances: Some(vec![
                balance(input, 0),
                balance(output, winner.route.expected_output),
            ]),
            ..TransactionMeta::default()
        },
        raw_payload: String::new(),
        received_bytes: 0,
    };
    let decoded = TransactionDecoder::new(signer.pubkey())
        .decode(&observed)
        .unwrap();
    assert_eq!(
        decoded.source_pool,
        Some(SourcePool {
            dex: winner.route.dex,
            address: winner.route.pool
        })
    );
    assert_eq!(decoded.source_input_amount, amount);
}

#[tokio::test]
async fn discovery_is_bounded_by_route_deadline_and_records_cancelled_time() {
    let server = TestRpc::start(|request| match request["method"].as_str().unwrap() {
        "getVersion" => json!({"solana-core":"3.1.0","feature-set":1}),
        _ => json!({"test_delay_ms":1000,"test_result":[]}),
    })
    .await;
    let transport = HttpTransport::new(&HttpConfig::default()).unwrap();
    let router = Router::supported(
        Arc::new(transport.solana_rpc(&server.url)),
        4,
        Duration::from_millis(50),
    );
    let backend = ExecutionBackend::Mainnet(Arc::new(MainnetClient::new(
        server.url.clone(),
        &MainnetConfig {
            source_direct: false,
            fixed_priority_fee_micro_lamports: None,
            sender_url: server.url.clone(),
            tip_lamports: 5000,
            priority_level: "high".to_owned(),
            max_priority_fee_micro_lamports: 100,
        },
        transport,
    )));
    let trade = SizedTrade {
        intent: TradeIntent {
            source_pool: None,
            source_signature: Signature::default(),
            slot: 42,
            input_asset: AssetId::Token(Pubkey::new_unique()),
            output_asset: AssetId::Token(Pubkey::new_unique()),
            source_input_amount: 100,
            source_output_amount: 100,
        },
        input_amount: 100,
    };
    let info = mint_info();
    let mut timings = RoutingTimings::default();
    let result = router
        .race_with_timings(
            &trade,
            &Keypair::new(),
            &backend,
            100,
            (&info, &info),
            &mut timings,
        )
        .await;
    assert!(
        result
            .err()
            .expect("timeout")
            .to_string()
            .contains("timed out")
    );
    assert!(timings.discovery_ms >= 40);
    assert_eq!(server.count("simulateTransaction"), 0);
}

#[tokio::test]
async fn refresh_preserves_learned_pairs_and_remember_preserves_alternatives() {
    let catalog = PoolCatalog::default();
    let input = Pubkey::new_unique();
    let output = Pubkey::new_unique();
    let learned = PoolDescriptor {
        dex: DexKind::RaydiumCpmm,
        address: Pubkey::new_unique(),
        mint_a: input,
        mint_b: output,
        liquidity_hint: 0,
    };
    let mut alternative = learned.clone();
    alternative.address = Pubkey::new_unique();
    catalog.remember(alternative.clone(), 4).await;
    catalog.remember(learned.clone(), 4).await;
    catalog.remember(learned.clone(), 4).await;
    catalog
        .refresh(
            DexKind::RaydiumCpmm,
            &[Pubkey::new_unique(), Pubkey::new_unique()],
            vec![],
        )
        .await;
    let candidates = catalog
        .candidates(DexKind::RaydiumCpmm, output, input, 4)
        .await;
    assert_eq!(candidates.len(), 2);
    assert!(candidates.contains(&learned));
    assert!(candidates.contains(&alternative));
    catalog
        .refresh(DexKind::RaydiumCpmm, &[input, output], vec![])
        .await;
    assert!(!catalog.contains_pair(input, output).await);
}

#[test]
fn source_price_minimum_is_scaled_and_checked() {
    let mut trade = SizedTrade {
        input_amount: 1000,
        intent: TradeIntent {
            source_pool: None,
            source_signature: Signature::default(),
            slot: 42,
            input_asset: AssetId::Token(Pubkey::new_unique()),
            output_asset: AssetId::Token(Pubkey::new_unique()),
            source_input_amount: 10000,
            source_output_amount: 8000,
        },
    };
    assert_eq!(super::source_outputs(&trade, 5000).unwrap(), (800, 400));
    assert!(super::source_outputs(&trade, 10001).is_err());
    trade.intent.source_input_amount = 0;
    assert!(super::source_outputs(&trade, 5000).is_err());
    trade.intent.source_input_amount = 1;
    trade.intent.source_output_amount = u64::MAX;
    trade.input_amount = u64::MAX;
    assert!(super::source_outputs(&trade, 5000).is_err());
    trade.input_amount = 0;
    assert!(super::source_outputs(&trade, 5000).is_err());
}
