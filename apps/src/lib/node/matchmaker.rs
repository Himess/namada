use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anoma::proto::{Intent, Tx};
use anoma::types::address::{self, Address};
use anoma::types::dylib;
use anoma::types::intent::{IntentTransfers, MatchedExchanges};
use anoma::types::key::ed25519::Keypair;
use anoma::types::matchmaker::AddIntentResult;
use anoma::types::transaction::{Fee, WrapperTx};
use borsh::{BorshDeserialize, BorshSerialize};
use libc::c_void;
use libloading::Library;
#[cfg(not(feature = "ABCI"))]
use tendermint_config::net;
#[cfg(not(feature = "ABCI"))]
use tendermint_config::net::Address as TendermintAddress;
#[cfg(feature = "ABCI")]
use tendermint_config_abci::net;
#[cfg(feature = "ABCI")]
use tendermint_config_abci::net::Address as TendermintAddress;
use tokio::sync::oneshot;

use super::gossip::matchmakers::{
    ClientDialer, ClientListener, MsgFromClient, MsgFromServer,
};
use crate::cli::args;
use crate::client::rpc;
use crate::client::tx::broadcast_tx;
use crate::{cli, config, wasm_loader};

/// Run a matchmaker
#[tokio::main]
pub async fn run(
    config::Matchmaker {
        matchmaker_path,
        tx_code_path,
    }: config::Matchmaker,
    intent_gossiper_addr: SocketAddr,
    ledger_addr: TendermintAddress,
    tx_signing_key: Rc<Keypair>,
    tx_source_address: Address,
    wasm_dir: impl AsRef<Path>,
) {
    let (runner, result_handler) = Runner::new_pair(
        intent_gossiper_addr,
        matchmaker_path,
        tx_code_path,
        ledger_addr,
        tx_signing_key,
        tx_source_address,
        wasm_dir,
    );

    // Instantiate and run the matchmaker implementation in a dedicated thread
    let runner_join_handle = std::thread::spawn(move || {
        runner.listen();
    });

    // Process results async
    result_handler.run().await;

    if let Err(error) = runner_join_handle.join() {
        eprintln!("Matchmaker runner failed with: {:?}", error);
        cli::safe_exit(1)
    }
}

/// A matchmaker receive intents and tries to find a match with previously
/// received intent.
#[derive(Debug)]
pub struct Runner {
    matchmaker_path: PathBuf,
    /// The client listener. This is consumed once the listener is started with
    /// [`Matchmaker::listen`].
    listener: Option<ClientListener>,
    /// Sender of results of matched intents to the [`ResultHandler`].
    result_send: tokio::sync::mpsc::UnboundedSender<AddIntentResult>,
}

/// Result handler processes the results sent from the matchmaker [`Runner`].
#[derive(Debug)]
pub struct ResultHandler {
    /// A dialer can send messages to the connected intent gossip node
    dialer: ClientDialer,
    /// A receiver of matched intents results from the [`Runner`].
    result_recv: tokio::sync::mpsc::UnboundedReceiver<AddIntentResult>,
    /// The ledger address to send any crafted transaction to
    ledger_address: net::Address,
    /// The code of the transaction that is going to be send to a ledger.
    tx_code: Vec<u8>,
    /// A source address for transactions created from intents.
    tx_source_address: Address,
    /// A keypair that will be used to sign transactions.
    tx_signing_key: Rc<Keypair>,
}

/// The loaded implementation's dylib and its state
#[derive(Debug)]
struct MatchmakerImpl {
    /// The matchmaker's state as a raw mutable pointer to allow custom user
    /// implementation in a dylib.
    /// NOTE: The `state` field MUST be above the `library` field to ensure
    /// that its destructor is ran before the implementation code is dropped.
    state: MatchmakerState,
    /// Matchmaker's implementation loaded from dylib
    library: Library,
}

/// The matchmaker's state as a raw mutable pointer to allow custom user
/// implementation in a dylib
#[derive(Debug)]
struct MatchmakerState(Arc<*mut c_void>);

/// Matchmaker message for communication between the runner, P2P and the
/// implementation
#[derive(Debug)]
pub enum MatchmakerMessage {
    /// Run the matchmaker with the given intent
    ApplyIntent(Intent, oneshot::Sender<bool>),
}

impl Runner {
    /// Create a new matchmaker and a dialer that can be used to send messages
    /// to the intent gossiper node.
    pub fn new_pair(
        intent_gossiper_addr: SocketAddr,
        matchmaker_path: PathBuf,
        tx_code_path: PathBuf,
        ledger_address: TendermintAddress,
        tx_signing_key: Rc<Keypair>,
        tx_source_address: Address,
        wasm_dir: impl AsRef<Path>,
    ) -> (Self, ResultHandler) {
        // Setup a channel for sending matchmaker results from `Self` to the
        // `ResultHandler`
        let (result_send, result_recv) = tokio::sync::mpsc::unbounded_channel();

        // Prepare a client for intent gossiper node connection
        let (listener, dialer) = ClientListener::new_pair(intent_gossiper_addr);

        let tx_code = wasm_loader::read_wasm(&wasm_dir, tx_code_path);

        (
            Self {
                matchmaker_path,
                listener: Some(listener),
                result_send,
            },
            ResultHandler {
                dialer,
                result_recv,
                ledger_address,
                tx_code,
                tx_source_address,
                tx_signing_key,
            },
        )
    }

