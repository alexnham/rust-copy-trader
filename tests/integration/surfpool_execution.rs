use std::{
    net::TcpListener,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use rust_copy_trader::{config::HttpConfig, http::HttpTransport, surfpool::SurfpoolClient};
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use solana_system_interface::instruction::transfer;
use tempfile::TempDir;
use tokio::time::sleep;
use url::Url;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(10);
const TRANSFER_LAMPORTS: u64 = 1_000_000;

struct SurfpoolProcess {
    child: Child,
    _directory: TempDir,
}

impl SurfpoolProcess {
    fn start(rpc_port: u16, ws_port: u16, studio_port: u16) -> std::io::Result<Self> {
        let directory = tempfile::tempdir()?;
        let child = Command::new("surfpool")
            .args([
                "start",
                "--offline",
                "--ci",
                "--no-deploy",
                "--db",
                ":memory:",
                "--port",
                &rpc_port.to_string(),
                "--ws-port",
                &ws_port.to_string(),
                "--studio-port",
                &studio_port.to_string(),
                "--slot-time",
                "50",
            ])
            .env("NO_DNA", "1")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self {
            child,
            _directory: directory,
        })
    }

    fn exited(&mut self) -> std::io::Result<bool> {
        self.child.try_wait().map(|status| status.is_some())
    }
}

impl Drop for SurfpoolProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reserve_ports() -> std::io::Result<[u16; 3]> {
    let rpc = TcpListener::bind(("127.0.0.1", 0))?;
    let ws = TcpListener::bind(("127.0.0.1", 0))?;
    let studio = TcpListener::bind(("127.0.0.1", 0))?;
    Ok([
        rpc.local_addr()?.port(),
        ws.local_addr()?.port(),
        studio.local_addr()?.port(),
    ])
}

async fn wait_until_ready(
    process: &mut SurfpoolProcess,
    client: &SurfpoolClient,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    while started.elapsed() < STARTUP_TIMEOUT {
        if process.exited()? {
            return Err("Surfpool exited before its RPC became ready".into());
        }
        if client.health().await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err("timed out waiting for Surfpool RPC readiness".into())
}

#[tokio::test]
#[ignore = "requires the surfpool CLI; run with `cargo test --test integration surfpool_executes_signed_transfer -- --ignored`"]
async fn surfpool_executes_signed_transfer() -> Result<(), Box<dyn std::error::Error>> {
    let [rpc_port, ws_port, studio_port] = reserve_ports()?;
    let mut process = SurfpoolProcess::start(rpc_port, ws_port, studio_port)?;
    let endpoint = Url::parse(&format!("http://127.0.0.1:{rpc_port}"))?;
    let transport = HttpTransport::new(&HttpConfig::default())?;
    let client = SurfpoolClient::new(endpoint, Arc::clone(&transport));
    wait_until_ready(&mut process, &client).await?;

    let payer = Keypair::new();
    let recipient = Keypair::new().pubkey();
    client.set_lamports(&payer.pubkey(), 2_000_000).await?;
    assert_eq!(client.rpc.get_balance(&payer.pubkey()).await?, 2_000_000);

    let blockhash = client.rpc.get_latest_blockhash().await?;
    let transaction = Transaction::new_signed_with_payer(
        &[transfer(&payer.pubkey(), &recipient, TRANSFER_LAMPORTS)],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );
    let signature = client.send(&transaction).await?;

    let started = Instant::now();
    loop {
        if client.rpc.get_balance(&recipient).await? == TRANSFER_LAMPORTS {
            break;
        }
        let statuses = client.rpc.get_signature_statuses(&[signature]).await?;
        if let Some(error) = statuses
            .value
            .first()
            .and_then(Option::as_ref)
            .and_then(|status| status.err.as_ref())
        {
            return Err(format!("transaction {signature} failed: {error}").into());
        }
        if started.elapsed() >= EXECUTION_TIMEOUT {
            return Err(format!("transaction {signature} did not update recipient balance").into());
        }
        sleep(Duration::from_millis(50)).await;
    }

    Ok(())
}
