use std::{collections::HashSet, sync::Mutex, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{process::Command, sync::mpsc, task::JoinHandle, time::sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use crate::{
    emotes::SharedEmoteRegistry,
    model::{ChatBadge, ChatEvent, ChatMessage, ModerationEvent, RgbColor},
    platform::PlatformAdapter,
    seventv::{SevenTvClient, SevenTvPlatform},
    target::KickTarget,
    text::sanitize_display_name,
    tls::install_crypto_provider,
};

const PUSHER_URL: &str = "wss://ws-us2.pusher.com/app/32cbd69e4b950bf97679?protocol=7&client=js&version=8.4.0&flash=false";

pub struct KickAdapter {
    target: KickTarget,
    emotes: SharedEmoteRegistry,
    seventv_task: Mutex<Option<JoinHandle<()>>>,
}

impl KickAdapter {
    pub fn new(target: KickTarget, emotes: SharedEmoteRegistry) -> Self {
        install_crypto_provider();
        Self {
            target,
            emotes,
            seventv_task: Mutex::new(None),
        }
    }

    fn start_seventv_once(
        &self,
        user_id: u64,
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
        *task = Some(tokio::spawn(async move {
            SevenTvClient::new()
                .run(
                    SevenTvPlatform::Kick,
                    user_id.to_string(),
                    registry,
                    events,
                    cancellation,
                )
                .await;
        }));
    }

    async fn run_connection(
        &self,
        channel: &ResolvedChannel,
        events: &mpsc::Sender<ChatEvent>,
        cancellation: &CancellationToken,
        retry: &mut Duration,
    ) -> Result<()> {
        let (mut socket, _) = connect_async(PUSHER_URL)
            .await
            .context("connect to Kick chat WebSocket")?;
        let mut subscribed = false;

        loop {
            let incoming = tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                incoming = socket.next() => incoming,
            };
            let Some(frame) = incoming else {
                bail!("Kick chat WebSocket closed");
            };
            match frame.context("read Kick chat WebSocket")? {
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .context("reply to Kick ping")?;
                }
                Message::Close(_) => bail!("Kick chat WebSocket closed"),
                Message::Text(text) => {
                    let Ok(envelope) = serde_json::from_str::<PusherEnvelope>(&text) else {
                        continue;
                    };
                    match envelope.event.as_str() {
                        "pusher:connection_established" => {
                            let subscription = json!({
                                "event": "pusher:subscribe",
                                "data": {
                                    "auth": "",
                                    "channel": format!("chatrooms.{}.v2", channel.chatroom.id),
                                }
                            });
                            socket
                                .send(Message::Text(subscription.to_string().into()))
                                .await
                                .context("subscribe to Kick chatroom")?;
                        }
                        "pusher_internal:subscription_succeeded" => {
                            if !subscribed {
                                subscribed = true;
                                *retry = Duration::from_secs(1);
                                events.send(ChatEvent::Connected).await.ok();
                                self.start_seventv_once(
                                    channel.user_id,
                                    events.clone(),
                                    cancellation.clone(),
                                );
                            }
                        }
                        "pusher:ping" => {
                            socket
                                .send(Message::Text(pusher_pong().into()))
                                .await
                                .context("reply to Kick Pusher ping")?;
                        }
                        "App\\Events\\ChatMessageEvent" if subscribed => {
                            if let Some(payload) = decode_chat_payload(envelope.data)
                                && events
                                    .send(ChatEvent::Message(convert_message(
                                        payload,
                                        &self.emotes,
                                    )))
                                    .await
                                    .is_err()
                            {
                                return Ok(());
                            }
                        }
                        "App\\Events\\MessageDeletedEvent"
                        | "App\\Events\\ChatMessageDeletedEvent"
                            if subscribed =>
                        {
                            if let Some(event) = decode_deleted_event(envelope.data) {
                                events.send(ChatEvent::Moderation(event)).await.ok();
                            }
                        }
                        "App\\Events\\UserBannedEvent" | "App\\Events\\ChatroomBanEvent"
                            if subscribed =>
                        {
                            if let Some(event) = decode_banned_event(envelope.data) {
                                events.send(ChatEvent::Moderation(event)).await.ok();
                            }
                        }
                        "App\\Events\\UserUnbannedEvent" if subscribed => {
                            if let Some(event) = decode_unbanned_event(envelope.data) {
                                events.send(ChatEvent::Moderation(event)).await.ok();
                            }
                        }
                        "App\\Events\\ChatroomClearEvent" if subscribed => {
                            let moderator = decode_event_value(envelope.data)
                                .as_ref()
                                .and_then(|value| actor_name(value, "cleared_by"));
                            events
                                .send(ChatEvent::Moderation(ModerationEvent::ChatCleared {
                                    moderator,
                                }))
                                .await
                                .ok();
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn pusher_pong() -> String {
    json!({"event": "pusher:pong", "data": {}}).to_string()
}

#[async_trait]
impl PlatformAdapter for KickAdapter {
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
                    "Resolving Kick channel {}",
                    self.target.channel()
                )))
                .await
                .ok();

            let attempt = async {
                let channel = resolve_channel(self.target.channel()).await?;
                events
                    .send(ChatEvent::Status(format!(
                        "Connecting anonymously to Kick #{}",
                        channel.slug
                    )))
                    .await
                    .ok();
                self.run_connection(&channel, &events, &cancellation, &mut retry)
                    .await
            }
            .await;

            if cancellation.is_cancelled() {
                return Ok(());
            }
            let reason = attempt
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "Kick connection closed".to_owned());
            events
                .send(ChatEvent::Disconnected {
                    reason: reason.clone(),
                })
                .await
                .ok();
            let jitter = rand::rng().random_range(0..=500);
            let delay = retry + Duration::from_millis(jitter);
            events
                .send(ChatEvent::Status(format!(
                    "{reason}; reconnecting in {:.1}s",
                    delay.as_secs_f32()
                )))
                .await
                .ok();
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = sleep(delay) => {}
            }
            retry = (retry * 2).min(Duration::from_secs(30));
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResolvedChannel {
    slug: String,
    user_id: u64,
    chatroom: ResolvedChatroom,
}

#[derive(Debug, Deserialize)]
struct ResolvedChatroom {
    id: u64,
}

async fn resolve_channel(slug: &str) -> Result<ResolvedChannel> {
    let url = format!("https://kick.com/api/v2/channels/{slug}");
    let mut command = Command::new("curl");
    command.kill_on_drop(true).args([
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--max-time",
        "15",
        "--header",
        "Accept: application/json",
        "--user-agent",
        "Chatterino7",
        &url,
    ]);
    #[cfg(target_os = "windows")]
    {
        command.creation_flags(0x08000000);
    }
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("Kick channel lookup timed out")?
        .context("run curl for Kick channel lookup; ensure curl is installed and on PATH")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "Kick channel lookup failed{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    let channel: ResolvedChannel =
        serde_json::from_slice(&output.stdout).context("decode Kick channel lookup response")?;
    if channel.chatroom.id == 0 || channel.user_id == 0 {
        bail!("Kick channel lookup returned invalid identifiers");
    }
    Ok(channel)
}

#[derive(Debug, Deserialize)]
struct PusherEnvelope {
    event: String,
    #[serde(default)]
    data: Value,
}

#[derive(Debug, Deserialize)]
struct KickChatPayload {
    id: String,
    content: String,
    sender: KickSender,
}

#[derive(Debug, Deserialize)]
struct KickSender {
    #[serde(default)]
    id: Value,
    username: String,
    #[serde(default)]
    identity: KickIdentity,
}

#[derive(Debug, Default, Deserialize)]
struct KickIdentity {
    #[serde(default)]
    color: String,
    #[serde(default)]
    badges: Vec<KickLegacyBadge>,
    #[serde(default)]
    badges_v2: Vec<KickBadgeV2>,
}

#[derive(Debug, Deserialize)]
struct KickLegacyBadge {
    #[serde(rename = "type")]
    name: String,
    #[serde(default)]
    count: u64,
}

#[derive(Debug, Deserialize)]
struct KickBadgeV2 {
    name: String,
    #[serde(default = "selected_by_default")]
    selected: bool,
    #[serde(default)]
    metadata: Value,
}

const fn selected_by_default() -> bool {
    true
}

fn decode_chat_payload(data: Value) -> Option<KickChatPayload> {
    match data {
        Value::String(data) => serde_json::from_str(&data).ok(),
        data => serde_json::from_value(data).ok(),
    }
}

fn convert_message(message: KickChatPayload, emotes: &SharedEmoteRegistry) -> ChatMessage {
    let badges = convert_badges(&message.sender.identity);
    ChatMessage {
        id: message.id,
        sender_id: scalar_string(&message.sender.id),
        sender: sanitize_display_name(&message.sender.username),
        color: RgbColor::from_hex(&message.sender.identity.color),
        badges,
        fragments: emotes.parse_kick_message(&message.content),
        moderation: None,
    }
}

fn convert_badges(identity: &KickIdentity) -> Vec<ChatBadge> {
    let mut seen = HashSet::new();
    identity
        .badges
        .iter()
        .map(|badge| ChatBadge {
            name: badge.name.clone(),
            version: (badge.count > 0).then(|| badge.count.to_string()),
        })
        .chain(
            identity
                .badges_v2
                .iter()
                .filter(|badge| badge.selected)
                .map(|badge| ChatBadge {
                    name: badge.name.clone(),
                    version: badge.metadata.get("level").and_then(scalar_string),
                }),
        )
        .filter(|badge| seen.insert(badge.name.to_ascii_lowercase()))
        .collect()
}

fn decode_event_value(data: Value) -> Option<Value> {
    match data {
        Value::String(data) => serde_json::from_str(&data).ok(),
        value @ Value::Object(_) => Some(value),
        _ => None,
    }
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |current, key| current.get(key))
}

fn first_scalar(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths
        .iter()
        .find_map(|path| nested(value, path).and_then(scalar_string))
}

fn actor_name(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(|actor| actor.get("username").or_else(|| actor.get("slug")))
        .and_then(scalar_string)
}

fn decode_deleted_event(data: Value) -> Option<ModerationEvent> {
    let value = decode_event_value(data)?;
    let message_id = first_scalar(
        &value,
        &[
            &["message", "id"],
            &["deleted_message", "id"],
            &["message_id"],
            &["id"],
        ],
    )?;
    let sender = first_scalar(
        &value,
        &[
            &["message", "sender", "username"],
            &["sender", "username"],
            &["user", "username"],
            &["username"],
        ],
    );
    let moderator = actor_name(&value, "deleted_by").or_else(|| actor_name(&value, "moderator"));
    Some(ModerationEvent::MessageDeleted {
        message_id,
        sender,
        moderator,
    })
}

fn decode_banned_event(data: Value) -> Option<ModerationEvent> {
    let value = decode_event_value(data)?;
    let user = first_scalar(
        &value,
        &[
            &["user", "username"],
            &["banned_user", "username"],
            &["username"],
        ],
    )?;
    let user_id = first_scalar(
        &value,
        &[&["user", "id"], &["banned_user", "id"], &["user_id"]],
    );
    let moderator = actor_name(&value, "banned_by").or_else(|| actor_name(&value, "moderator"));
    let duration_seconds = value
        .get("duration")
        .and_then(Value::as_u64)
        .or_else(|| value.get("duration_seconds").and_then(Value::as_u64));
    let permanent = value.get("permanent").and_then(Value::as_bool);
    if permanent == Some(false) || duration_seconds.is_some() {
        Some(ModerationEvent::UserTimedOut {
            user_id,
            user,
            duration_seconds,
            moderator,
        })
    } else {
        Some(ModerationEvent::UserBanned {
            user_id,
            user,
            moderator,
        })
    }
}

fn decode_unbanned_event(data: Value) -> Option<ModerationEvent> {
    let value = decode_event_value(data)?;
    Some(ModerationEvent::UserUnbanned {
        user_id: first_scalar(&value, &[&["user", "id"], &["user_id"]]),
        user: first_scalar(&value, &[&["user", "username"], &["username"]])?,
        moderator: actor_name(&value, "unbanned_by").or_else(|| actor_name(&value, "moderator")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatFragment, EmoteProvider};

    #[test]
    fn decodes_string_and_object_pusher_payloads() {
        let payload = json!({
            "id": "message-id",
            "content": "hello",
            "sender": {"username": "tester", "identity": {"color": "#1E90FF"}}
        });
        assert!(decode_chat_payload(Value::String(payload.to_string())).is_some());
        assert!(decode_chat_payload(payload).is_some());
        assert!(decode_chat_payload(json!({"bad": true})).is_none());
    }

    #[test]
    fn converts_message_color_and_native_emote() {
        let payload = decode_chat_payload(json!({
            "id": "message-id",
            "content": "hi [emote:123:Wave]",
            "sender": {"username": "tester", "identity": {
                "color": "#1E90FF",
                "badges": [{"type": "moderator", "text": "Moderator", "count": 0}],
                "badges_v2": [
                    {"name": "moderator", "selected": true},
                    {"name": "level", "selected": true, "metadata": {"level": 28}},
                    {"name": "subscriber", "selected": false}
                ]
            }}
        }))
        .unwrap();
        let converted = convert_message(payload, &SharedEmoteRegistry::default());
        assert_eq!(converted.sender, "tester");
        assert_eq!(converted.color.unwrap().blue, 0xFF);
        assert_eq!(
            converted
                .badges
                .iter()
                .map(|badge| badge.name.as_str())
                .collect::<Vec<_>>(),
            vec!["moderator", "level"]
        );
        assert_eq!(converted.badges[1].version.as_deref(), Some("28"));
        assert!(matches!(
            converted.fragments.last(),
            Some(ChatFragment::Emote(emote)) if emote.provider == EmoteProvider::Kick
        ));
    }

    #[test]
    fn decodes_message_deletion_payload_variants() {
        let nested = decode_deleted_event(Value::String(
            json!({
                "message": {"id": "deleted-id", "sender": {"username": "alice"}},
                "deleted_by": {"username": "modname"}
            })
            .to_string(),
        ))
        .unwrap();
        assert_eq!(
            nested,
            ModerationEvent::MessageDeleted {
                message_id: "deleted-id".to_owned(),
                sender: Some("alice".to_owned()),
                moderator: Some("modname".to_owned()),
            }
        );

        let flat = decode_deleted_event(json!({"message_id": "flat-id"})).unwrap();
        assert!(matches!(
            flat,
            ModerationEvent::MessageDeleted { message_id, .. } if message_id == "flat-id"
        ));
        assert!(decode_deleted_event(json!({"unknown": true})).is_none());
    }

    #[test]
    fn decodes_kick_timeout_ban_and_unban_events() {
        let timeout = decode_banned_event(json!({
            "user": {"id": 42, "username": "alice"},
            "banned_by": {"username": "modname"},
            "permanent": false,
            "duration": 60
        }))
        .unwrap();
        assert_eq!(
            timeout,
            ModerationEvent::UserTimedOut {
                user_id: Some("42".to_owned()),
                user: "alice".to_owned(),
                duration_seconds: Some(60),
                moderator: Some("modname".to_owned()),
            }
        );

        let ban = decode_banned_event(json!({
            "user": {"id": "42", "username": "alice"},
            "permanent": true
        }))
        .unwrap();
        assert!(matches!(ban, ModerationEvent::UserBanned { user, .. } if user == "alice"));

        let unban = decode_unbanned_event(json!({
            "user": {"id": 42, "username": "alice"},
            "unbanned_by": {"slug": "modname"}
        }))
        .unwrap();
        assert_eq!(
            unban,
            ModerationEvent::UserUnbanned {
                user_id: Some("42".to_owned()),
                user: "alice".to_owned(),
                moderator: Some("modname".to_owned()),
            }
        );
    }

    #[test]
    fn pusher_ping_response_is_a_pong() {
        let value: Value = serde_json::from_str(&pusher_pong()).unwrap();
        assert_eq!(value["event"], "pusher:pong");
        assert!(value["data"].is_object());
    }

    #[tokio::test]
    #[ignore = "requires curl and live Kick connectivity"]
    async fn live_channel_lookup_and_websocket_smoke() {
        let channel = resolve_channel("xqc").await.unwrap();
        assert!(channel.chatroom.id > 0);
        let target = KickTarget::parse("xqc").unwrap();
        let adapter = KickAdapter::new(target, SharedEmoteRegistry::default());
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
        assert!(connected, "Kick WebSocket did not subscribe");
    }
}
