mod cli;
mod disk;
mod hex_utils;

use crate::disk::FilesystemLogger;
use bdk::blockchain::ElectrumBlockchain;
use bdk::database::MemoryDatabase;
use bdk::electrum_client::Client;
use bdk::keys::ExtendedKey;

use bdk::template::DescriptorTemplateOut;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::util::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey};
use bitcoin::BlockHash;
use bitcoin_bech32::WitnessProgram;
use lightning::chain;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::chainmonitor;
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager};
use lightning::chain::{BestBlock, Filter, Watch};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{
    ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::routing::network_graph::{NetGraphMsgHandler, NetworkGraph};
use lightning::routing::scorer::Scorer;
use lightning::util::config::UserConfig;
use lightning::util::events::{Event, PaymentPurpose};
use lightning::util::ser::ReadableArgs;
use lightning_background_processor::BackgroundProcessor;
use lightning_invoice::payment;
use lightning_invoice::utils::DefaultRouter;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use rand::{thread_rng, Rng};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use std::{fmt, iter};

pub(crate) enum HTLCStatus {
    Pending,
    Succeeded,
    Failed,
}

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Some(amt) => write!(f, "{}", amt),
            None => write!(f, "unknown"),
        }
    }
}

pub(crate) struct PaymentInfo {
    preimage: Option<PaymentPreimage>,
    secret: Option<PaymentSecret>,
    status: HTLCStatus,
    amt_msat: MillisatAmount,
}

pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, PaymentInfo>>>;

type LightningWallet = bdk_ldk::LightningWallet<ElectrumBlockchain, MemoryDatabase>;

type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<dyn Filter + Send + Sync>,
    Arc<LightningWallet>,
    Arc<LightningWallet>,
    Arc<FilesystemLogger>,
    Arc<FilesystemPersister>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
    SocketDescriptor,
    ChainMonitor,
    LightningWallet,
    LightningWallet,
    dyn chain::Access + Send + Sync,
    FilesystemLogger,
>;

pub(crate) type ChannelManager =
    SimpleArcChannelManager<ChainMonitor, LightningWallet, LightningWallet, FilesystemLogger>;

pub(crate) type InvoicePayer<E> = payment::InvoicePayer<
    Arc<ChannelManager>,
    Router,
    Arc<Mutex<Scorer>>,
    Arc<FilesystemLogger>,
    E,
>;

type Router = DefaultRouter<Arc<NetworkGraph>, Arc<FilesystemLogger>>;

type ConfirmableMonitor = (
    ChannelMonitor<InMemorySigner>,
    Arc<LightningWallet>,
    Arc<LightningWallet>,
    Arc<FilesystemLogger>,
);

fn get_wpkh_descriptors_for_extended_key(
    xkey: ExtendedKey,
    network: Network,
    base_path: &str,
    account_number: u32,
) -> (DescriptorTemplateOut, DescriptorTemplateOut) {
    let master_xprv = xkey.into_xprv(network).unwrap();
    let coin_type = match network {
        Network::Bitcoin => 0,
        _ => 1,
    };

    let base_path = DerivationPath::from_str(base_path).unwrap();
    let derivation_path = base_path.extend(&[
        ChildNumber::from_hardened_idx(coin_type).unwrap(),
        ChildNumber::from_hardened_idx(account_number).unwrap(),
    ]);

    let receive_descriptor_template = bdk::descriptor!(wpkh((
        master_xprv,
        derivation_path.extend(&[ChildNumber::Normal { index: 0 }])
    )))
    .unwrap();
    let change_descriptor_template = bdk::descriptor!(wpkh((
        master_xprv,
        derivation_path.extend(&[ChildNumber::Normal { index: 1 }])
    )))
    .unwrap();

    (receive_descriptor_template, change_descriptor_template)
}

