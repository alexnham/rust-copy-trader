use crate::domain::{DexKind, ObservedTransaction, SourcePool};

use super::transaction::{all_instructions, full_account_keys};

const SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
const SWAP_V2: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
const ANCHOR_EVENT: [u8; 8] = 0x1d9acb512ea545e4_u64.to_le_bytes();

pub(super) fn extract_source_pool(observed: &ObservedTransaction) -> Option<SourcePool> {
    let keys = full_account_keys(observed).ok()?;
    let mut hint = None;
    for instruction in all_instructions(observed).ok()? {
        let program = keys.get(usize::from(instruction.program_id_index))?;
        let Some(dex) = [
            DexKind::RaydiumCpmm,
            DexKind::RaydiumClmm,
            DexKind::OrcaWhirlpool,
            DexKind::MeteoraDlmm,
            DexKind::PumpSwap,
        ]
        .into_iter()
        .find(|dex| dex.program_id() == *program) else {
            continue;
        };
        let discriminator: [u8; 8] = instruction.data.get(..8)?.try_into().ok()?;
        // Anchor event CPIs accompany swaps on PumpSwap and Meteora.
        if discriminator == ANCHOR_EVENT && instruction.data.len() >= 16 {
            continue;
        }
        // Unknown DEX instructions and multiple swaps use general discovery.
        let (pool_index, minimum_accounts, minimum_data) = layout(dex, discriminator)?;
        if hint.is_some()
            || instruction.accounts.len() < minimum_accounts
            || instruction.data.len() < minimum_data
        {
            return None;
        }
        let pool = *instruction.accounts.get(pool_index)?;
        hint = Some(SourcePool {
            dex,
            address: *keys.get(usize::from(pool))?,
        });
    }
    hint
}

