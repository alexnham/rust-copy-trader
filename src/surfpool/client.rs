use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::{signature::Signature, transaction::Transaction};
use tracing::debug;
use url::Url;

use crate::{
    error::{CopyTraderError, Result},
    http::HttpTransport,
};

pub struct SurfpoolClient {
    pub rpc: RpcClient,
    http: Arc<HttpTransport>,
    endpoint: Url,
    request_id: AtomicU64,
}

impl SurfpoolClient {
    pub fn new(endpoint: Url, http: Arc<HttpTransport>) -> Self {
        Self {
            rpc: http.solana_rpc(&endpoint),
            http,
            endpoint,
            request_id: AtomicU64::new(1),
        }
    }

    pub async fn health(&self) -> Result<Value> {
        self.call("surfnet_getSurfnetInfo", json!([])).await
    }

    pub async fn set_lamports(&self, account: &Pubkey, lamports: u64) -> Result<()> {
        let _: Value = self
            .call(
                "surfnet_setAccount",
                json!([account.to_string(), { "lamports": lamports }]),
            )
            .await?;
        Ok(())
    }

    pub async fn set_token_balance(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        amount: u64,
        token_program: &Pubkey,
    ) -> Result<()> {
        let _: Value = self
            .call(
                "surfnet_setTokenAccount",
                set_token_account_params(owner, mint, amount, token_program),
            )
            .await?;
        Ok(())
    }

    pub async fn time_travel(&self, timestamp: i64) -> Result<()> {
        let _: Value = self
            .call("surfnet_timeTravel", json!([{ "timestamp": timestamp }]))
            .await?;
        Ok(())
    }

    pub async fn align_slot(&self, source_slot: u64) -> Result<()> {
        let current_slot = self.rpc.get_slot().await.map_err(|error| {
            CopyTraderError::Surfpool(format!("failed to read Surfpool slot: {error}"))
        })?;
        if source_slot > current_slot {
            let _: Value = self
                .call(
                    "surfnet_timeTravel",
                    json!([{ "absoluteSlot": source_slot }]),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn send(&self, transaction: &Transaction) -> Result<Signature> {
        self.rpc
            .send_transaction_with_config(
                transaction,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    max_retries: Some(0),
                    ..RpcSendTransactionConfig::default()
                },
            )
            .await
            .map_err(|error| CopyTraderError::Surfpool(format!("send failed: {error}")))
    }

    pub async fn reset_accounts(&self, accounts: &[Pubkey]) -> Result<()> {
        for account in accounts {
            let _: Value = self
                .call(
                    "surfnet_resetAccount",
                    json!([
                        account.to_string(),
                        {
                            "includeOwnedAccounts": false
                        }
                    ]),
                )
                .await?;
        }

        Ok(())
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let started = Instant::now();
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .http
            .client()
            .post(self.endpoint.clone())
            .timeout(Duration::from_millis(500))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }))
            .send()
            .await
            .map_err(|error| {
                CopyTraderError::Surfpool(format!("{method} request failed: {error}"))
            })?
            .error_for_status()
            .map_err(|error| CopyTraderError::Surfpool(format!("{method} HTTP failure: {error}")))?
            .json::<RpcResponse<T>>()
            .await
            .map_err(|error| {
                CopyTraderError::Surfpool(format!("invalid {method} response: {error}"))
            })?;
        if let Some(error) = response.error {
            return Err(CopyTraderError::Surfpool(format!(
                "{method} returned an error: {error}"
            )));
        }
        debug!(
            method,
            elapsed_ms = started.elapsed().as_millis(),
            "Surfpool RPC request complete"
        );
        response
            .result
            .ok_or_else(|| CopyTraderError::Surfpool(format!("{method} response omitted result")))
    }
}

fn set_token_account_params(
    owner: &Pubkey,
    mint: &Pubkey,
    amount: u64,
    token_program: &Pubkey,
) -> Value {
    json!([
        owner.to_string(),
        mint.to_string(),
        { "amount": amount },
        token_program.to_string()
    ])
}

#[derive(serde::Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<Value>,
}

#[cfg(test)]
mod tests {
    use solana_sdk::pubkey::Pubkey;

    use super::set_token_account_params;

    #[test]
    fn token_account_cheatcode_uses_positional_parameters() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_program = Pubkey::new_unique();
        let params = set_token_account_params(&owner, &mint, 42, &token_program);
        let values = params.as_array();
        let owner = owner.to_string();
        let mint = mint.to_string();
        let token_program = token_program.to_string();

        assert_eq!(values.map(Vec::len), Some(4));
        assert_eq!(
            values.and_then(|items| items[0].as_str()),
            Some(owner.as_str())
        );
        assert_eq!(
            values.and_then(|items| items[1].as_str()),
            Some(mint.as_str())
        );
        assert_eq!(
            values
                .and_then(|items| items[2].get("amount"))
                .and_then(serde_json::Value::as_u64),
            Some(42)
        );
        assert_eq!(
            values.and_then(|items| items[3].as_str()),
            Some(token_program.as_str())
        );
    }
}
