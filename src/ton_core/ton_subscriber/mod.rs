use std::collections::{hash_map, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use anyhow::Result;
use nekoton::core::models::TokenWalletVersion;
use nekoton::transport::models::ExistingContract;
use nekoton_utils::TrustMe;
use parking_lot::{Mutex, RwLock};
use tiny_adnl::utils::FxHashMap;
use tokio::sync::Notify;
use ton_block::{Deserializable, HashmapAugType};
use ton_indexer::utils::{BlockIdExtExtension, BlockProofStuff, BlockStuff, ShardStateStuff};
use ton_indexer::EngineStatus;
use ton_types::{HashmapType, UInt256};

use crate::ton_core::*;

pub struct TonSubscriber {
    ready: AtomicBool,
    ready_signal: Notify,
    current_utime: AtomicU32,
    state_subscriptions: RwLock<HashMap<UInt256, StateSubscription>>,
    token_subscription: RwLock<Option<TokenSubscription>>,
    shard_accounts_cache: RwLock<HashMap<ton_block::ShardIdent, ton_block::ShardAccounts>>,
    mc_block_awaiters: Mutex<FxHashMap<usize, Box<dyn BlockAwaiter>>>,
    messages_queue: Arc<PendingMessagesQueue>,
}

impl TonSubscriber {
    pub fn new(messages_queue: Arc<PendingMessagesQueue>) -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(false),
            ready_signal: Notify::new(),
            current_utime: AtomicU32::new(0),
            state_subscriptions: RwLock::new(HashMap::new()),
            token_subscription: RwLock::new(None),
            shard_accounts_cache: RwLock::new(HashMap::new()),
            mc_block_awaiters: Mutex::new(FxHashMap::with_capacity_and_hasher(
                4,
                Default::default(),
            )),
            messages_queue,
        })
    }

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        self.wait_sync().await;
        Ok(())
    }

    pub fn current_utime(&self) -> u32 {
        self.current_utime.load(Ordering::Acquire)
    }

    pub fn add_transactions_subscription<I, T>(&self, accounts: I, subscription: &Arc<T>)
    where
        I: IntoIterator<Item = UInt256>,
        T: TransactionsSubscription + 'static,
    {
        let mut state_subscriptions = self.state_subscriptions.write();

        let weak = Arc::downgrade(subscription) as Weak<dyn TransactionsSubscription>;

        for account in accounts {
            match state_subscriptions.entry(account) {
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(StateSubscription {
                        transaction_subscriptions: vec![weak.clone()],
                    });
                }
                hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().transaction_subscriptions.push(weak.clone());
                }
            };
        }
    }

    pub fn add_token_subscription<T>(&self, subscription: &Arc<T>)
    where
        T: TransactionsSubscription + 'static,
    {
        let mut token_subscription = self.token_subscription.write();
        let _ = token_subscription.insert(TokenSubscription {
            transaction_subscription: subscription.clone(),
        });
    }

    pub fn get_contract_state(&self, account: &UInt256) -> Result<Option<ExistingContract>> {
        let shard_accounts_cache = self.shard_accounts_cache.read();
        for (shard_ident, shard_accounts) in shard_accounts_cache.iter() {
            if contains_account(shard_ident, account) {
                match shard_accounts.get(account) {
                    Ok(account) => return ExistingContract::from_shard_account_opt(&account),
                    Err(e) => {
                        log::error!("Failed to get account {}: {:?}", account.to_hex_string(), e);
                    }
                };
            }
        }

        Ok(None)
    }

    fn handle_masterchain_block(&self, block: &ton_block::Block) -> Result<()> {
        let block_info = block.info.read_struct()?;
        self.current_utime
            .store(block_info.gen_utime().0, Ordering::Release);

        let mut mc_block_awaiters = self.mc_block_awaiters.lock();
        mc_block_awaiters.retain(
            |_, awaiter| match awaiter.handle_block(block, &block_info) {
                Ok(action) => action == BlockAwaiterAction::Retain,
                Err(e) => {
                    log::error!("Failed to handle masterchain block: {:?}", e);
                    true
                }
            },
        );

        Ok(())
    }

    fn handle_shard_block(
        &self,
        block: &ton_block::Block,
        shard_state: &ton_block::ShardStateUnsplit,
        block_hash: &UInt256,
    ) -> Result<()> {
        let block_info = block.info.read_struct()?;
        let extra = block.extra.read_struct()?;
        let account_blocks = extra.read_account_blocks()?;
        let shard_accounts = shard_state.read_accounts()?;

        let mut shard_accounts_cache = self.shard_accounts_cache.write();
        shard_accounts_cache.insert(*block_info.shard(), shard_accounts.clone());
        if block_info.after_merge() || block_info.after_split() {
            let block_ids = block_info.read_prev_ids()?;
            match block_ids.len() {
                1 => {
                    let (left, right) = block_ids[0].shard_id.split()?;
                    if shard_accounts_cache.contains_key(&left)
                        && shard_accounts_cache.contains_key(&right)
                    {
                        shard_accounts_cache.remove(&block_ids[0].shard_id);
                    }
                }
                len if len > 1 => {
                    for block_id in block_info.read_prev_ids()? {
                        shard_accounts_cache.remove(&block_id.shard_id);
                    }
                }
                _ => {}
            }
        }
        drop(shard_accounts_cache);

        let subscriptions = self.state_subscriptions.read();
        let token_subscription = self.token_subscription.read();
        account_blocks.iterate_with_keys(|account, account_block| {
            match subscriptions.get(&account) {
                Some(subscription) => {
                    if let Err(e) = subscription.handle_block(
                        &self.messages_queue,
                        &shard_accounts,
                        &block_info,
                        &account_block,
                        &account,
                        block_hash,
                    ) {
                        log::error!("Failed to handle block: {:?}", e);
                    }
                }
                None => {
                    if token_subscription.is_some() {
                        if let Err(e) = token_subscription.as_ref().trust_me().handle_block(
                            &self.state_subscriptions,
                            &shard_accounts,
                            &block_info,
                            &account_block,
                            &account,
                            block_hash,
                        ) {
                            log::error!("Failed to handle block: {:?}", e);
                        }
                    }
                }
            };

            Ok(true)
        })?;

        self.messages_queue
            .update(block_info.shard(), block_info.gen_utime().0);

        Ok(())
    }

    async fn wait_sync(&self) {
        if self.ready.load(Ordering::Acquire) {
            return;
        }
        self.ready_signal.notified().await;
    }
}