async fn handle_ldk_events(
    channel_manager: Arc<ChannelManager>,
    wallet: Arc<LightningWallet>,
    keys_manager: Arc<KeysManager>,
    inbound_payments: PaymentInfoStorage,
    outbound_payments: PaymentInfoStorage,
    network: Network,
    event: &Event,
) {
    match event {
        Event::FundingGenerationReady {
            temporary_channel_id,
            channel_value_satoshis,
            output_script,
            ..
        } => {
            // Construct the raw transaction with one output, that is paid the amount of the
            // channel.
            let _addr = WitnessProgram::from_scriptpubkey(
                &output_script[..],
                match network {
                    Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
                    Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
                    Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
                    Network::Signet => panic!("Signet unsupported"),
                },
            )
            .expect("Lightning funding tx should always be to a SegWit output")
            .to_address();

            let target_blocks = 2;

            // Have wallet put the inputs into the transaction such that the output
            // is satisfied and then sign the funding transaction
            let funding_tx = wallet
                .construct_funding_transaction(
                    output_script,
                    *channel_value_satoshis,
                    target_blocks,
                )
                .unwrap();

            // Give the funding transaction back to LDK for opening the channel.
            if channel_manager
                .funding_transaction_generated(temporary_channel_id, funding_tx)
                .is_err()
            {
                println!(
					"\nERROR: Channel went away before we could fund it. The peer disconnected or refused the channel.");
                print!("> ");
                io::stdout().flush().unwrap();
            }
        }
        Event::PaymentReceived {
            payment_hash,
            purpose,
            amt,
            ..
        } => {
            let mut payments = inbound_payments.lock().unwrap();
            let (payment_preimage, payment_secret) = match purpose {
                PaymentPurpose::InvoicePayment {
                    payment_preimage,
                    payment_secret,
                    ..
                } => (*payment_preimage, Some(*payment_secret)),
                PaymentPurpose::SpontaneousPayment(preimage) => (Some(*preimage), None),
            };
            let status = match channel_manager.claim_funds(payment_preimage.unwrap()) {
                true => {
                    println!(
                        "\nEVENT: received payment from payment hash {} of {} millisatoshis",
                        hex_utils::hex_str(&payment_hash.0),
                        amt
                    );
                    print!("> ");
                    io::stdout().flush().unwrap();
                    HTLCStatus::Succeeded
                }
                _ => HTLCStatus::Failed,
            };
            match payments.entry(*payment_hash) {
                Entry::Occupied(mut e) => {
                    let payment = e.get_mut();
                    payment.status = status;
                    payment.preimage = payment_preimage;
                    payment.secret = payment_secret;
                }
                Entry::Vacant(e) => {
                    e.insert(PaymentInfo {
                        preimage: payment_preimage,
                        secret: payment_secret,
                        status,
                        amt_msat: MillisatAmount(Some(*amt)),
                    });
                }
            }
        }
        Event::PaymentSent {
            payment_preimage,
            payment_hash,
            ..
        } => {
            let mut payments = outbound_payments.lock().unwrap();
            for (hash, payment) in payments.iter_mut() {
                if *hash == *payment_hash {
                    payment.preimage = Some(*payment_preimage);
                    payment.status = HTLCStatus::Succeeded;
                    println!(
                        "\nEVENT: successfully sent payment of {} millisatoshis from \
								 payment hash {:?} with preimage {:?}",
                        payment.amt_msat,
                        hex_utils::hex_str(&payment_hash.0),
                        hex_utils::hex_str(&payment_preimage.0)
                    );
                    print!("> ");
                    io::stdout().flush().unwrap();
                }
            }
        }
        Event::PaymentPathFailed {
            payment_hash,
            rejected_by_dest,
            all_paths_failed,
            short_channel_id,
            ..
        } => {
            print!(
                "\nEVENT: Failed to send payment{} to payment hash {:?}",
                if *all_paths_failed {
                    ""
                } else {
                    " along MPP path"
                },
                hex_utils::hex_str(&payment_hash.0)
            );
            if let Some(scid) = short_channel_id {
                print!(" because of failure at channel {}", scid);
            }
            if *rejected_by_dest {
                println!(": re-attempting the payment will not succeed");
            } else {
                println!(": exhausted payment retry attempts");
            }
            print!("> ");
            io::stdout().flush().unwrap();

            let mut payments = outbound_payments.lock().unwrap();
            if payments.contains_key(payment_hash) {
                let payment = payments.get_mut(payment_hash).unwrap();
                payment.status = HTLCStatus::Failed;
            }
        }
        Event::PaymentForwarded {
            fee_earned_msat,
            claim_from_onchain_tx,
        } => {
            let from_onchain_str = if *claim_from_onchain_tx {
                "from onchain downstream claim"
            } else {
                "from HTLC fulfill message"
            };
            if let Some(fee_earned) = fee_earned_msat {
                println!(
                    "\nEVENT: Forwarded payment, earning {} msat {}",
                    fee_earned, from_onchain_str
                );
            } else {
                println!(
                    "\nEVENT: Forwarded payment, claiming onchain {}",
                    from_onchain_str
                );
            }
            print!("> ");
            io::stdout().flush().unwrap();
        }
        Event::PendingHTLCsForwardable { time_forwardable } => {
            let forwarding_channel_manager = channel_manager.clone();
            let min = time_forwardable.as_millis() as u64;
            tokio::spawn(async move {
                let millis_to_sleep = thread_rng().gen_range(min, min * 5) as u64;
                tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;
                forwarding_channel_manager.process_pending_htlc_forwards();
            });
        }
        Event::SpendableOutputs { outputs } => {
            let destination_address = wallet.get_unused_address().unwrap();
            let output_descriptors = &outputs.iter().collect::<Vec<_>>();
            let tx_feerate = wallet.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
            let spending_tx = keys_manager
                .spend_spendable_outputs(
                    output_descriptors,
                    Vec::new(),
                    destination_address.script_pubkey(),
                    tx_feerate,
                    &Secp256k1::new(),
                )
                .unwrap();
            wallet.broadcast_transaction(&spending_tx);
        }
        Event::ChannelClosed {
            channel_id,
            reason,
            user_channel_id: _,
        } => {
            println!(
                "\nEVENT: Channel {} closed due to: {:?}",
                hex_utils::hex_str(channel_id),
                reason
            );
            print!("> ");
            io::stdout().flush().unwrap();
        }
        Event::DiscardFunding { .. } => {
            // A "real" node should probably "lock" the UTXOs spent in funding transactions until
            // the funding transaction either confirms, or this event is generated.
        }
    }
}

