use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use termchat_live::{
    cli::{Cli, Command},
    emotes::SharedEmoteRegistry,
    platform::{PlatformAdapter, twitch::TwitchAdapter},
    tui,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Twitch { targets, images } => {
            let adapters: Vec<Arc<dyn PlatformAdapter>> = targets
                .into_iter()
                .map(|target| {
                    Arc::new(TwitchAdapter::new(target, SharedEmoteRegistry::default()))
                        as Arc<dyn PlatformAdapter>
                })
                .collect();
            tui::run(adapters, images).await
        }
    }
}
