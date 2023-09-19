// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the Discord Faucet library.
//
// You should have received a copy of the MIT License
// along with the Discord Faucet library. If not, see <https://mit-license.org/>.

//! Web server for the discord faucet.
//!
//! Serves these purposes:
//! 1. Provide a healthcheck endpoint for the discord bot, so it can be automatically
//!    restarted if it fails.
//! 2. Test and use the faucet locally without connecting to Discord.
use async_std::channel::Sender;
use async_std::sync::RwLock;
use ethers::types::Address;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::env;
use std::io;
use thiserror::Error;
use tide_disco::RequestError;
use tide_disco::{http::StatusCode, Api, App, Error};

#[derive(Clone, Debug, Deserialize, Serialize, Error)]
pub enum FaucetError {
    #[error("faucet error {status}: {msg}")]
    FaucetError { status: StatusCode, msg: String },
    #[error("unable to parse Ethereum address: {input}")]
    BadAddress { status: StatusCode, input: String },
}

impl tide_disco::Error for FaucetError {
    fn catch_all(status: StatusCode, msg: String) -> Self {
        Self::FaucetError { status, msg }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::FaucetError { status, .. } => *status,
            Self::BadAddress { status, .. } => *status,
        }
    }
}

impl From<RequestError> for FaucetError {
    fn from(err: RequestError) -> Self {
        Self::catch_all(StatusCode::BadRequest, err.to_string())
    }
}

pub(crate) async fn serve(port: u16, state: WebState) -> io::Result<()> {
    let mut app = App::<_, FaucetError>::with_state(RwLock::new(state));
    app.with_version(env!("CARGO_PKG_VERSION").parse().unwrap());

    // Include API specification in binary
    let toml = toml::from_str::<toml::value::Value>(include_str!("api.toml"))
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;

    let mut api = Api::<RwLock<WebState>, FaucetError>::new(toml).unwrap();
    api.with_version(env!("CARGO_PKG_VERSION").parse().unwrap());

    // Can invoke with
    //    `curl -i -X POST http://0.0.0.0:8111/faucet/request/0x1234567890123456789012345678901234567890`
    api.post("request", |req, state| {
        async move {
            let address = req.string_param("address")?;
            let address = address.parse().map_err(|_| FaucetError::BadAddress {
                status: StatusCode::BadRequest,
                input: address.to_string(),
            })?;
            tracing::info!("Received faucet request for {:?}", address);
            state.request(address).await?;
            Ok(())
        }
        .boxed()
    })
    .unwrap();

    app.register_module("faucet", api).unwrap();
    app.serve(format!("0.0.0.0:{}", port)).await
}

#[derive(Clone, Debug)]
pub(crate) struct WebState {
    faucet_queue: Sender<Address>,
}

impl WebState {
    pub fn new(faucet_queue: Sender<Address>) -> Self {
        Self { faucet_queue }
    }

