use anyhow::{Context, Result};
use argh::FromArgs;
use futures::prelude::*;

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
            let mut config = config::Config::new();
            config.merge(read_config(app.config).context("Failed to read config")?)?;
            config.merge(config::Environment::new())?;

            run.execute(config.try_into()?).await
        }
        Subcommand::RootToken(run) => run.execute().await,
        Subcommand::ApiService(run) => run.execute().await,
        Subcommand::ApiServiceKey(run) => run.execute().await,
    }
}

#[derive(Debug, PartialEq, FromArgs)]
#[argh(description = "")]
struct App {
    #[argh(subcommand)]
    command: Subcommand,

    /// path to config file ('config.yaml' by default)
    #[argh(option, short = 'c', default = "String::from(\"config.yaml\")")]
    config: String,
}

#[derive(Debug, PartialEq, FromArgs)]
#[argh(subcommand)]
enum Subcommand {
    Server(CmdServer),
    RootToken(CmdRootToken),
    ApiService(CmdApiService),
    ApiServiceKey(CmdApiServiceKey),
}

#[derive(Debug, PartialEq, FromArgs)]
/// Starts relay node
#[argh(subcommand, name = "server")]
struct CmdServer {
    /// path to global config file
    #[argh(option, short = 'g')]
    global_config: String,
}

impl CmdServer {
    async fn execute(self, config: AppConfig) -> Result<()> {
        let global_config = ton_indexer::GlobalConfig::from_file(&self.global_config)
            .context("Failed to open global config")?;

        init_logger(&config.logger_settings).context("Failed to init logger")?;

        server_run(config, global_config).await?;

        future::pending().await
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
    /// service name
    #[argh(option, short = 'n')]
    name: String,
    /// service id
    #[argh(option, short = 'i')]
    id: Option<String>,
}

impl CmdApiService {
    async fn execute(self) -> Result<()> {
        create_api_service(self.name, self.id).await
    }
}

#[derive(Debug, PartialEq, FromArgs)]
/// Create a new api service key
#[argh(subcommand, name = "api_service_key")]
struct CmdApiServiceKey {
    /// service id
    #[argh(option, short = 'i')]
    id: String,
    /// service key
    #[argh(option, short = 'k')]
    key: String,
    /// service secret
    #[argh(option, short = 's')]
    secret: String,
}

impl CmdApiServiceKey {
    async fn execute(self) -> Result<()> {
        create_api_service_key(self.id, self.key, self.secret).await
    }
}

fn read_config<P>(path: P) -> Result<config::File<config::FileSourceString>>
where
    P: AsRef<std::path::Path>,
{
    let data = std::fs::read_to_string(path)?;
    let re = regex::Regex::new(r"\$\{([a-zA-Z_][0-9a-zA-Z_]*)\}").unwrap();
    let result = re.replace_all(&data, |caps: &regex::Captures| {
        match std::env::var(&caps[1]) {
            Ok(value) => value,
            Err(_) => {
                log::warn!("Environment variable {} was not set", &caps[1]);
                String::default()
            }
        }
    });

    Ok(config::File::from_str(
        result.as_ref(),
        config::FileFormat::Yaml,
    ))
}

fn init_logger(config: &serde_yaml::Value) -> Result<()> {
    let config = serde_yaml::from_value(config.clone())?;
    log4rs::config::init_raw_config(config)?;
    Ok(())
}
