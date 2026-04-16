use anchor_client::{Client, Cluster, CommitmentConfig};
use solana_keypair::Keypair;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;

use crate::rpc;

fn create_client(
    private_key: &str,
    rpc_and_ws_url: Option<(String, String)>,
) -> Client<Arc<Keypair>> {
    let signer = Keypair::from_base58_string(&private_key);
    let signer = Arc::new(signer);
    let cluster = match rpc_and_ws_url {
        Some((rpc_url, ws_url)) => Cluster::Custom(rpc_url, ws_url),
        None => Cluster::Mainnet,
    };

    let client = Client::new_with_options(cluster, signer, CommitmentConfig::confirmed());
    client
}

fn create_rpc_client(rpc_url: Option<String>) -> RpcClient {
    let rpc_url = rpc_url.unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed())
}
