mod daemon_rpc;
mod wallet_rpc;

use std::time::Duration;

use daemon_rpc::{DaemonRPC, DaemonRPCError};
use monero::Network;
use wallet_rpc::{WalletRPC, WalletRPCError};
use futures::try_join;

pub use monero::util::address::{Address, PaymentId};
pub use wallet_rpc::Payment;

pub struct Wallet {
    wallet_rpc: WalletRPC,
    daemon_rpc: DaemonRPC,
    addr: Address,

}

impl Wallet {
    pub async fn new(wallet_url: String, daemon_url: String, connection_timeout: Option<Duration>, send_timeout: Option<Duration>) -> Self {
        // Local daemon connections are trusted, everything else is not
        let trusted_daemon = daemon_url.contains("127.0.0.1");

        let wallet_rpc = WalletRPC::new(wallet_url, connection_timeout, send_timeout);
        let daemon_rpc = DaemonRPC::new(daemon_url, connection_timeout, send_timeout);

        let addr = wallet_rpc.get_address().await.unwrap();

        Self {
            wallet_rpc,
            daemon_rpc,
            addr,
        }
    }

    /// Generates a new payment address given a PaymentID
    pub fn new_integrated_addr(&self, id: PaymentId) -> Address {
        Address::integrated(Network::Mainnet, self.addr.public_spend, self.addr.public_view, id)
        
    }

    /// The address currently being used by the wallet
    pub fn address(&self) -> &Address {
        &self.addr

    }

    /// Get the current total balance of this monero wallet (including all user balances)
    pub async fn get_balance(&self) -> Result<u64, WalletError> {
        Ok(self.wallet_rpc.get_balance().await?)

    }

    /// Get the current block height of the monero daemon
    pub async fn get_height(&self) -> Result<u64, WalletError> {
        Ok(self.daemon_rpc.daemon_height().await?)

    }

    pub async fn get_payments(&self, payment_id: PaymentId) -> Result<Vec<Payment>, WalletError> {
        Ok(self.wallet_rpc.get_payments(payment_id).await?)
    }

    pub async fn get_fee(&self) -> Result<u64, WalletError> {
        Ok(self.daemon_rpc.get_fee().await?)
    }

    pub async fn set_daemon(&self, daemon_url: &str, trusted: bool) -> Result<(), WalletError> {
        Ok(self.wallet_rpc.set_daemon(daemon_url, trusted).await?)
    }

    pub async fn set_refresh_time(&self, time: u64) -> Result<(), WalletError> {
        Ok(self.wallet_rpc.set_refresh_time(time).await?)
    }
}

#[derive(Debug)]
pub enum WalletError {
    WalletRPC(WalletRPCError),
    DaemonRPC(DaemonRPCError),
}

impl From<WalletRPCError> for WalletError {
    fn from(e: WalletRPCError) -> Self {
        Self::WalletRPC(e)

    }
}

impl From<DaemonRPCError> for WalletError {
    fn from(e: DaemonRPCError) -> Self {
        Self::DaemonRPC(e)

    }
}