// Account ordering follows the DEX IDLs; only the pool identity is reused.
fn layout(dex: DexKind, discriminator: [u8; 8]) -> Option<(usize, usize, usize)> {
    match (dex, discriminator) {
        (
            DexKind::RaydiumCpmm,
            [143, 190, 90, 218, 196, 30, 51, 222] | [55, 217, 98, 86, 163, 74, 180, 173],
        ) => Some((3, 13, 24)),
        (DexKind::RaydiumClmm, SWAP) => Some((2, 10, 41)),
        (DexKind::RaydiumClmm, SWAP_V2) => Some((2, 13, 41)),
        (DexKind::OrcaWhirlpool, SWAP) => Some((2, 11, 42)),
        (DexKind::OrcaWhirlpool, SWAP_V2) => Some((4, 15, 43)),
        (
            DexKind::MeteoraDlmm,
            SWAP | [250, 73, 101, 33, 38, 207, 75, 184] | [56, 173, 230, 208, 173, 228, 156, 205],
        ) => Some((0, 15, 24)),
        (
            DexKind::MeteoraDlmm,
            [65, 75, 63, 76, 235, 91, 91, 136]
            | [43, 215, 247, 132, 137, 60, 243, 81]
            | [74, 98, 192, 214, 177, 51, 75, 51],
        ) => Some((0, 16, 24)),
        (
            DexKind::PumpSwap,
            [102, 6, 61, 18, 1, 218, 235, 234] | [198, 46, 21, 82, 180, 217, 232, 112],
        ) => Some((0, 23, 24)),
        (DexKind::PumpSwap, [51, 230, 133, 164, 1, 127, 131, 173]) => Some((0, 21, 24)),
        (
            DexKind::PumpSwap,
            [184, 23, 238, 97, 103, 197, 211, 61]
            | [194, 171, 28, 70, 104, 77, 91, 47]
            | [93, 246, 130, 60, 231, 233, 64, 178],
        ) => Some((0, 17, 24)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        LoadedAddresses, SignalOrigin, TransactionMeta, UiCompiledInstruction, UiInnerInstructions,
    };
    use solana_sdk::{
        hash::Hash,
        message::{
            Message, MessageHeader, VersionedMessage, compiled_instruction::CompiledInstruction, v0,
        },
        pubkey::Pubkey,
        signature::Signature,
        transaction::VersionedTransaction,
    };

    fn observation(dex: DexKind, discriminator: [u8; 8]) -> ObservedTransaction {
        let (index, accounts, bytes) = layout(dex, discriminator).unwrap();
        let mut instruction = CompiledInstruction {
            program_id_index: 1,
            accounts: vec![0; accounts],
            data: vec![0; bytes],
        };
        instruction.accounts[index] = 2;
        instruction.data[..8].copy_from_slice(&discriminator);
        ObservedTransaction {
            signature: Signature::default(),
            slot: 42,
            block_time: None,
            origin: SignalOrigin::Live,
            transaction: VersionedTransaction {
                signatures: vec![],
                message: VersionedMessage::Legacy(Message {
                    header: MessageHeader::default(),
                    account_keys: vec![
                        Pubkey::new_unique(),
                        dex.program_id(),
                        Pubkey::new_unique(),
                    ],
                    recent_blockhash: Hash::default(),
                    instructions: vec![instruction],
                }),
            },
            meta: TransactionMeta::default(),
            raw_payload: String::new(),
            received_bytes: 0,
        }
    }

    #[test]
    fn extracts_supported_outer_swaps_and_rejects_truncation() {
        for (dex, discriminator) in [
            (DexKind::RaydiumCpmm, [143, 190, 90, 218, 196, 30, 51, 222]),
            (DexKind::RaydiumCpmm, [55, 217, 98, 86, 163, 74, 180, 173]),
            (DexKind::RaydiumClmm, SWAP),
            (DexKind::RaydiumClmm, SWAP_V2),
            (DexKind::OrcaWhirlpool, SWAP),
            (DexKind::OrcaWhirlpool, SWAP_V2),
            (DexKind::MeteoraDlmm, SWAP),
            (DexKind::MeteoraDlmm, [65, 75, 63, 76, 235, 91, 91, 136]),
            (DexKind::PumpSwap, [102, 6, 61, 18, 1, 218, 235, 234]),
            (DexKind::PumpSwap, [51, 230, 133, 164, 1, 127, 131, 173]),
            (DexKind::PumpSwap, [194, 171, 28, 70, 104, 77, 91, 47]),
        ] {
            let mut observed = observation(dex, discriminator);
            let pool = observed.transaction.message.static_account_keys()[2];
            assert_eq!(
                extract_source_pool(&observed),
                Some(SourcePool { dex, address: pool })
            );
            let VersionedMessage::Legacy(ref mut message) = observed.transaction.message else {
                unreachable!()
            };
            message.instructions[0].data.truncate(8);
            assert_eq!(extract_source_pool(&observed), None);
        }
    }

    #[test]
    fn resolves_inner_swap_pool_and_program_from_lookup_table_addresses() {
        let mut observed = observation(DexKind::OrcaWhirlpool, SWAP_V2);
        let pool = observed.transaction.message.static_account_keys()[2];
        let mut swap = observed.transaction.message.instructions()[0].clone();
        swap.program_id_index = 2;
        swap.accounts[4] = 1;
        observed.transaction.message = VersionedMessage::V0(v0::Message {
            header: MessageHeader::default(),
            account_keys: vec![Pubkey::new_unique()],
            recent_blockhash: Hash::default(),
            instructions: vec![],
            address_table_lookups: vec![v0::MessageAddressTableLookup {
                account_key: Pubkey::new_unique(),
                writable_indexes: vec![0],
                readonly_indexes: vec![1],
            }],
        });
        observed.meta.loaded_addresses = Some(LoadedAddresses {
            writable: vec![pool.to_string()],
            readonly: vec![DexKind::OrcaWhirlpool.program_id().to_string()],
        });
        observed.meta.inner_instructions = Some(vec![UiInnerInstructions {
            index: 0,
            instructions: vec![UiCompiledInstruction {
                program_id_index: swap.program_id_index,
                accounts: swap.accounts,
                data: bs58::encode(swap.data).into_string(),
                stack_height: Some(2),
            }],
        }]);
        assert_eq!(
            extract_source_pool(&observed),
            Some(SourcePool {
                dex: DexKind::OrcaWhirlpool,
                address: pool
            })
        );
        observed.meta.loaded_addresses = None;
        assert_eq!(extract_source_pool(&observed), None);
    }

    #[test]
    fn ambiguous_unknown_and_malformed_instructions_use_fallback() {
        for variant in 0..5 {
            let mut observed = observation(DexKind::RaydiumClmm, SWAP_V2);
            let VersionedMessage::Legacy(ref mut message) = observed.transaction.message else {
                unreachable!()
            };
            match variant {
                0 => message.instructions.push(message.instructions[0].clone()),
                1 => message.instructions[0].data[..8].fill(0),
                2 => message.instructions[0].accounts[2] = 255,
                3 => message.instructions[0].program_id_index = 255,
                _ => message.account_keys[1] = Pubkey::new_unique(),
            }
            assert_eq!(extract_source_pool(&observed), None);
        }
    }

    #[test]
    fn event_cpis_do_not_turn_a_single_swap_into_an_unknown_route() {
        let mut observed = observation(DexKind::PumpSwap, [102, 6, 61, 18, 1, 218, 235, 234]);
        let expected = extract_source_pool(&observed);
        let mut event = ANCHOR_EVENT.to_vec();
        event.extend_from_slice(&[0; 32]);
        observed.meta.inner_instructions = Some(vec![UiInnerInstructions {
            index: 0,
            instructions: vec![UiCompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: bs58::encode(event).into_string(),
                stack_height: Some(2),
            }],
        }]);
        assert!(expected.is_some());
        assert_eq!(extract_source_pool(&observed), expected);
    }

    #[test]
    fn arbitrary_truncations_and_indices_do_not_panic() {
        for length in 0..45 {
            for index in 0..=u8::MAX {
                let mut observed = observation(DexKind::OrcaWhirlpool, SWAP_V2);
                let VersionedMessage::Legacy(ref mut message) = observed.transaction.message else {
                    unreachable!()
                };
                message.instructions[0].data.truncate(length);
                message.instructions[0].accounts[4] = index;
                let result = extract_source_pool(&observed);
                if length < 43 || index > 2 {
                    assert!(result.is_none());
                }
            }
        }
    }
}
