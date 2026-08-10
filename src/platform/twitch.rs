use std::{sync::Mutex, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use rand::Rng;
use tokio::{sync::mpsc, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use twitch_irc::{
    ClientConfig, SecureTCPTransport, TwitchIRCClient,
    login::StaticLoginCredentials,
    message::{PrivmsgMessage, ServerMessage},
};

use crate::{
    emotes::SharedEmoteRegistry,
    model::{ChatEvent, ChatMessage, RgbColor},
    platform::PlatformAdapter,
    seventv::SevenTvClient,
    target::TwitchTarget,
    tls::install_crypto_provider,
};

pub struct TwitchAdapter {
    target: TwitchTarget,
    emotes: SharedEmoteRegistry,
    seventv_task: Mutex<Option<JoinHandle<()>>>,
}

impl TwitchAdapter {
    pub fn new(target: TwitchTarget, emotes: SharedEmoteRegistry) -> Self {
        install_crypto_provider();
        Self {
            target,
            emotes,
            seventv_task: Mutex::new(None),
        }
    }

    fn start_seventv_once(
        &self,
        room_id: String,
        events: mpsc::Sender<ChatEvent>,
        cancellation: CancellationToken,
    ) {
        let mut task = self
            .seventv_task
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if task.is_some() {
            return;
        }
        let registry = self.emotes.clone();
        let client = SevenTvClient::new();
        *task = Some(tokio::spawn(async move {
            client.run(room_id, registry, events, cancellation).await;
        }));
    }
}

#[async_trait]
impl PlatformAdapter for TwitchAdapter {
    fn display_target(&self) -> &str {
        self.target.channel()
    }

    async fn run(
        &self,
        events: mpsc::Sender<ChatEvent>,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let mut retry = Duration::from_secs(1);
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            events
                .send(ChatEvent::Status(format!(
                    "Connecting anonymously to #{}",
                    self.target.channel()
                )))
                .await
                .ok();

            let config = ClientConfig::new_simple(StaticLoginCredentials::anonymous());
            let (mut incoming, client) =
                TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);
            client
                .join(self.target.channel().to_owned())
                .context("join Twitch channel")?;

            let mut announced_connected = false;
            loop {
                let message = tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    message = incoming.recv() => message,
                };
                let Some(message) = message else {
                    break;
                };
                match message {
                    ServerMessage::RoomState(state) => {
                        if !announced_connected {
                            events.send(ChatEvent::Connected).await.ok();
                            announced_connected = true;
                            retry = Duration::from_secs(1);
                        }
                        self.start_seventv_once(
                            state.channel_id,
                            events.clone(),
                            cancellation.clone(),
                        );
                    }
                    ServerMessage::Privmsg(message) => {
                        if !announced_connected {
                            events.send(ChatEvent::Connected).await.ok();
                            announced_connected = true;
                            retry = Duration::from_secs(1);
                        }
                        self.start_seventv_once(
                            message.channel_id.clone(),
                            events.clone(),
                            cancellation.clone(),
                        );
                        let message = convert_message(message, &self.emotes);
                        if events.send(ChatEvent::Message(message)).await.is_err() {
                            return Ok(());
                        }
                    }
                    ServerMessage::Reconnect(_) => {
                        events
                            .send(ChatEvent::Status("Twitch requested a reconnect".to_owned()))
                            .await
                            .ok();
                    }
                    ServerMessage::Notice(notice) => {
                        let lower = notice.message_text.to_ascii_lowercase();
                        if lower.contains("authentication failed")
                            || lower.contains("improperly formatted auth")
                        {
                            events
                                .send(ChatEvent::Status(
                                    "Twitch rejected anonymous chat access".to_owned(),
                                ))
                                .await
                                .ok();
                        } else {
                            events
                                .send(ChatEvent::Status(notice.message_text))
                                .await
                                .ok();
                        }
                    }
                    _ => {}
                }
            }

            events
                .send(ChatEvent::Disconnected {
                    reason: "Twitch connection closed".to_owned(),
                })
                .await
                .ok();
            let jitter = rand::rng().random_range(0..=500);
            events
                .send(ChatEvent::Status(format!(
                    "Reconnecting in {:.1}s",
                    (retry + Duration::from_millis(jitter)).as_secs_f32()
                )))
                .await
                .ok();
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = sleep(retry + Duration::from_millis(jitter)) => {}
            }
            retry = (retry * 2).min(Duration::from_secs(30));
        }
    }
}

fn convert_message(message: PrivmsgMessage, emotes: &SharedEmoteRegistry) -> ChatMessage {
    let color = message.name_color.map(|color| RgbColor {
        red: color.r,
        green: color.g,
        blue: color.b,
    });
    ChatMessage {
        id: message.message_id,
        sender: message.sender.name,
        color,
        fragments: emotes.parse_message(&message.message_text, &message.emotes),
    }
}

#[cfg(test)]
mod tests {
    use std::convert::TryFrom;

    use twitch_irc::message::{IRCMessage, PrivmsgMessage};

    use super::*;
    use crate::model::{ChatFragment, EmoteProvider};

    #[test]
    fn converts_irc_message_and_twitch_emote() {
        let raw = "@badge-info=;badges=;color=#1E90FF;display-name=Tester;emotes=25:3-7;flags=;id=message-id;room-id=123;subscriber=0;tmi-sent-ts=1594545155039;turbo=0;user-id=29803735;user-type= :tester!tester@tester.tmi.twitch.tv PRIVMSG #channel :hi Kappa";
        let irc = IRCMessage::parse(raw).unwrap();
        let message = PrivmsgMessage::try_from(irc).unwrap();

        let converted = convert_message(message, &SharedEmoteRegistry::default());

        assert_eq!(converted.sender, "Tester");
        assert_eq!(converted.color.unwrap().blue, 0xFF);
        assert!(matches!(
            &converted.fragments[1],
            ChatFragment::Emote(emote) if emote.provider == EmoteProvider::Twitch
        ));
    }

    #[tokio::test]
    #[ignore = "requires live Twitch connectivity"]
    async fn anonymous_live_smoke() {
        let target = TwitchTarget::parse("twitchdev").unwrap();
        let adapter = TwitchAdapter::new(target, SharedEmoteRegistry::default());
        let (events, mut received) = mpsc::channel(32);
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { adapter.run(events, task_cancellation).await });

        let connected = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(event) = received.recv().await {
                if matches!(event, ChatEvent::Connected) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);

        cancellation.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(connected, "Twitch did not accept the anonymous connection");
    }
}