#[async_trait::async_trait]
impl ton_indexer::Subscriber for TonSubscriber {
    async fn engine_status_changed(&self, status: EngineStatus) {
        if status == EngineStatus::Synced {
            log::info!("TON subscriber is ready");
            self.ready.store(true, Ordering::Release);
            self.ready_signal.notify_waiters();
        }
    }

    async fn process_block(
        &self,
        block: &BlockStuff,
        _block_proof: Option<&BlockProofStuff>,
        shard_state: &ShardStateStuff,
    ) -> Result<()> {
        if block.id().is_masterchain() {
            self.handle_masterchain_block(block.block())?;
        } else {
            self.handle_shard_block(block.block(), shard_state.state(), &block.id().root_hash)?;
        }

        Ok(())
    }
}

struct StateSubscription {
    transaction_subscriptions: Vec<Weak<dyn TransactionsSubscription>>,
}

impl StateSubscription {
    fn handle_block(
        &self,
        messages_queue: &PendingMessagesQueue,
        shard_accounts: &ton_block::ShardAccounts,
        block_info: &ton_block::BlockInfo,
        account_block: &ton_block::AccountBlock,
        account: &UInt256,
        block_hash: &UInt256,
    ) -> Result<()> {
        if self.transaction_subscriptions.is_empty() {
            return Ok(());
        }

        for transaction in account_block.transactions().iter() {
            let (hash, transaction) = match transaction.and_then(|(_, value)| {
                let cell = value.into_cell().reference(0)?;
                let hash = cell.repr_hash();

                ton_block::Transaction::construct_from_cell(cell)
                    .map(|transaction| (hash, transaction))
            }) {
                Ok(tx) => tx,
                Err(e) => {
                    log::error!(
                        "Failed to parse transaction in block {} for account {}: {:?}",
                        block_info.seq_no(),
                        account.to_hex_string(),
                        e
                    );
                    continue;
                }
            };

            // Skip non-ordinary transactions
            let transaction_info = match transaction.description.read_struct() {
                Ok(ton_block::TransactionDescr::Ordinary(info)) => info,
                _ => continue,
            };

            let in_msg = match transaction
                .in_msg
                .as_ref()
                .map(|message| (message, message.read_struct()))
            {
                Some((message_cell, Ok(message))) => {
                    if matches!(message.header(), ton_block::CommonMsgInfo::ExtInMsgInfo(_)) {
                        messages_queue.deliver_message(*account, message_cell.hash());
                    }
                    message
                }
                _ => continue,
            };

            let ctx = TxContext {
                shard_accounts,
                block_info,
                block_hash,
                account,
                transaction_hash: &hash,
                transaction_info: &transaction_info,
                transaction: &transaction,
                in_msg: &in_msg,
                token_transaction: &None,
            };

            // Handle transaction
            for subscription in self.iter_transaction_subscriptions() {
                if let Err(e) = subscription.handle_transaction(ctx) {
                    log::error!(
                        "Failed to handle transaction {} for account {}: {:?}",
                        hash.to_hex_string(),
                        account.to_hex_string(),
                        e
                    );
                }
            }
        }

        Ok(())
    }