    pub fn listen(mut self) {
        // Load the implementation's dylib and instantiate it. We have to do
        // that here instead of `Self::new_pair`, because we cannot send
        // it across threads and the listener is launched in a dedicated thread.

        // The dylib should be built in the same directory as where Anoma
        // binaries are, even when ran via `cargo run`. Anoma's pre-built
        // binaries are distributed with the dylib(s) in the same directory.
        let dylib_dir = {
            let anoma_path = env::current_exe().unwrap();
            anoma_path
                .parent()
                .map(|path| path.to_owned())
                .unwrap_or_else(|| ".".into())
        };
        let mut matchmaker_dylib = dylib_dir.join(&self.matchmaker_path);
        matchmaker_dylib.set_extension(dylib::FILE_EXT);
        tracing::info!(
            "Running matchmaker from {}",
            matchmaker_dylib.to_string_lossy()
        );
        if !matchmaker_dylib.exists() {
            panic!(
                "The matchmaker library couldn't not be found. Did you build \
                 it?"
            )
        }
        let matchmaker_code =
            unsafe { Library::new(matchmaker_dylib).unwrap() };

        // Instantiate the matchmaker
        let new_matchmaker: libloading::Symbol<
            unsafe extern "C" fn() -> *mut c_void,
        > = unsafe { matchmaker_code.get(b"_new_matchmaker").unwrap() };

        let state = MatchmakerState(Arc::new(unsafe { new_matchmaker() }));

        let r#impl = MatchmakerImpl {
            state,
            library: matchmaker_code,
        };

        // Run the listener for messages from the connected intent gossiper node
        self.listener.take().unwrap().listen(|msg| match msg {
            MsgFromServer::AddIntent { id, data } => {
                self.try_match_intent(&r#impl, id, data);
            }
        })
    }

    /// add the intent to the matchmaker mempool and tries to find a match for
    /// that intent
    fn try_match_intent(
        &self,
        r#impl: &MatchmakerImpl,
        intent_id: Vec<u8>,
        intent_data: Vec<u8>,
    ) {
        let add_intent: libloading::Symbol<
            unsafe extern "C" fn(
                *mut c_void,
                &Vec<u8>,
                &Vec<u8>,
            ) -> AddIntentResult,
        > = unsafe { r#impl.library.get(b"_add_intent").unwrap() };

        let result =
            unsafe { add_intent(*r#impl.state.0, &intent_id, &intent_data) };

        self.result_send.send(result).unwrap();
    }
}

impl Drop for MatchmakerImpl {
    fn drop(&mut self) {
        let drop_matchmaker: libloading::Symbol<
            unsafe extern "C" fn(*mut c_void),
        > = unsafe { self.library.get(b"_drop_matchmaker").unwrap() };

        unsafe { drop_matchmaker(*self.state.0) };
    }
}

impl ResultHandler {
    async fn run(mut self) {
        while let Some(result) = self.result_recv.recv().await {
            if let Some(tx) = result.tx {
                self.submit_tx(tx).await
            }
            if let Some(intent_ids) = result.matched_intents {
                self.dialer.send(MsgFromClient::Matched { intent_ids })
            }
        }
    }

    async fn submit_tx(&self, tx_data: Vec<u8>) {
        let tx_code = self.tx_code.clone();
        let matches = MatchedExchanges::try_from_slice(&tx_data[..]).unwrap();
        let intent_transfers = IntentTransfers {
            matches,
            source: self.tx_source_address.clone(),
        };
        let tx_data = intent_transfers.try_to_vec().unwrap();
        let tx = WrapperTx::new(
            Fee {
                amount: 0.into(),
                token: address::xan(),
            },
            &self.tx_signing_key,
            rpc::query_epoch(args::Query {
                ledger_address: self.ledger_address.clone(),
            })
            .await,
            0.into(),
            Tx::new(tx_code, Some(tx_data)).sign(&self.tx_signing_key),
        );

        let response =
            broadcast_tx(self.ledger_address.clone(), tx, &self.tx_signing_key)
                .await;
        match response {
            Ok(tx_response) => {
                tracing::info!(
                    "Injected transaction from matchmaker with result: {:#?}",
                    tx_response
                );
            }
            Err(err) => {
                tracing::error!(
                    "Matchmaker error in submitting a transaction to the \
                     ledger: {}",
                    err
                );
            }
        }
    }
}
