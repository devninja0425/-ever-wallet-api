use std::collections::{hash_map, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use nekoton::core::models::{TokenWalletTransaction, TokenWalletVersion};
use nekoton::transport::models::{ExistingContract, RawContractState};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::mpsc;
use ton_block::{CommonMsgInfo, GetRepresentationHash, MsgAddressInt, Serializable};
use ton_types::UInt256;

use self::settings::*;
use self::ton_subscriber::*;
use self::transaction_handler::*;
use crate::models::*;
use crate::utils::*;

mod models;
mod settings;
mod ton_subscriber;
mod transaction_handler;

pub struct TonCore {
    ton_engine: Arc<ton_indexer::Engine>,
    ton_subscriber: Arc<TonSubscriber>,

    owners_cache: OwnersCache,

    transaction_observer: Arc<TransactionObserver>,
    token_transaction_observer: Arc<TokenTransactionObserver>,

    transaction_producer: ReceiveTransactionTx,
    token_transaction_producer: ReceiveTokenTransactionTx,

    pending_messages_producer: PendingMessagesTx,
    pending_messages: Arc<Mutex<PendingMessagesCache>>,

    initialized: tokio::sync::Mutex<bool>,
}

impl TonCore {
    pub async fn new(
        config: TonCoreConfig,
        global_config: ton_indexer::GlobalConfig,
        owners_cache: OwnersCache,
        transaction_producer: ReceiveTransactionTx,
        token_transaction_producer: ReceiveTokenTransactionTx,
    ) -> Result<Arc<Self>> {
        let pending_messages_cache = Arc::new(Mutex::new(HashMap::new()));
        let ton_subscriber = TonSubscriber::new(pending_messages_cache.clone());

        let node_config = get_node_config(&config).await?;
        let ton_engine = ton_indexer::Engine::new(
            node_config,
            global_config,
            vec![ton_subscriber.clone() as Arc<dyn ton_indexer::Subscriber>],
        )
        .await?;

        let (transaction_tx, transaction_rx) = mpsc::unbounded_channel();
        let (token_transaction_tx, token_transaction_rx) = mpsc::unbounded_channel();

        let (pending_messages_tx, pending_messages_rx) = mpsc::unbounded_channel();

        let engine = Arc::new(Self {
            ton_engine,
            owners_cache,
            ton_subscriber,
            transaction_producer,
            token_transaction_producer,
            transaction_observer: Arc::new(TransactionObserver { tx: transaction_tx }),
            token_transaction_observer: Arc::new(TokenTransactionObserver {
                tx: token_transaction_tx,
            }),
            pending_messages_producer: pending_messages_tx,
            pending_messages: pending_messages_cache,
            initialized: Default::default(),
        });

        engine.start_listening_transactions(transaction_rx);
        engine.start_listening_token_transactions(token_transaction_rx);

        engine.start_listening_pending_messages(pending_messages_rx);

        Ok(engine)
    }

    pub async fn start(&self) -> Result<()> {
        let mut initialized = self.initialized.lock().await;
        if *initialized {
            return Err(TonCoreError::AlreadyInitialized.into());
        }

        self.ton_engine.start().await?;
        self.ton_subscriber.start().await?;

        *initialized = true;
        Ok(())
    }

    pub async fn get_contract_state(&self, account: UInt256) -> Result<ExistingContract> {
        match self.ton_subscriber.get_contract_state(account).await? {
            RawContractState::Exists(contract) => Ok(contract),
            RawContractState::NotExists => {
                Err(TonCoreError::AccountNotFound(account.to_hex_string()).into())
            }
        }
    }

    pub async fn send_ton_message(
        &self,
        message: &ton_block::Message,
        expire_at: u32,
    ) -> Result<()> {
        let (account, to) = match message.header() {
            ton_block::CommonMsgInfo::ExtInMsgInfo(header) => (
                UInt256::from_be_bytes(&header.dst.address().get_bytestring(0)),
                ton_block::AccountIdPrefixFull::prefix(&header.dst)?,
            ),
            _ => return Err(TonCoreError::ExternalTonMessageExpected.into()),
        };

        let cells = message.write_to_new_cell()?.into();
        let serialized = ton_types::serialize_toc(&cells)?;

        let message_hash = message.serialize()?.repr_hash();
        self.add_pending_message(account, message_hash, expire_at)?;

        match self
            .ton_engine
            .broadcast_external_message(&to, &serialized)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                self.cancel_pending_message(account, message_hash)?;
                Err(e)
            }
        }
    }

    pub fn add_account_subscription<I>(&self, accounts: I)
    where
        I: IntoIterator<Item = UInt256>,
    {
        self.ton_subscriber
            .add_transactions_subscription(accounts, &self.transaction_observer);
    }

    pub fn add_token_account_subscription<I>(&self, accounts: I)
    where
        I: IntoIterator<Item = UInt256>,
    {
        self.ton_subscriber
            .add_transactions_subscription(accounts, &self.token_transaction_observer);
    }

    fn start_listening_transactions(
        self: &Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<TransactionContext>,
    ) {
        let engine = Arc::downgrade(self);

        tokio::spawn(async move {
            while let Some(transaction_ctx) = rx.recv().await {
                let engine = match engine.upgrade() {
                    Some(engine) => engine,
                    None => break,
                };

                log::info!("Transaction context: {:#?}", transaction_ctx);

                // Find sent transaction and mark as Delivered
                if let Some(in_msg) = transaction_ctx
                    .transaction
                    .in_msg
                    .as_ref()
                    .and_then(|data| data.read_struct().ok())
                {
                    if let CommonMsgInfo::ExtInMsgInfo(_) = in_msg.header() {
                        let mut cache = engine.pending_messages.lock();
                        let hash = in_msg.hash().unwrap_or_default();
                        let state = cache.get_mut(&(transaction_ctx.account, hash));
                        if let Some(state) = state {
                            if let Some(tx) = state.tx.take() {
                                tx.send((
                                    transaction_ctx.account,
                                    hash,
                                    PendingMessageStatus::Delivered,
                                ))
                                .ok();
                            }
                        }
                    }
                }

                match handle_transaction(transaction_ctx).await {
                    Ok(transaction) => {
                        engine.transaction_producer.send(transaction).ok();
                    }
                    Err(e) => {
                        log::error!("Failed to handle received transaction: {}", e);
                    }
                }
            }

            rx.close();
            while rx.recv().await.is_some() {}
        });
    }

    fn start_listening_token_transactions(
        self: &Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<(TokenTransactionContext, TokenWalletTransaction)>,
    ) {
        let engine = Arc::downgrade(self);

        tokio::spawn(async move {
            while let Some((token_transaction_ctx, parsed_token_transaction)) = rx.recv().await {
                let engine = match engine.upgrade() {
                    Some(engine) => engine,
                    None => break,
                };

                log::info!("Token transaction context: {:#?}", token_transaction_ctx);
                log::info!("Parsed token transaction: {:#?}", parsed_token_transaction);

                match handle_token_transaction(
                    token_transaction_ctx,
                    parsed_token_transaction,
                    &engine.owners_cache,
                )
                .await
                {
                    Ok(transaction) => {
                        engine.token_transaction_producer.send(transaction).ok();
                    }
                    Err(e) => {
                        log::error!("Failed to handle received token transaction: {}", e);
                    }
                }
            }

            rx.close();
            while rx.recv().await.is_some() {}
        });
    }

    fn start_listening_pending_messages(self: &Arc<Self>, mut rx: PendingMessagesRx) {
        let engine = Arc::downgrade(self);

        tokio::spawn(async move {
            while let Some((account, hash, status)) = rx.recv().await {
                let engine = match engine.upgrade() {
                    Some(engine) => engine,
                    None => break,
                };

                if status == PendingMessageStatus::Expired {
                    engine
                        .transaction_producer
                        .send(ReceiveTransaction::UpdateSent(UpdateSentTransaction {
                            message_hash: hash.to_hex_string(),
                            account_workchain_id: ton_block::BASE_WORKCHAIN_ID,
                            account_hex: account.to_hex_string(),
                            input: UpdateSendTransaction::error("Expired".to_string()),
                        }))
                        .ok();
                }

                if let Err(err) = engine.cancel_pending_message(account, hash) {
                    log::error!("Failed to cancel pending message: {:?}", err)
                }
            }

            rx.close();
            while rx.recv().await.is_some() {}
        });
    }

    fn add_pending_message(&self, account: UInt256, hash: UInt256, expired_at: u32) -> Result<()> {
        let mut cache = self.pending_messages.lock();
        match cache.entry((account, hash)) {
            hash_map::Entry::Vacant(entry) => {
                entry.insert(PendingMessageState {
                    expired_at,
                    tx: Some(self.pending_messages_producer.clone()),
                });
            }
            hash_map::Entry::Occupied(_) => {
                return Err(TonCoreError::PendingMessageExist(
                    hash.to_hex_string(),
                    account.to_hex_string(),
                )
                .into());
            }
        };
        Ok(())
    }

    fn cancel_pending_message(&self, account: UInt256, hash: UInt256) -> Result<()> {
        let mut cache = self.pending_messages.lock();
        if cache.remove(&(account, hash)).is_none() {
            return Err(TonCoreError::PendingMessageNotExist(
                hash.to_hex_string(),
                account.to_hex_string(),
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct TransactionContext {
    account: UInt256,
    transaction_hash: UInt256,
    transaction: ton_block::Transaction,
}

struct TransactionObserver {
    tx: mpsc::UnboundedSender<TransactionContext>,
}

impl TransactionsSubscription for TransactionObserver {
    fn handle_transaction(&self, ctx: TxContext<'_>) -> Result<()> {
        let transaction = TransactionContext {
            account: *ctx.account,
            transaction_hash: *ctx.transaction_hash,
            transaction: ctx.transaction.clone(),
        };

        self.tx.send(transaction)?;

        // Done
        Ok(())
    }
}

#[derive(Debug)]
pub struct TokenTransactionContext {
    account: UInt256,
    block_hash: UInt256,
    block_utime: u32,
    message_hash: UInt256,
    transaction_hash: UInt256,
    transaction: ton_block::Transaction,
    shard_accounts: ton_block::ShardAccounts,
}

struct TokenTransactionObserver {
    tx: mpsc::UnboundedSender<(TokenTransactionContext, TokenWalletTransaction)>,
}

impl TransactionsSubscription for TokenTransactionObserver {
    fn handle_transaction(&self, ctx: TxContext<'_>) -> Result<()> {
        if ctx.transaction_info.aborted {
            return Ok(());
        }

        let parsed = nekoton::core::parsing::parse_token_transaction(
            ctx.transaction,
            ctx.transaction_info,
            TokenWalletVersion::Tip3v4,
        );

        if let Some(parsed) = parsed {
            let message_hash = match &parsed {
                TokenWalletTransaction::IncomingTransfer(_)
                | TokenWalletTransaction::Accept(_)
                | TokenWalletTransaction::TransferBounced(_)
                | TokenWalletTransaction::SwapBackBounced(_) => ctx
                    .transaction
                    .in_msg
                    .clone()
                    .map(|message| message.hash())
                    .unwrap_or_default(),
                TokenWalletTransaction::OutgoingTransfer(_)
                | TokenWalletTransaction::SwapBack(_) => {
                    let mut hash = Default::default();
                    let _ = ctx.transaction.out_msgs.iterate(|message| {
                        hash = message.hash().unwrap_or_default();
                        Ok(false)
                    });
                    hash
                }
            };

            self.tx
                .send((
                    TokenTransactionContext {
                        account: *ctx.account,
                        block_hash: *ctx.block_hash,
                        block_utime: ctx.block_info.gen_utime().0,
                        message_hash,
                        transaction_hash: *ctx.transaction_hash,
                        transaction: ctx.transaction.clone(),
                        shard_accounts: ctx.shard_accounts.clone(),
                    },
                    parsed,
                ))
                .ok();
        }

        Ok(())
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct TonCoreConfig {
    pub port: u16,
    pub rocks_db_path: PathBuf,
    pub file_db_path: PathBuf,
    pub keys_path: PathBuf,
}

pub enum ReceiveTransaction {
    Create(CreateReceiveTransaction),
    UpdateSent(UpdateSentTransaction),
}

pub enum ReceiveTokenTransaction {
    Create(CreateReceiveTokenTransaction),
    UpdateSent(UpdateSentTokenTransaction),
}

pub type ReceiveTransactionTx = mpsc::UnboundedSender<ReceiveTransaction>;
pub type ReceiveTransactionRx = mpsc::UnboundedReceiver<ReceiveTransaction>;

pub type ReceiveTokenTransactionTx = mpsc::UnboundedSender<ReceiveTokenTransaction>;
pub type ReceiveTokenTransactionRx = mpsc::UnboundedReceiver<ReceiveTokenTransaction>;

pub type PendingMessagesTx = mpsc::UnboundedSender<(UInt256, UInt256, PendingMessageStatus)>;
pub type PendingMessagesRx = mpsc::UnboundedReceiver<(UInt256, UInt256, PendingMessageStatus)>;

pub type PendingMessagesCache = HashMap<(UInt256, UInt256), PendingMessageState>;

pub struct PendingMessageState {
    expired_at: u32,
    tx: Option<PendingMessagesTx>,
}

#[derive(PartialEq)]
pub enum PendingMessageStatus {
    Delivered,
    Expired,
}

#[derive(thiserror::Error, Debug)]
enum TonCoreError {
    #[error("Already initialized")]
    AlreadyInitialized,
    #[error("External ton message expected")]
    ExternalTonMessageExpected,
    #[error("Pending message hash `{0}` exist for account `{1}`")]
    PendingMessageExist(String, String),
    #[error("Pending message hash `{0}` not exist for account `{1}`")]
    PendingMessageNotExist(String, String),
    #[error("Account `{0}` not found")]
    AccountNotFound(String),
}