    fn iter_transaction_subscriptions(
        &'_ self,
    ) -> impl Iterator<Item = Arc<dyn TransactionsSubscription>> + '_ {
        self.transaction_subscriptions
            .iter()
            .map(Weak::upgrade)
            .flatten()
    }
}

struct TokenSubscription {
    transaction_subscription: Arc<dyn TransactionsSubscription>,
}

impl TokenSubscription {
    fn handle_block(
        &self,
        state_subscriptions: &RwLock<HashMap<UInt256, StateSubscription>>,
        shard_accounts: &ton_block::ShardAccounts,
        block_info: &ton_block::BlockInfo,
        account_block: &ton_block::AccountBlock,
        account: &UInt256,
        block_hash: &UInt256,
    ) -> Result<()> {
        for transaction in account_block.transactions().iter() {
            let (hash, transaction) = match transaction.and_then(|(_, value)| {
                let cell = value.into_cell().reference(0)?;
                let hash = cell.repr_hash();
                ton_block::Transaction::construct_from_cell(cell)
                    .map(|transaction| (hash, transaction))
            }) {
                Ok(tx) => tx,
                Err(e) => {
                    log::error!(
                        "Failed to parse transaction in block {} for account {}: {:?}",
                        block_info.seq_no(),
                        account.to_hex_string(),
                        e
                    );
                    continue;
                }
            };

            // Skip non-ordinary transactions
            let transaction_info = match transaction.description.read_struct() {
                Ok(ton_block::TransactionDescr::Ordinary(info)) => info,
                _ => continue,
            };

            let parsed_token_transaction = nekoton::core::parsing::parse_token_transaction(
                &transaction,
                &transaction_info,
                TokenWalletVersion::Tip3v4,
            );

            if let Some(parsed) = parsed_token_transaction {
                let address = MsgAddressInt::with_standart(
                    None,
                    ton_block::BASE_WORKCHAIN_ID as i8,
                    ton_types::AccountId::from(account),
                )?;

                let (token_wallet, _) = get_token_wallet_details(&address, shard_accounts)?;
                let owner =
                    UInt256::from_be_bytes(&token_wallet.owner_address.address().get_bytestring(0));

                if state_subscriptions.read().get(&owner).is_some() {
                    let in_msg = match transaction
                        .in_msg
                        .as_ref()
                        .map(|message| (message, message.read_struct()))
                    {
                        Some((_, Ok(message))) => message,
                        _ => continue,
                    };

                    let ctx = TxContext {
                        shard_accounts,
                        block_info,
                        block_hash,
                        account,
                        transaction_hash: &hash,
                        transaction_info: &transaction_info,
                        transaction: &transaction,
                        in_msg: &in_msg,
                        token_transaction: &Some(parsed),
                    };

                    if let Err(e) = self.transaction_subscription.handle_transaction(ctx) {
                        log::error!(
                            "Failed to handle transaction {} for account {}: {:?}",
                            hash.to_hex_string(),
                            account.to_hex_string(),
                            e
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

trait BlockAwaiter: Send + Sync {
    fn handle_block(
        &mut self,
        block: &ton_block::Block,
        block_info: &ton_block::BlockInfo,
    ) -> Result<BlockAwaiterAction>;
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum BlockAwaiterAction {
    Retain,
}

pub trait TransactionsSubscription: Send + Sync {
    fn handle_transaction(&self, ctx: TxContext<'_>) -> Result<()>;
}

/// Generic listener for transactions
pub struct AccountObserver<T>(AccountEventsTx<T>);

impl<T> AccountObserver<T> {
    pub fn new(tx: AccountEventsTx<T>) -> Arc<Self> {
        Arc::new(Self(tx))
    }
}

impl<T> TransactionsSubscription for AccountObserver<T>
where
    T: ReadFromTransaction + std::fmt::Debug + Send + Sync,
{
    fn handle_transaction(&self, ctx: TxContext<'_>) -> Result<()> {
        let event = T::read_from_transaction(&ctx);

        // Send event to event manager if it exist
        if let Some(event) = event {
            if self.0.send(event).is_err() {
                log::error!("Failed to send event: channel is dropped");
            }
        }

        // Done
        Ok(())
    }
}

pub type AccountEventsTx<T> = mpsc::UnboundedSender<T>;
