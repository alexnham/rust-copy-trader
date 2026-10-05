use solana_sdk::signature::Signer;
use tracing::{debug, info};

use crate::{
    config::{AppConfig, TokenRule},
    error::{CopyTraderError, Result},
    storage::Store,
    surfpool::SurfpoolClient,
    telemetry::compact_id,
    token::{
        accounts::associated_token_address,
        extensions::{MintInfo, TokenSafetyClient, decimal_to_atomic},
    },
};

pub async fn seed_balances(
    config: &AppConfig,
    store: &Store,
    surfpool: &SurfpoolClient,
    token_safety: &TokenSafetyClient,
    signer: &impl Signer,
) -> Result<()> {
    let surfpool_config = config.surfpool.as_ref().ok_or_else(|| {
        CopyTraderError::Configuration("missing [surfpool] configuration".to_owned())
    })?;
    if !store.is_seeded(&surfpool_config.run_id, "__SOL__").await? {
        surfpool
            .set_lamports(&signer.pubkey(), surfpool_config.seed_sol_lamports)
            .await?;
        store
            .record_seeded_balance(
                &surfpool_config.run_id,
                "__SOL__",
                &signer.pubkey().to_string(),
                surfpool_config.seed_sol_lamports,
            )
            .await?;
        info!(
            lamports = surfpool_config.seed_sol_lamports,
            "seeded Surfpool SOL balance"
        );
    } else {
        debug!("Surfpool SOL seed already recorded");
    }
    for rule in &config.tokens {
        if store
            .is_seeded(&surfpool_config.run_id, &rule.mint.to_string())
            .await?
        {
            continue;
        }
        let mint = token_safety.inspect_mint(&rule.mint).await?;
        seed_token_balance(
            &surfpool_config.run_id,
            rule,
            &mint,
            store,
            surfpool,
            signer,
        )
        .await?;
    }
    Ok(())
}

pub async fn seed_token_balance(
    run_id: &str,
    rule: &TokenRule,
    mint: &MintInfo,
    store: &Store,
    surfpool: &SurfpoolClient,
    signer: &impl Signer,
) -> Result<()> {
    if store.is_seeded(run_id, &rule.mint.to_string()).await? {
        return Ok(());
    }
    let amount = decimal_to_atomic(&rule.seed_balance, mint.decimals)?;
    surfpool
        .set_token_balance(&signer.pubkey(), &rule.mint, amount, &mint.token_program)
        .await?;
    let token_account = associated_token_address(&signer.pubkey(), &rule.mint, &mint.token_program);
    store
        .record_seeded_balance(
            run_id,
            &rule.mint.to_string(),
            &token_account.to_string(),
            amount,
        )
        .await?;
    info!(mint = %compact_id(rule.mint.to_string()), amount, "seeded Surfpool token balance");
    Ok(())
}
