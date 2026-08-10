use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use crate::{
    emotes::{RegistryIds, SharedEmoteRegistry},
    model::{ChatEvent, EmoteProvider, EmoteRef},
    tls::install_crypto_provider,
};

const DEFAULT_API_BASE: &str = "https://7tv.io/v3";
const DEFAULT_EVENT_URL: &str = "wss://events.7tv.io/v3?app=termchat&version=0.1.0";

#[derive(Clone, Debug)]
pub struct SevenTvClient {
    http: reqwest::Client,
    api_base: String,
    event_url: String,
}

impl Default for SevenTvClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SevenTvClient {
    pub fn new() -> Self {
        install_crypto_provider();
        Self {
            http: reqwest::Client::new(),
            api_base: DEFAULT_API_BASE.to_owned(),
            event_url: DEFAULT_EVENT_URL.to_owned(),
        }
    }

    #[cfg(test)]
    fn with_api_base(api_base: String) -> Self {
        install_crypto_provider();
        Self {
            http: reqwest::Client::new(),
            api_base,
            event_url: DEFAULT_EVENT_URL.to_owned(),
        }
    }

    pub async fn run(
        &self,
        twitch_room_id: String,
        registry: SharedEmoteRegistry,
        events: mpsc::Sender<ChatEvent>,
        cancellation: CancellationToken,
    ) {
        self.refresh_global(&registry, &events).await;
        self.refresh_channel(&twitch_room_id, &registry, &events)
            .await;

        let mut retry = Duration::from_secs(1);
        loop {
            if cancellation.is_cancelled() {
                return;
            }
            let ids = registry.snapshot_ids();
            match self
                .run_event_connection(&twitch_room_id, &registry, &events, &cancellation, ids)
                .await
            {
                Ok(()) if cancellation.is_cancelled() => return,
                Ok(()) => {}
                Err(error) => {
                    let _ = events
                        .send(ChatEvent::Status(format!(
                            "7TV updates unavailable; retrying: {error:#}"
                        )))
                        .await;
                }
            }

            let jitter = rand::rng().random_range(0..=500);
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = sleep(retry + Duration::from_millis(jitter)) => {}
            }
            retry = (retry * 2).min(Duration::from_secs(30));
            self.refresh_global(&registry, &events).await;
            self.refresh_channel(&twitch_room_id, &registry, &events)
                .await;
        }
    }

    async fn run_event_connection(
        &self,
        twitch_room_id: &str,
        registry: &SharedEmoteRegistry,
        events: &mpsc::Sender<ChatEvent>,
        cancellation: &CancellationToken,
        mut ids: RegistryIds,
    ) -> Result<()> {
        let (socket, _) = connect_async(&self.event_url)
            .await
            .context("connect to 7TV EventAPI")?;
        let (mut writer, mut reader) = socket.split();
        for subscription in subscriptions(&ids) {
            writer
                .send(Message::Text(subscription.to_string().into()))
                .await
                .context("subscribe to 7TV updates")?;
        }
        let _ = events
            .send(ChatEvent::Status("7TV emotes are live".to_owned()))
            .await;

        loop {
            let incoming = tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                incoming = reader.next() => incoming,
            };
            let Some(message) = incoming else {
                bail!("7TV EventAPI closed the connection");
            };
            let message = message.context("read 7TV event")?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).context("parse 7TV event")?;
            match value.get("op").and_then(Value::as_u64) {
                Some(0) => match classify_dispatch(&value, &ids) {
                    DispatchTarget::Global => self.refresh_global(registry, events).await,
                    DispatchTarget::ChannelSet => {
                        if let Some(set_id) = ids.channel_set_id.as_deref() {
                            self.refresh_set(set_id, false, registry, events).await;
                        }
                    }
                    DispatchTarget::User => {
                        let old_set = ids.channel_set_id.clone();
                        self.refresh_channel(twitch_room_id, registry, events).await;
                        ids = registry.snapshot_ids();
                        if ids.channel_set_id != old_set {
                            // Reconnect so the new set replaces the old subscription cleanly.
                            return Ok(());
                        }
                    }
                    DispatchTarget::Other => {}
                },
                Some(4 | 7) => return Ok(()),
                _ => {}
            }
        }
    }

    async fn refresh_global(
        &self,
        registry: &SharedEmoteRegistry,
        events: &mpsc::Sender<ChatEvent>,
    ) {
        match self.fetch_set("global").await {
            Ok(set) => registry.replace_global(set.id, set.emotes),
            Err(error) => {
                let _ = events
                    .send(ChatEvent::Status(format!(
                        "7TV global emotes unavailable: {error:#}"
                    )))
                    .await;
            }
        }
    }

    async fn refresh_channel(
        &self,
        twitch_room_id: &str,
        registry: &SharedEmoteRegistry,
        events: &mpsc::Sender<ChatEvent>,
    ) {
        match self.fetch_channel(twitch_room_id).await {
            Ok(Some(channel)) => {
                registry.replace_channel(channel.user_id, channel.set.id, channel.set.emotes)
            }
            Ok(None) => {
                let _ = events
                    .send(ChatEvent::Status(
                        "This channel has no 7TV emote set".to_owned(),
                    ))
                    .await;
            }
            Err(error) => {
                let _ = events
                    .send(ChatEvent::Status(format!(
                        "7TV channel emotes unavailable: {error:#}"
                    )))
                    .await;
            }
        }
    }

    async fn refresh_set(
        &self,
        set_id: &str,
        global: bool,
        registry: &SharedEmoteRegistry,
        events: &mpsc::Sender<ChatEvent>,
    ) {
        match self.fetch_set(set_id).await {
            Ok(set) if global => registry.replace_global(set.id, set.emotes),
            Ok(set) => {
                if let Some(user_id) = registry.snapshot_ids().user_id {
                    registry.replace_channel(user_id, set.id, set.emotes);
                }
            }
            Err(error) => {
                let _ = events
                    .send(ChatEvent::Status(format!("7TV refresh failed: {error:#}")))
                    .await;
            }
        }
    }

    async fn fetch_set(&self, set_id: &str) -> Result<ParsedSet> {
        let value = self
            .http
            .get(format!("{}/emote-sets/{set_id}", self.api_base))
            .send()
            .await
            .context("request 7TV emote set")?
            .error_for_status()
            .context("7TV emote-set request failed")?
            .json::<Value>()
            .await
            .context("decode 7TV emote set")?;
        parse_set(&value)
    }

    async fn fetch_channel(&self, twitch_room_id: &str) -> Result<Option<ParsedChannel>> {
        let response = self
            .http
            .get(format!("{}/users/twitch/{twitch_room_id}", self.api_base))
            .send()
            .await
            .context("request 7TV channel")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value = response
            .error_for_status()
            .context("7TV channel request failed")?
            .json::<Value>()
            .await
            .context("decode 7TV channel")?;
        let user_id = value
            .pointer("/user/id")
            .and_then(Value::as_str)
            .or_else(|| value.get("id").and_then(Value::as_str))
            .context("7TV channel response has no user ID")?
            .to_owned();
        let set = parse_set(
            value
                .get("emote_set")
                .context("7TV channel response has no emote set")?,
        )?;
        Ok(Some(ParsedChannel { user_id, set }))
    }
}

