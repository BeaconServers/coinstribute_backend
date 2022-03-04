mod daemon_rpc;
mod wallet_rpc;

use std::time::Duration;

use daemon_rpc::{DaemonRPC, DaemonRPCError};
use monero::Network;
use wallet_rpc::{WalletRPC, WalletRPCError};

pub use monero::util::address::{Address, PaymentId};
pub use wallet_rpc::{Transfers, TransferOut, WalletBalance};

pub struct Wallet {
    wallet_rpc: WalletRPC,
    daemon_rpc: DaemonRPC,
    addr: Address,

}

impl Wallet {
    /// Creates a new Wallet configured in a sepcific way
    pub async fn new(wallet_url: String, daemon_url: String, connection_timeout: Option<Duration>, send_timeout: Option<Duration>) -> Self {
        let wallet_rpc = WalletRPC::new(wallet_url, connection_timeout, send_timeout);
        let daemon_rpc = DaemonRPC::new(daemon_url, connection_timeout, send_timeout);

        let mut addr: Option<Address> = None;

        while addr == None {
            addr = wallet_rpc.get_address().await.ok();
            
        }

        Self {
            wallet_rpc,
            daemon_rpc,
            addr: addr.unwrap(),
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

    /// Send X Monero to an address
	pub async fn transfer(&self, priority: Option<u8>, dst: Address, amt: u64, addr_index: u64) -> Result<TransferOut, WalletRPCError> {
        Ok(self.wallet_rpc.transfer(priority, dst, amt, addr_index).await?)
    }

    /// Get the current total balance of this monero wallet (including all user balances)
    pub async fn get_balance(&self, acc_indices: &[u64]) -> Result<WalletBalance, WalletError> {
        Ok(self.wallet_rpc.get_balance(acc_indices).await?)

    }

    pub async fn get_transfers(&self, acc_indices: &[u64], transfers_in: bool, transfers_out: bool) -> Result<Vec<Transfers>, WalletError> {
        Ok(self.wallet_rpc.get_transfers(acc_indices, transfers_in, transfers_out).await?)

    }

    /// Get the current block height of the monero daemon
    pub async fn get_height(&self) -> Result<u64, WalletError> {
        Ok(self.daemon_rpc.daemon_height().await?)

    }
    
    /// Get an estiimation of the current Monero network fee per byte
    pub async fn get_fee(&self) -> Result<u64, WalletError> {
        Ok(self.daemon_rpc.get_fee().await?)
    }

    /// Sets the daemon url
    pub async fn set_daemon(&self, daemon_url: &str, trusted: bool) -> Result<(), WalletError> {
        Ok(self.wallet_rpc.set_daemon(daemon_url, trusted).await?)
    }

    // The amount of time in between refreshes
    pub async fn set_refresh_time(&self, time: u64) -> Result<(), WalletError> {
        Ok(self.wallet_rpc.set_refresh_time(time).await?)
    }

    /// Returns the new address and it's subaddress index
    pub async fn create_address(&self, username: &str) -> Result<(Address, u64), WalletError> {
        Ok(self.wallet_rpc.create_address(username).await?)

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
