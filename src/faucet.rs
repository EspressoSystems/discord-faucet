// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the Discord Faucet library.
//
// You should have received a copy of the MIT License
// along with the Discord Faucet library. If not, see <https://mit-license.org/>.

use anyhow::{Error, Result};
use async_std::{
    channel::Receiver,
    sync::{RwLock, RwLockUpgradableReadGuard},
    task::{sleep, JoinHandle},
};
use clap::Parser;
use ethers::{
    prelude::SignerMiddleware,
    providers::{Http, Middleware as _, Provider, StreamExt, Ws},
    signers::{coins_bip39::English, LocalWallet, MnemonicBuilder, Signer},
    types::{
        Address, BlockId, Transaction, TransactionReceipt, TransactionRequest, H256, U256, U512,
    },
    utils::{parse_ether, ConversionError},
};
use std::{
    collections::{BinaryHeap, HashMap, VecDeque},
    num::ParseIntError,
    ops::Index,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use url::Url;

pub type Middleware = SignerMiddleware<Provider<Http>, LocalWallet>;

pub(crate) const TEST_MNEMONIC: &str =
    "test test test test test test test test test test test junk";

#[derive(Parser, Debug, Clone)]
pub struct Options {
    /// Number of Ethereum accounts to use for the faucet.
    ///
    /// This is the number of faucet grant requests that can be executed in
    /// parallel. Each client can only do about one request per block_time
    /// (which is 12 seconds for public Ethereum networks.)
    ///
    /// When initially setting and increasing the number of wallets the faucet
    /// will make sure they are all funded before serving any faucet requests.
    /// However when reducing the number of wallets the faucet will not collect
    /// the funds in the wallets that are no longer used.
    #[arg(
        long,
        env = "ESPRESSO_DISCORD_FAUCET_NUM_CLIENTS",
        default_value = "10"
    )]
    pub num_clients: usize,

    /// The mnemonic of the faucet wallet.
    #[arg(long, env = "ESPRESSO_DISCORD_FAUCET_MNEMONIC")]
    pub mnemonic: String,

    /// The index in the HD key derivation tree derived from mnemonic of the first account to use
    /// for faucet transfers.
    ///
    /// Subsequent accounts, if requested, will be derived from consecutively increasing account
    /// indices.
    #[arg(
        long,
        env = "ESPRESSO_DISCORD_FAUCET_FIRST_ACCOUNT_INDEX",
        default_value = "0"
    )]
    pub first_account_index: u32,

    /// Port on which to serve the API.
    #[arg(
        short,
        long,
        env = "ESPRESSO_DISCORD_FAUCET_PORT",
        default_value = "8111"
    )]
    pub port: u16,

    /// The amount of funds to grant to each account on startup in Ethers.
    #[arg(
        long,
        env = "ESPRESSO_DISCORD_FAUCET_GRANT_AMOUNT_ETHERS",
        value_parser = |arg: &str| -> Result<U256, ConversionError> { Ok(parse_ether(arg)?) },
        default_value = "100",
    )]
    pub faucet_grant_amount: U256,

    /// The time after which a transfer is considered timed out and will be re-sent
    #[arg(
        long,
        env = "ESPRESSO_DISCORD_FAUCET_TRANSACTION_TIMEOUT_SECS",
        default_value = "300",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> { Ok(Duration::from_secs(arg.parse::<u64>()?)) }
    )]
    pub transaction_timeout: Duration,

    /// The URL of the WebSockets JsonRPC the faucet connects to.
    ///
    /// If provided, the faucet will use this endpoint for monitoring transactions and streaming
    /// receipts. If not provided, it will fall back to polling provider-url-http, which can be less
    /// efficient.
    #[arg(long, env = "ESPRESSO_DISCORD_FAUCET_WEB3_PROVIDER_URL_WS")]
    pub provider_url_ws: Option<Url>,

    /// The URL of the JsonRPC the faucet connects to.
    #[arg(long, env = "ESPRESSO_DISCORD_FAUCET_WEB3_PROVIDER_URL_HTTP")]
    pub provider_url_http: Url,

    /// The authentication token for the discord bot.
    #[arg(long, env = "ESPRESSO_DISCORD_FAUCET_DISCORD_TOKEN")]
    pub discord_token: Option<String>,

    /// The polling interval for HTTP subscriptions to the RPC provider.
    #[arg(
        long,
        env = "ESPRESSO_DISCORD_FAUCET_POLL_INTERVAL",
        default_value = "7s",
        value_parser = duration_str::parse,
    )]
    pub poll_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        // Supply explicit default arguments for the required command line arguments. For everything
        // else just use the default value specified via clap.
        Self::parse_from([
            "--",
            "--mnemonic",
            TEST_MNEMONIC,
            "--provider-url-ws",
            "ws://localhost:8545",
            "--provider-url-http",
            "http://localhost:8545",
        ])
    }
}