    pub async fn request(&self, address: Address) -> Result<(), FaucetError> {
        self.faucet_queue
            .send(address)
            .await
            .map_err(|err| FaucetError::FaucetError {
                status: StatusCode::InternalServerError,
                msg: err.to_string(),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::faucet::{Faucet, Middleware, Options, TEST_MNEMONIC};
    use anyhow::Result;
    use async_compatibility_layer::logging::{setup_backtrace, setup_logging};
    use async_std::task::spawn;
    use ethers::{
        providers::{Http, Middleware as _, Provider},
        signers::{coins_bip39::English, MnemonicBuilder, Signer},
        types::{TransactionRequest, U256},
        utils::parse_ether,
    };
    use sequencer_utils::AnvilOptions;
    use std::{sync::Arc, time::Duration};
    use surf_disco::Client;

    async fn run_faucet_test(options: Options, num_transfers: usize) -> Result<()> {
        let client =
            Client::<FaucetError>::new(format!("http://localhost:{}", options.port).parse()?);
        // Avoids waiting 10 seconds for the retry in `connect`.
        async_std::task::sleep(Duration::from_millis(100)).await;
        client.connect(None).await;

        let recipient = Address::random();
        let mut total_transfer_amount = U256::zero();

        for _ in 0..num_transfers {
            client
                .post(&format!("faucet/request/{recipient:?}"))
                .send()
                .await?;

            total_transfer_amount += options.faucet_grant_amount;
        }

        let provider = Provider::<Http>::try_from(options.provider_url_http.to_string())?;
        loop {
            let balance = provider.get_balance(recipient, None).await.unwrap();
            tracing::info!("Balance is {balance}");
            if balance == total_transfer_amount {
                break;
            }
            async_std::task::sleep(Duration::from_secs(1)).await;
        }

        Ok(())
    }

    #[async_std::test]
    async fn test_faucet_anvil() -> Result<()> {
        setup_logging();
        setup_backtrace();

        let anvil = AnvilOptions::default().spawn().await;

        let mut ws_url = anvil.url();
        ws_url.set_scheme("ws").unwrap();

        // With anvil 10 clients are pre-funded. We use more than that to make
        // sure the funding logic runs.
        let options = Options {
            num_clients: 12,
            faucet_grant_amount: parse_ether(1).unwrap(),
            provider_url_ws: ws_url,
            provider_url_http: anvil.url(),
            port: portpicker::pick_unused_port().unwrap(),
            ..Default::default()
        };

        let (sender, receiver) = async_std::channel::unbounded();

        // Start the faucet
        let faucet = Faucet::create(options.clone(), receiver).await?;
        let _handle = faucet.start().await;

        // Start the web server
        spawn(async move { serve(options.port, WebState::new(sender)).await });

        run_faucet_test(options, 30).await?;
        Ok(())
    }

    #[async_std::test]
    async fn test_node_restart() -> Result<()> {
        setup_logging();
        setup_backtrace();

        let anvil_opts = AnvilOptions::default();
        let mut anvil = anvil_opts.clone().spawn().await;

        let mut ws_url = anvil.url();
        ws_url.set_scheme("ws").unwrap();

        // With anvil 10 clients are pre-funded. We use more than that to make
        // sure the funding logic runs.
        let options = Options {
            num_clients: 12,
            faucet_grant_amount: parse_ether(1).unwrap(),
            provider_url_ws: ws_url,
            provider_url_http: anvil.url(),
            port: portpicker::pick_unused_port().unwrap(),
            ..Default::default()
        };

        let (sender, receiver) = async_std::channel::unbounded();

        // Start the faucet
        let faucet = Faucet::create(options.clone(), receiver).await?;
        let _handle = faucet.start().await;

        // Start the web server
        spawn(async move { serve(options.port, WebState::new(sender)).await });

        run_faucet_test(options.clone(), 3).await?;

        tracing::info!("Restarting anvil to trigger web socket reconnect");
        anvil.restart(anvil_opts).await;

        run_faucet_test(options, 3).await?;

        Ok(())
    }

    // A test to verify that the faucet functions if it's funded only after startup.
    #[async_std::test]
    async fn test_unfunded_faucet() -> Result<()> {
        setup_logging();
        setup_backtrace();

        let anvil_opts = AnvilOptions::default();
        let anvil = anvil_opts.clone().spawn().await;

        let mut ws_url = anvil.url();
        ws_url.set_scheme("ws").unwrap();

        let provider = Provider::<Http>::try_from(anvil.url().to_string())?;
        let chain_id = provider.get_chainid().await?.as_u64();

        let funded_wallet = MnemonicBuilder::<English>::default()
            .phrase(TEST_MNEMONIC)
            .index(0u32)?
            .build()?
            .with_chain_id(chain_id);
        let funded_client = Arc::new(Middleware::new(provider.clone(), funded_wallet));

        // An unfunded mnemonic
        let mnemonic =
            "obvious clean kidney better photo young sun similar unit home half rough".to_string();
        let faucet_wallet = MnemonicBuilder::<English>::default()
            .phrase(mnemonic.as_str())
            .index(0u32)?
            .build()?
            .with_chain_id(chain_id);

        let options = Options {
            num_clients: 2,
            faucet_grant_amount: parse_ether(1).unwrap(),
            provider_url_ws: ws_url,
            provider_url_http: anvil.url(),
            port: portpicker::pick_unused_port().unwrap(),
            mnemonic,
            ..Default::default()
        };

        let (sender, receiver) = async_std::channel::unbounded();

        // Start the faucet
        let faucet = Faucet::create(options.clone(), receiver).await?;
        let _handle = faucet.start().await;

        // Start the web server
        spawn(async move { serve(options.port, WebState::new(sender)).await });

        // Transfer some funds to the faucet
        funded_client
            .send_transaction(
                TransactionRequest::pay(faucet_wallet.address(), options.faucet_grant_amount * 100),
                None,
            )
            .await?
            .await?;

        run_faucet_test(options, 3).await?;

        Ok(())
    }
}