#[derive(Debug)]
struct ParsedChannel {
    user_id: String,
    set: ParsedSet,
}

#[derive(Debug)]
struct ParsedSet {
    id: String,
    emotes: Vec<EmoteRef>,
}

fn parse_set(value: &Value) -> Result<ParsedSet> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("global")
        .to_owned();
    let emotes = value
        .get("emotes")
        .and_then(Value::as_array)
        .context("7TV set has no emote list")?
        .iter()
        .filter_map(parse_emote)
        .collect();
    Ok(ParsedSet { id, emotes })
}

fn parse_emote(value: &Value) -> Option<EmoteRef> {
    let id = value.get("id")?.as_str()?.to_owned();
    let name = value.get("name")?.as_str()?.to_owned();
    let data = value.get("data")?;
    let animated = data
        .get("animated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let host = data.pointer("/host/url")?.as_str()?;
    let files = data.pointer("/host/files")?.as_array()?;
    let file_name = ["2x.webp", "1x.webp"]
        .into_iter()
        .find(|wanted| {
            files
                .iter()
                .any(|file| file.get("name").and_then(Value::as_str) == Some(*wanted))
        })
        .or_else(|| {
            files.iter().find_map(|file| {
                file.get("name")
                    .and_then(Value::as_str)
                    .filter(|name| name.ends_with(".webp"))
            })
        })?;
    let host = if host.starts_with("//") {
        format!("https:{host}")
    } else {
        host.to_owned()
    };
    Some(EmoteRef {
        provider: EmoteProvider::SevenTv,
        id,
        name,
        image_url: format!("{host}/{file_name}"),
        animated,
    })
}

fn subscriptions(ids: &RegistryIds) -> Vec<Value> {
    let mut result = Vec::new();
    if let Some(id) = &ids.global_set_id {
        result.push(subscription("emote_set.update", id));
    }
    if let Some(id) = &ids.channel_set_id {
        result.push(subscription("emote_set.update", id));
    }
    if let Some(id) = &ids.user_id {
        result.push(subscription("user.update", id));
    }
    result
}

fn subscription(kind: &str, object_id: &str) -> Value {
    json!({
        "op": 35,
        "d": {
            "type": kind,
            "condition": { "object_id": object_id }
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchTarget {
    Global,
    ChannelSet,
    User,
    Other,
}

fn classify_dispatch(value: &Value, ids: &RegistryIds) -> DispatchTarget {
    let dispatch_type = value.pointer("/d/type").and_then(Value::as_str);
    let object_id = value
        .pointer("/d/body/id")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/d/body/object/id").and_then(Value::as_str));
    match (dispatch_type, object_id) {
        (Some("emote_set.update"), Some(id)) if ids.global_set_id.as_deref() == Some(id) => {
            DispatchTarget::Global
        }
        (Some("emote_set.update"), Some(id)) if ids.channel_set_id.as_deref() == Some(id) => {
            DispatchTarget::ChannelSet
        }
        (Some("user.update"), Some(id)) if ids.user_id.as_deref() == Some(id) => {
            DispatchTarget::User
        }
        _ => DispatchTarget::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn emote_json() -> Value {
        json!({
            "id": "01ABC",
            "name": "Wave",
            "data": {
                "animated": false,
                "host": {
                    "url": "//cdn.7tv.app/emote/01ABC",
                    "files": [{"name": "1x.webp"}, {"name": "2x.webp"}]
                }
            }
        })
    }

    #[test]
    fn parses_emote_set_and_prefers_two_x_webp() {
        let set = parse_set(&json!({"id": "set", "emotes": [emote_json()]})).unwrap();
        assert_eq!(set.id, "set");
        assert_eq!(set.emotes[0].name, "Wave");
        assert_eq!(
            set.emotes[0].image_url,
            "https://cdn.7tv.app/emote/01ABC/2x.webp"
        );
    }

    #[test]
    fn classifies_live_update_targets() {
        let ids = RegistryIds {
            user_id: Some("user".to_owned()),
            global_set_id: Some("global".to_owned()),
            channel_set_id: Some("channel".to_owned()),
        };
        let event = json!({"op": 0, "d": {"type": "emote_set.update", "body": {"id": "channel"}}});
        assert_eq!(classify_dispatch(&event, &ids), DispatchTarget::ChannelSet);
    }

    #[tokio::test]
    async fn fetches_channel_from_configurable_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/twitch/123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "user": {"id": "user"},
                "emote_set": {"id": "set", "emotes": [emote_json()]}
            })))
            .mount(&server)
            .await;
        let client = SevenTvClient::with_api_base(server.uri());

        let channel = client.fetch_channel("123").await.unwrap().unwrap();

        assert_eq!(channel.user_id, "user");
        assert_eq!(channel.set.emotes.len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires live 7TV connectivity"]
    async fn live_api_and_eventapi_smoke() {
        let client = SevenTvClient::new();
        let global = client.fetch_set("global").await.unwrap();
        let channel = client.fetch_channel("11148817").await.unwrap().unwrap();
        assert!(!global.emotes.is_empty());
        assert!(!channel.set.emotes.is_empty());

        let registry = SharedEmoteRegistry::default();
        registry.replace_global(global.id, global.emotes);
        registry.replace_channel(channel.user_id, channel.set.id, channel.set.emotes);
        let (events, _received) = mpsc::channel(8);
        let cancellation = CancellationToken::new();
        let timer_cancellation = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(2)).await;
            timer_cancellation.cancel();
        });

        client
            .run_event_connection(
                "11148817",
                &registry,
                &events,
                &cancellation,
                registry.snapshot_ids(),
            )
            .await
            .unwrap();
    }
}
