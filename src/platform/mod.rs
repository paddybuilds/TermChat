use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::model::ChatEvent;

pub mod kick;
pub mod twitch;

#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn display_target(&self) -> &str;

    async fn run(
        &self,
        events: mpsc::Sender<ChatEvent>,
        cancellation: CancellationToken,
    ) -> Result<()>;
}
