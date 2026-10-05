use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Signature, transaction::Transaction};

use crate::{
    config::ExecutionTarget, error::Result, mainnet::MainnetClient, surfpool::SurfpoolClient,
};

pub enum ExecutionBackend {
    Surfpool(Arc<SurfpoolClient>),
    Mainnet(Arc<MainnetClient>),
}

impl ExecutionBackend {
    pub const fn target(&self) -> ExecutionTarget {
        match self {
            Self::Surfpool(_) => ExecutionTarget::Surfpool,
            Self::Mainnet(_) => ExecutionTarget::Mainnet,
        }
    }

    pub const fn label(&self) -> &'static str {
        self.target().as_str()
    }

    pub fn rpc(&self) -> &RpcClient {
        match self {
            Self::Surfpool(client) => &client.rpc,
            Self::Mainnet(client) => &client.rpc,
        }
    }

    pub async fn prepare_market(&self, slot: u64, accounts: &[Pubkey]) -> Result<()> {
        if let Self::Surfpool(client) = self {
            client.align_slot(slot).await?;
            client.reset_accounts(accounts).await?;
        }
        Ok(())
    }

    pub async fn send(&self, transaction: &Transaction) -> Result<Signature> {
        match self {
            Self::Surfpool(client) => client.send(transaction).await,
            Self::Mainnet(client) => client.send(transaction).await,
        }
    }

    pub fn mainnet(&self) -> Option<&MainnetClient> {
        match self {
            Self::Mainnet(client) => Some(client),
            Self::Surfpool(_) => None,
        }
    }

    pub fn surfpool(&self) -> Option<&SurfpoolClient> {
        match self {
            Self::Surfpool(client) => Some(client),
            Self::Mainnet(_) => None,
        }
    }
}