pub trait T: chain::Confirm + Sync {}
impl<O: chain::Confirm + Sync> T for O {}

async fn start_ldk() {
    let args = match cli::parse_startup_args() {
        Ok(user_args) => user_args,
        Err(()) => return,
    };

    // Initialize the LDK data directory if necessary.
    let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
    fs::create_dir_all(ldk_data_dir.clone()).unwrap();

    let seed_path = format!("{}/seed", ldk_data_dir.clone());
    let seed = if let Ok(seed_file) = fs::read(seed_path.clone()) {
        assert_eq!(seed_file.len(), 32);
        let mut seed = [0; 32];
        seed.copy_from_slice(&seed_file);
        seed
    } else {
        let mut seed = [0; 32];
        thread_rng().fill_bytes(&mut seed);
        match File::create(seed_path.clone()) {
            Ok(mut f) => {
                f.write_all(&seed)
                    .expect("Failed to write node seed to disk");
                f.sync_all().expect("Failed to sync node seed to disk");
            }
            Err(e) => {
                println!("ERROR: Unable to create seed file {}: {}", seed_path, e);
                return;
            }
        }
        seed
    };
    let cur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();

    // Generate an onchain bdk wallet from our seed using native segwit wpkh descriptors
    let xprivkey = ExtendedPrivKey::new_master(args.network, &seed).unwrap();
    let xkey = ExtendedKey::from(xprivkey);
    let native_segwit_base_path = "m/84";
    let account_number = 0;
    let (receive_descriptor_template, change_descriptor_template) =
        get_wpkh_descriptors_for_extended_key(
            xkey,
            args.network,
            native_segwit_base_path,
            account_number,
        );
    // Use an in memory database for our bdk chain data
    // In a 'real' node you'd likely want to cache the chain data
    // using either sled, sqlite, or similar
    let database = MemoryDatabase::default();

    // This example uses ElectrumBlockchain but you can use any bdk Blockchain
    // backend that implements `IndexedChain` trait.
    let electrum_client = Client::new(&args.electrum_url).unwrap();
    let blockchain = ElectrumBlockchain::from(electrum_client);

    // Initialize the bdk wallet
    let bdk_wallet = bdk::Wallet::new(
        receive_descriptor_template,
        Some(change_descriptor_template),
        args.network,
        database,
        blockchain,
    )
    .unwrap();

    // Initialize our 'lightning wallet' which will implement some ldk functionality
    // on top of our bdk wallet
    let lightning_wallet = Arc::new(LightningWallet::new(bdk_wallet));

    // ## Setup
    // Step 1: Initialize the FeeEstimator

    // LightningWallet implements the FeeEstimator trait using the underlying bdk::Blockchain
    let fee_estimator = lightning_wallet.clone();

    // Step 2: Initialize the Logger
    let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

    // Step 3: Initialize the BroadcasterInterface

    // LightningWallet implements the BroadcasterInterface trait using the underlying bdk::Blockchain
    let broadcaster = lightning_wallet.clone();

    // Step 4: Initialize Persist
    let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

    // Step 5: Initialize the Transaction Filter

    // LightningWallet implements the Filter trait for us
    let filter = lightning_wallet.clone();

    // Step 6: Initialize the ChainMonitor
    let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
        Some(filter.clone()),
        broadcaster.clone(),
        logger.clone(),
        fee_estimator.clone(),
        persister.clone(),
    ));

    // Step 7: Initialize the KeysManager using our seed
    let keys_manager = Arc::new(KeysManager::new(&seed, cur.as_secs(), cur.subsec_nanos()));

    // Step 8: Read ChannelMonitor state from disk
    let mut channelmonitors = persister
        .read_channelmonitors(keys_manager.clone())
        .unwrap();

    // Step 9: Initialize the ChannelManager
    let mut user_config = UserConfig::default();
    user_config
        .peer_channel_config_limits
        .force_announced_channel_preference = false;

    let (_channel_manager_blockhash, channel_manager) = {
        if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
            let mut channel_monitor_mut_references = Vec::new();
            for (_, channel_monitor) in channelmonitors.iter_mut() {
                channel_monitor_mut_references.push(channel_monitor);
            }
            let read_args = ChannelManagerReadArgs::new(
                keys_manager.clone(),
                fee_estimator.clone(),
                chain_monitor.clone(),
                broadcaster.clone(),
                logger.clone(),
                user_config,
                channel_monitor_mut_references,
            );
            <(BlockHash, ChannelManager)>::read(&mut f, read_args).unwrap()
        } else {
            // We're starting a fresh node.
            let (tip_height, tip_header) = lightning_wallet.get_tip().unwrap();
            let tip_hash = tip_header.block_hash();

            let chain_params = ChainParameters {
                network: args.network,
                best_block: BestBlock::new(tip_hash, tip_height),
            };
            let fresh_channel_manager = channelmanager::ChannelManager::new(
                fee_estimator.clone(),
                chain_monitor.clone(),
                broadcaster.clone(),
                logger.clone(),
                keys_manager.clone(),
                user_config,
                chain_params,
            );
            (tip_hash, fresh_channel_manager)
        }
    };

    let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);

    // Make sure our filter is initialized with all the txs and outputs
    // that we need to be watching based on our set of channel monitors
    for (_, monitor) in channelmonitors.iter() {
        monitor.load_outputs_to_watch(&filter.clone());
    }

    // `Confirm` trait is not implemented on an individual ChannelMonitor
    // but on a tuple consisting of (channel_monitor, broadcaster, fee_estimator, logger)
    // this maps our channel monitors into a tuple that implements Confirm
    let mut confirmable_monitors = channelmonitors
        .into_iter()
        .map(|(_monitor_hash, channel_monitor)| {
            (
                channel_monitor,
                broadcaster.clone(),
                fee_estimator.clone(),
                logger.clone(),
            )
        })
        .collect::<Vec<ConfirmableMonitor>>();

    // construct and collect a Vec of references to objects that implement the Confirm trait
    // note: we chain the channel_manager into this Vec
    let confirmables = confirmable_monitors
        .iter()
        .map(|cm| cm as &dyn chain::Confirm)
        .chain(iter::once(&*channel_manager as &dyn chain::Confirm))
        .collect::<Vec<&dyn chain::Confirm>>();

    // Step 10: Sync our channel monitors and channel manager to chain tip
    lightning_wallet.sync(confirmables).unwrap();

    // Step 11: Give ChannelMonitors to ChainMonitor to watch
    for confirmable_monitor in confirmable_monitors.drain(..) {
        let channel_monitor = confirmable_monitor.0;
        let funding_txo = channel_monitor.get_funding_txo().0;
        chain_monitor
            .watch_channel(funding_txo, channel_monitor)
            .unwrap();
    }

    // this probably doesn't need to be done here
    // as we will sync them every 30s in a polling loop later
    let confirmables = vec![
        &*channel_manager as &dyn chain::Confirm,
        &*chain_monitor as &dyn chain::Confirm,
    ];

    lightning_wallet.sync(confirmables).unwrap();

    // Step 12: Optional: Initialize the NetGraphMsgHandler
    let genesis = genesis_block(args.network).header.block_hash();
    let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
    let network_graph = Arc::new(disk::read_network(Path::new(&network_graph_path), genesis));
    let network_gossip = Arc::new(NetGraphMsgHandler::new(
        Arc::clone(&network_graph),
        None::<Arc<dyn chain::Access + Send + Sync>>,
        logger.clone(),
    ));
    let network_graph_persist = Arc::clone(&network_graph);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(600));
        loop {
            interval.tick().await;
            if disk::persist_network(Path::new(&network_graph_path), &network_graph_persist)
                .is_err()
            {
                // Persistence errors here are non-fatal as we can just fetch the routing graph
                // again later, but they may indicate a disk error which could be fatal elsewhere.
                eprintln!(
                    "Warning: Failed to persist network graph, check your disk and permissions"
                );
            }
        }
    });

    // Step 13: Initialize the PeerManager
    let mut ephemeral_bytes = [0; 32];
    rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
    let lightning_msg_handler = MessageHandler {
        chan_handler: channel_manager.clone(),
        route_handler: network_gossip.clone(),
    };
    let peer_manager = Arc::new(PeerManager::new(
        lightning_msg_handler,
        keys_manager.get_node_secret(),
        &ephemeral_bytes,
        logger.clone(),
        Arc::new(IgnoringMessageHandler {}),
    ));

    // ## Running LDK
    // Step 14: Initialize networking

    let peer_manager_connection_handler = peer_manager.clone();
    let listening_port = args.ldk_peer_listening_port;
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", listening_port))
            .await
            .expect("Failed to bind to listen port - is something else already listening on it?");
        loop {
            let peer_mgr = peer_manager_connection_handler.clone();
            let tcp_stream = listener.accept().await.unwrap().0;
            tokio::spawn(async move {
                lightning_net_tokio::setup_inbound(
                    peer_mgr.clone(),
                    tcp_stream.into_std().unwrap(),
                )
                .await;
            });
        }
    });

    // Step 15: Periodically sync ChannelManager and ChainMonitor to new tip
    let lightning_wallet_sync = lightning_wallet.clone();
    let channel_manager_sync = channel_manager.clone();
    let chain_monitor_sync = chain_monitor.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let confirmables = vec![
                &*channel_manager_sync as &dyn chain::Confirm,
                &*chain_monitor_sync as &dyn chain::Confirm,
            ];

            if let Err(e) = lightning_wallet_sync.sync(confirmables) {
                println!("sync failed: {:?}", e);
            }
        }
    });

    // Step 16: Handle LDK Events
    let channel_manager_event_listener = channel_manager.clone();
    let keys_manager_listener = keys_manager.clone();
    // TODO: persist payment info to disk
    let lightning_wallet_listener = lightning_wallet.clone();
    let inbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
    let outbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
    let inbound_pmts_for_events = inbound_payments.clone();
    let outbound_pmts_for_events = outbound_payments.clone();
    let network = args.network;
    let handle = tokio::runtime::Handle::current();
    let event_handler = move |event: &Event| {
        handle.block_on(handle_ldk_events(
            channel_manager_event_listener.clone(),
            lightning_wallet_listener.clone(),
            keys_manager_listener.clone(),
            inbound_pmts_for_events.clone(),
            outbound_pmts_for_events.clone(),
            network,
            event,
        ));
    };

    // Step 17: Initialize routing Scorer
    let scorer_path = format!("{}/scorer", ldk_data_dir.clone());
    let scorer = Arc::new(Mutex::new(disk::read_scorer(Path::new(&scorer_path))));
    let scorer_persist = Arc::clone(&scorer);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(600));
        loop {
            interval.tick().await;
            if disk::persist_scorer(Path::new(&scorer_path), &scorer_persist.lock().unwrap())
                .is_err()
            {
                // Persistence errors here are non-fatal as channels will be re-scored as payments
                // fail, but they may indicate a disk error which could be fatal elsewhere.
                eprintln!("Warning: Failed to persist scorer, check your disk and permissions");
            }
        }
    });

    // Step 18: Create InvoicePayer
    let router = DefaultRouter::new(network_graph.clone(), logger.clone());
    let invoice_payer = Arc::new(InvoicePayer::new(
        channel_manager.clone(),
        router,
        scorer.clone(),
        logger.clone(),
        event_handler,
        payment::RetryAttempts(5),
    ));

    // Step 19: Persist ChannelManager
    let data_dir = ldk_data_dir.clone();
    let persist_channel_manager_callback =
        move |node: &ChannelManager| FilesystemPersister::persist_manager(data_dir.clone(), &*node);

    // Step 20: Background Processing
    let background_processor = BackgroundProcessor::start(
        persist_channel_manager_callback,
        invoice_payer.clone(),
        chain_monitor.clone(),
        channel_manager.clone(),
        Some(network_gossip.clone()),
        peer_manager.clone(),
        logger.clone(),
    );

    // Reconnect to channel peers if possible.
    let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
    match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
        Ok(mut info) => {
            for (pubkey, peer_addr) in info.drain() {
                for chan_info in channel_manager.list_channels() {
                    if pubkey == chan_info.counterparty.node_id {
                        let _ =
                            cli::connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone())
                                .await;
                    }
                }
            }
        }
        Err(e) => println!(
            "ERROR: errored reading channel peer info from disk: {:?}",
            e
        ),
    }

    // Regularly broadcast our node_announcement. This is only required (or possible) if we have
    // some public channels, and is only useful if we have public listen address(es) to announce.
    // In a production environment, this should occur only after the announcement of new channels
    // to avoid churn in the global network graph.
    let chan_manager = Arc::clone(&channel_manager);
    let network = args.network;
    if !args.ldk_announced_listen_addr.is_empty() {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                chan_manager.broadcast_node_announcement(
                    [0; 3],
                    args.ldk_announced_node_name,
                    args.ldk_announced_listen_addr.clone(),
                );
            }
        });
    }

    // Start the CLI.
    cli::poll_for_user_input(
        lightning_wallet.clone(),
        invoice_payer.clone(),
        peer_manager.clone(),
        channel_manager.clone(),
        keys_manager.clone(),
        network_graph.clone(),
        scorer.clone(),
        inbound_payments,
        outbound_payments,
        ldk_data_dir.clone(),
        logger.clone(),
        network,
    )
    .await;

    // Stop the background processor.
    background_processor.stop().unwrap();
}

#[tokio::main]
pub async fn main() {
    start_ldk().await;
}