impl Options {
    /// Returns the minimum balance required to consider a client funded.
    ///
    /// Set to 2 times the faucet grant amount to be on the safe side regarding gas.
    fn min_funding_balance(&self) -> U256 {
        self.faucet_grant_amount * 2
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TransferRequest {
    Faucet {
        to: Address,
        amount: U256,
    },
    Funding {
        to: Address,
        average_wallet_balance: U256,
    },
}

impl TransferRequest {
    pub fn faucet(to: Address, amount: U256) -> Self {
        Self::Faucet { to, amount }
    }

    pub fn funding(to: Address, average_wallet_balance: U256) -> Self {
        Self::Funding {
            to,
            average_wallet_balance,
        }
    }

    pub fn to(&self) -> Address {
        match self {
            Self::Faucet { to, .. } => *to,
            Self::Funding { to, .. } => *to,
        }
    }

    pub fn required_funds(&self) -> U256 {
        match self {
            // Double the faucet amount to be on the safe side regarding gas.
            Self::Faucet { amount, .. } => *amount * 2,
            Self::Funding {
                average_wallet_balance,
                ..
            } => *average_wallet_balance,
        }
    }
}

#[derive(Debug, Clone)]
struct Transfer {
    sender: Arc<Middleware>,
    request: TransferRequest,
    timestamp: Instant,
}

impl Transfer {
    pub fn new(sender: Arc<Middleware>, request: TransferRequest) -> Self {
        Self {
            sender,
            request,
            timestamp: Instant::now(),
        }
    }
}

#[derive(Clone, Debug, Error)]
pub enum TransferError {
    #[error("Error during transfer submission: {transfer:?} {sender:?} {msg}")]
    RpcSubmitError {
        transfer: TransferRequest,
        sender: Address,
        msg: String,
    },
    #[error("No client available")]
    NoClient,
    #[error("No transfers requests available")]
    NoRequests,
}

#[derive(Debug, Clone, Default)]
struct ClientPool {
    clients: HashMap<Address, Arc<Middleware>>,
    priority: BinaryHeap<(U256, Address)>,
}

impl ClientPool {
    pub fn pop(&mut self) -> Option<(U256, Arc<Middleware>)> {
        let (balance, address) = self.priority.pop()?;
        let client = self.clients.remove(&address)?;
        Some((balance, client))
    }

    pub fn push(&mut self, balance: U256, client: Arc<Middleware>) {
        self.clients.insert(client.address(), client.clone());
        self.priority.push((balance, client.address()));
    }

    pub fn has_client_for(&self, transfer: TransferRequest) -> bool {
        self.priority
            .peek()
            .map_or(false, |(balance, _)| *balance >= transfer.required_funds())
    }
}

#[derive(Debug, Clone, Default)]
struct State {
    clients: ClientPool,
    inflight: HashMap<H256, Transfer>,
    clients_being_funded: HashMap<Address, Arc<Middleware>>,
    // Funding wallets has priority, these transfer requests must be pushed to
    // the front.
    transfer_queue: VecDeque<TransferRequest>,
    monitoring_started: bool,
}

#[derive(Debug, Clone)]
pub struct Faucet {
    config: Options,
    state: Arc<RwLock<State>>,
    /// Used to monitor Ethereum transactions.
    provider: Provider<Http>,
    ws_provider: Option<Provider<Ws>>,
    /// Channel to receive faucet requests.
    faucet_receiver: Arc<RwLock<Receiver<Address>>>,
}

impl Faucet {
    /// Create a new faucet.
    ///
    /// Creates `num_clients` wallets and transfers funds and queues transfers
    /// from the ones with most balance to the ones with less than average
    /// balance.
    pub async fn create(options: Options, faucet_receiver: Receiver<Address>) -> Result<Self> {
        // Use a http provider for non-subscribe requests
        let provider = Provider::<Http>::try_from(options.provider_url_http.to_string())?
            .interval(options.poll_interval);
        let chain_id = provider.get_chainid().await?.as_u64();

        let mut state = State::default();
        let mut clients = vec![];

        // We want each account to have a minimum value that is at least 80% of the average value.
        // For this computation, convert into U512 to avoid overflow while adding up the total
        // balance or multiplying to compute 80%.
        let mut total_balance = U512::zero();

        // Create clients
        for index in 0..options.num_clients {
            let wallet = MnemonicBuilder::<English>::default()
                .phrase(options.mnemonic.as_str())
                .index(options.first_account_index + (index as u32))?
                .build()?
                .with_chain_id(chain_id);
            let client = Arc::new(Middleware::new(provider.clone(), wallet));

            // On startup we may get a "[-32000] failed to get the last block
            // number from state" error even after the request for getChainId is
            // successful.
            let balance = loop {
                if let Ok(balance) = provider.get_balance(client.address(), None).await {
                    break balance;
                }
                tracing::info!("Failed to get balance for client, retrying...");
                async_std::task::sleep(Duration::from_secs(1)).await;
            };

            tracing::info!(
                "Created client {index} {:?} with balance {balance}",
                client.address(),
            );

            total_balance += balance.into();
            clients.push((balance, client));
        }

        let desired_balance = std::cmp::max(
            total_balance / options.num_clients * 8 / 10,
            options.min_funding_balance().into(),
        );
        // At this point, `desired_balance` is less than the average of all the clients' balances,
        // each of which was a `U256`, so we can safely cast back into a `U256`.
        let desired_balance =
            U256::try_from(desired_balance).expect("average balance overflows U256");

        for (balance, client) in clients {
            // Fund all clients who have significantly less than average balance.
            if balance < desired_balance {
                tracing::info!("Queuing funding transfer for {:?}", client.address());
                let transfer = TransferRequest::funding(client.address(), desired_balance);
                state.transfer_queue.push_back(transfer);
                state.clients_being_funded.insert(client.address(), client);
            } else {
                state.clients.push(balance, client);
            }
        }

        let ws_provider = match &options.provider_url_ws {
            Some(url) => Some(Provider::<Ws>::connect(url.clone()).await?),
            None => None,
        };

        Ok(Self {
            config: options,
            state: Arc::new(RwLock::new(state)),
            provider,
            ws_provider,
            faucet_receiver: Arc::new(RwLock::new(faucet_receiver)),
        })
    }

    pub async fn start(
        self,
    ) -> JoinHandle<(
        Result<(), Error>,
        Result<(), Error>,
        Result<(), Error>,
        Result<(), Error>,
    )> {
        let futures = async move {
            futures::join!(
                self.monitor_transactions(),
                self.monitor_faucet_requests(),
                self.monitor_transaction_timeouts(),
                self.execute_transfers_loop()
            )
        };
        async_std::task::spawn(futures)
    }

    async fn balance(&self, address: Address) -> Result<U256> {
        Ok(self.provider.get_balance(address, None).await?)
    }

    async fn request_transfer(&self, transfer: TransferRequest) {
        tracing::info!("Adding transfer to queue: {:?}", transfer);
        self.state.write().await.transfer_queue.push_back(transfer);
    }

    async fn execute_transfers_loop(&self) -> Result<()> {
        loop {
            if self.state.read().await.monitoring_started {
                break;
            } else {
                tracing::info!("Waiting for transaction monitoring to start...");
                async_std::task::sleep(Duration::from_secs(1)).await;
            }
        }
        loop {
            if let Err(err) = self.execute_transfer().await {
                match err {
                    TransferError::RpcSubmitError { .. } => {
                        tracing::error!("Failed to execute transfer: {:?}", err)
                    }
                    TransferError::NoClient => {
                        tracing::info!("No clients to handle transfer requests.")
                    }
                    TransferError::NoRequests => {}
                };
                // Avoid creating a busy loop.
                async_std::task::sleep(Duration::from_secs(1)).await;
            };
        }
    }

    async fn execute_transfer(&self) -> Result<H256, TransferError> {
        let mut state = self.state.write().await;
        if state.transfer_queue.is_empty() {
            Err(TransferError::NoRequests)?;
        }
        let transfer = state.transfer_queue.index(0);
        if !state.clients.has_client_for(*transfer) {
            Err(TransferError::NoClient)?;
        }
        let (balance, sender) = state.clients.pop().unwrap();
        let transfer = state.transfer_queue.pop_front().unwrap();

        // Drop the guard while we are doing the request to the RPC.
        drop(state);

        let amount = match transfer {
            TransferRequest::Faucet { amount, .. } => amount,
            TransferRequest::Funding { .. } => balance / 2,
        };
        match sender
            .clone()
            .send_transaction(TransactionRequest::pay(transfer.to(), amount), None)
            .await
        {
            Ok(tx) => {
                tracing::info!("Sending transfer: {:?} hash={:?}", transfer, tx.tx_hash());
                // Note: if running against an *extremely* fast chain , it is possible
                // that the transaction is mined before we have a chance to add it to
                // the inflight transfers. In that case, the receipt handler may not yet
                // find the transaction and fail to process it correctly. I think the
                // risk of this happening outside of local testing is neglible. We could
                // sign the tx locally first and then insert it but this also means we
                // would have to remove it again if the submission fails.
                self.state
                    .write()
                    .await
                    .inflight
                    .insert(tx.tx_hash(), Transfer::new(sender.clone(), transfer));
                Ok(tx.tx_hash())
            }
            Err(err) => {
                // Make the client available again.
                self.state
                    .write()
                    .await
                    .clients
                    .push(balance, sender.clone());

                // Requeue the transfer.
                self.request_transfer(transfer).await;

                Err(TransferError::RpcSubmitError {
                    transfer,
                    sender: sender.address(),
                    msg: err.to_string(),
                })?
            }
        }
    }

    /// Handle external incoming transfers to faucet accounts
    async fn handle_non_faucet_transfer(&self, receipt: &TransactionReceipt) -> Result<()> {
        tracing::debug!("Handling external incoming transfer to {:?}", receipt.to);
        if let Some(receiver) = receipt.to {
            let state = self.state.upgradable_read().await;
            if state.clients_being_funded.contains_key(&receiver) {
                let balance = self.balance(receiver).await?;
                if balance >= self.config.min_funding_balance() {
                    tracing::info!("Funded client {:?} with external transfer", receiver);
                    let mut state = RwLockUpgradableReadGuard::upgrade(state).await;
                    if let Some(transfer_index) =
                        state.transfer_queue.iter().position(|r| r.to() == receiver)
                    {
                        tracing::info!("Removing funding request from queue");
                        state.transfer_queue.remove(transfer_index);
                    } else {
                        tracing::warn!("Funding request not found in queue");
                    }
                    tracing::info!("Making client {receiver:?} available");
                    let client = state.clients_being_funded.remove(&receiver).unwrap();
                    state.clients.push(balance, client);
                } else {
                    tracing::warn!(
                        "Balance for client {receiver:?} {balance:?} too low to make it available"
                    );
                }
            } else {
                tracing::debug!("Irrelevant transaction {:?}", receipt.transaction_hash);
            }
        }
        Ok(())
    }

    async fn handle_tx(&self, tx: Transaction) -> Result<()> {
        let tx_hash = tx.hash();
        tracing::debug!("Got tx hash {:?}", tx_hash);

        // Using `cloned` here to avoid borrow
        let state = self.state.read().await;
        let inflight = state.inflight.get(&tx_hash).cloned();

        // Only continue if there's an inflight transfer or the recipient is a client being funded.
        let is_relevant = inflight.is_some()
            || tx
                .to
                .as_ref()
                .is_some_and(|to| state.clients_being_funded.contains_key(to));

        drop(state);

        if !is_relevant {
            return Ok(());
        }

        // In case there is a race condition and the receipt is not yet available, wait for it.
        let receipt = loop {
            if let Ok(Some(tx)) = self.provider.get_transaction_receipt(tx_hash).await {
                break tx;
            }
            tracing::warn!("No receipt for tx_hash={tx_hash:?}, will retry");
            async_std::task::sleep(Duration::from_secs(1)).await;
        };

        tracing::debug!("Got receipt {:?}", receipt);

        let Some(Transfer {
            sender, request, ..
        }) = inflight
        else {
            return self.handle_non_faucet_transfer(&receipt).await;
        };

        tracing::info!("Received receipt for {request:?}");
        // Do all external calls before state modifications
        let new_sender_balance = self.balance(sender.address()).await?;

        // For successful funding transfers, we also need to update the receiver's balance.
        let receiver_update = if receipt.status == Some(1.into()) {
            if let TransferRequest::Funding { to: receiver, .. } = request {
                Some((receiver, self.balance(receiver).await?))
            } else {
                None
            }
        } else {
            None
        };

        // Update state, the rest of the operations must be atomic.
        let mut state = self.state.write().await;

        // Make the sender available
        state.clients.push(new_sender_balance, sender.clone());

        // Apply the receiver update, if there is one.
        if let Some((receiver, balance)) = receiver_update {
            if let Some(client) = state.clients_being_funded.remove(&receiver) {
                tracing::info!("Funded client {:?} with {:?}", receiver, balance);
                state.clients.push(balance, client);
            } else {
                tracing::warn!(
                    "Received funding transfer for unknown client {:?}",
                    receiver
                );
            }
        }

        // If the transaction failed, schedule it again.
        if receipt.status == Some(0.into()) {
            // TODO: this code is currently untested.
            tracing::warn!(
                "Transfer failed tx_hash={:?}, will resend: {:?}",
                tx_hash,
                request
            );
            state.transfer_queue.push_back(request);
        };

        // Finally remove the transaction from the inflight list.
        state.inflight.remove(&tx_hash);

        // TODO: I think for transactions with bad nonces we would not even get
        // a transactions receipt. As a result the sending client would remain
        // stuck. As a workaround we could add a timeout to the inflight clients
        // and unlock them after a while. It may be difficult to set a good
        // fixed value for the timeout because the zkevm-node currently waits
        // for hotshot blocks being sequenced in the contract.

        Ok(())
    }

    async fn monitor_transactions(&self) -> Result<()> {
        loop {
            let mut stream = match &self.ws_provider {
                Some(provider) => match provider.subscribe_blocks().await {
                    Ok(stream) => stream
                        .filter_map(|block| async move {
                            if block.hash.is_none() {
                                tracing::warn!("Received block without hash, ignoring: {block:?}");
                            }
                            block.hash
                        })
                        .boxed(),
                    Err(err) => {
                        tracing::error!("Error reconnecting to block stream: {err}");
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                },
                None => match self.provider.watch_blocks().await {
                    Ok(stream) => stream.boxed(),
                    Err(err) => {
                        tracing::error!("Error reconnecting to block stream: {err}");
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                },
            };

            self.state.write().await.monitoring_started = true;
            tracing::info!("Transaction monitoring started ...");

            while let Some(hash) = stream.next().await {
                if let Some(block) = self
                    .provider
                    .get_block_with_txs(BlockId::from(hash))
                    .await?
                {
                    for tx in block.transactions.iter() {
                        self.handle_tx(tx.clone()).await?;
                    }
                } else {
                    // `provider.get_block_with_txs` is allowed to return `None` if it cannot
                    // find a block with the requested hash. Since we only ever request
                    // block hashes that have just been confirmed by `watch_blocks`, the
                    // only way a block can possibly be missing is if there was an L2
                    // reorg. This is rare but possible. In this case, since the block
                    // we were fetching has been re-orged out, we can just ignore it.
                    tracing::error!(
                        "received hash {hash} from watch_blocks, but block was missing"
                    );
                }
            }

            // If we get here, the subscription was closed. This happens for example
            // if the RPC server is restarted.
            tracing::warn!("Block subscription closed, will restart ...");
            sleep(Duration::from_secs(5)).await;
        }
    }

    async fn monitor_faucet_requests(&self) -> Result<()> {
        loop {
            if let Ok(address) = self.faucet_receiver.write().await.recv().await {
                self.request_transfer(TransferRequest::faucet(
                    address,
                    self.config.faucet_grant_amount,
                ))
                .await;
            }
        }
    }

    async fn monitor_transaction_timeouts(&self) -> Result<()> {
        loop {
            async_std::task::sleep(Duration::from_secs(60)).await;
            self.process_transaction_timeouts().await?;
        }
    }

    async fn process_transaction_timeouts(&self) -> Result<()> {
        tracing::info!("Processing transaction timeouts");
        let inflight = self.state.read().await.inflight.clone();

        for (
            tx_hash,
            Transfer {
                sender, request, ..
            },
        ) in inflight
            .iter()
            .filter(|(_, transfer)| transfer.timestamp.elapsed() > self.config.transaction_timeout)
        {
            tracing::warn!("Transfer timed out: {:?}", request);
            let balance = self.balance(sender.address()).await?;
            let mut state = self.state.write().await;
            state.transfer_queue.push_back(*request);
            state.inflight.remove(tx_hash);
            state.clients.push(balance, sender.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use async_compatibility_layer::logging::{setup_backtrace, setup_logging};
    use sequencer_utils::AnvilOptions;

    #[async_std::test]
    async fn test_faucet_inflight_timeouts_ws() -> Result<()> {
        test_faucet_inflight_timeouts(true).await
    }

    #[async_std::test]
    async fn test_faucet_inflight_timeouts_http() -> Result<()> {
        test_faucet_inflight_timeouts(false).await
    }

    async fn test_faucet_inflight_timeouts(ws: bool) -> Result<()> {
        setup_logging();
        setup_backtrace();

        let anvil = AnvilOptions::default()
            .block_time(Duration::from_secs(3600))
            .spawn()
            .await;

        let provider_url_ws = if ws {
            let mut ws_url = anvil.url();
            ws_url.set_scheme("ws").unwrap();
            Some(ws_url)
        } else {
            None
        };

        let options = Options {
            num_clients: 1,
            provider_url_ws,
            provider_url_http: anvil.url(),
            transaction_timeout: Duration::from_secs(0),
            ..Default::default()
        };

        let (_, receiver) = async_std::channel::unbounded();
        let faucet = Faucet::create(options.clone(), receiver).await?;

        // Manually execute a transfer.
        let transfer = TransferRequest::faucet(Address::zero(), options.faucet_grant_amount);
        faucet.request_transfer(transfer).await;
        faucet.execute_transfer().await?;

        // Assert that there is an inflight transaction.
        assert!(!faucet.state.read().await.inflight.is_empty());

        // Process the timed out transaction.
        faucet.process_transaction_timeouts().await?;
        assert!(faucet.state.read().await.inflight.is_empty());

        // Assert that the client is available again.
        faucet.state.write().await.clients.pop().unwrap();

        // Assert that the transaction was not executed.
        assert_eq!(faucet.balance(Address::zero()).await?, 0.into());

        Ok(())
    }

    #[async_std::test]
    async fn test_faucet_funding_ws() -> Result<()> {
        test_faucet_funding(true).await
    }

    #[async_std::test]
    async fn test_faucet_funding_http() -> Result<()> {
        test_faucet_funding(false).await
    }

    // A regression test for a bug where clients that received funding transfers
    // were not made available.
    async fn test_faucet_funding(ws: bool) -> Result<()> {
        setup_logging();
        setup_backtrace();

        let anvil = AnvilOptions::default().spawn().await;

        let provider_url_ws = if ws {
            let mut ws_url = anvil.url();
            ws_url.set_scheme("ws").unwrap();
            Some(ws_url)
        } else {
            None
        };
        let options = Options {
            // 10 clients are already funded with anvil
            num_clients: 11,
            provider_url_ws,
            provider_url_http: anvil.url(),
            ..Default::default()
        };

        let (_, receiver) = async_std::channel::unbounded();
        let faucet = Faucet::create(options.clone(), receiver).await?;

        // There is one client that needs funding.
        assert_eq!(faucet.state.read().await.clients_being_funded.len(), 1);

        let tx_hash = faucet.execute_transfer().await?;
        let tx = faucet.provider.get_transaction(tx_hash).await?.unwrap();
        faucet.handle_tx(tx).await?;

        let mut state = faucet.state.write().await;
        // The newly funded client is now funded.
        assert_eq!(state.clients_being_funded.len(), 0);
        assert_eq!(state.clients.clients.len(), 11);

        // All clients now have a non-zero balance.
        while let Some((balance, _)) = state.clients.pop() {
            assert!(balance > 0.into());
        }

        Ok(())
    }
}
