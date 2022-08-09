use std::sync::Arc;

use anyhow::{Context, Result};
use argh::FromArgs;
use tokio::sync::mpsc;

use ton_wallet_api::commands::*;
use ton_wallet_api::server::*;
use ton_wallet_api::settings::*;

#[global_allocator]
static GLOBAL: ton_indexer::alloc::Allocator = ton_indexer::alloc::allocator();

#[tokio::main]
async fn main() -> Result<()> {
    run(argh::from_env()).await
}

async fn run(app: App) -> Result<()> {
    match app.command {
        Subcommand::Server(run) => {
            let config: AppConfig = broxus_util::read_config(&run.config)?;
            run.execute(config).await
        }
        Subcommand::RootToken(run) => run.execute().await,
        Subcommand::ApiService(run) => run.execute().await,
    }
}

#[derive(Debug, PartialEq, FromArgs)]
#[argh(description = "")]
struct App {
    #[argh(subcommand)]
    command: Subcommand,
}

#[derive(Debug, PartialEq, FromArgs)]
#[argh(subcommand)]
enum Subcommand {
    Server(CmdServer),
    RootToken(CmdRootToken),
    ApiService(CmdApiService),
}

#[derive(Debug, PartialEq, FromArgs)]
/// Starts relay node
#[argh(subcommand, name = "server")]
struct CmdServer {
    /// path to config file ('config.yaml' by default)
    #[argh(option, short = 'c', default = "String::from(\"config.yaml\")")]
    config: String,

    /// path to global config file
    #[argh(option, short = 'g')]
    global_config: String,
}

impl CmdServer {
    async fn execute(self, config: AppConfig) -> Result<()> {
        let ton_wallet_api = Arc::new(TonWalletApi {
            engine: Default::default(),
        });

        let global_config = ton_indexer::GlobalConfig::from_file(&self.global_config)
            .context("Failed to open global config")?;

        broxus_util::init_logger(&config.logger_settings).context("Failed to init logger")?;

        log::info!("Initializing ton-wallet-api...");
        let mut shutdown_requests_rx = ton_wallet_api.init(config, global_config).await?;
        log::info!("Initialized ton-wallet-api");

        shutdown_requests_rx.recv().await;
        Ok(())
    }
}

#[derive(Debug, PartialEq, FromArgs)]
/// Add root token address
#[argh(subcommand, name = "root_token")]
struct CmdRootToken {
    /// root token name
    #[argh(option, short = 'n')]
    name: String,
    /// root token address
    #[argh(option, short = 'a')]
    address: String,
}

impl CmdRootToken {
    async fn execute(self) -> Result<()> {
        add_root_token(self.name, self.address).await
    }
}

#[derive(Debug, PartialEq, FromArgs)]
/// Create a new api service
#[argh(subcommand, name = "api_service")]
struct CmdApiService {
    /// service id
    #[argh(option, short = 'i')]
    id: Option<String>,
    /// service name
    #[argh(option, short = 'n')]
    name: String,
    /// service key
    #[argh(option, short = 'k')]
    key: String,
    /// service secret
    #[argh(option, short = 's')]
    secret: String,
}

impl CmdApiService {
    async fn execute(self) -> Result<()> {
        create_api_service(self.id, self.name, self.key, self.secret).await
    }
}

struct TonWalletApi {
    engine: tokio::sync::Mutex<Option<Arc<Engine>>>,
}

impl TonWalletApi {
    async fn init(
        &self,
        config: AppConfig,
        global_config: ton_indexer::GlobalConfig,
    ) -> Result<ShutdownRequestsRx> {
        let (shutdown_requests_tx, shutdown_requests_rx) = mpsc::unbounded_channel();

        let engine = Engine::new(config, global_config, shutdown_requests_tx)
            .await
            .context("Failed to create engine")?;
        *self.engine.lock().await = Some(engine.clone());

        engine.start().await.context("Failed to start engine")?;

        Ok(shutdown_requests_rx)
    }
}
