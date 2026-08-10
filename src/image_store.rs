use std::{collections::HashSet, num::NonZeroUsize, path::PathBuf, sync::Arc};

use directories::ProjectDirs;
use image::DynamicImage;
use lru::LruCache;
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};
use tokio::sync::mpsc;

use crate::{cache::DiskCache, model::EmoteRef, tls::install_crypto_provider};

pub const EMOTE_WIDTH: u16 = 4;
pub const EMOTE_HEIGHT: u16 = 2;
const DISK_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const DECODED_CACHE_ITEMS: usize = 128;

pub struct ImageStore {
    picker: Option<Picker>,
    cache: Arc<DiskCache>,
    protocols: LruCache<String, Protocol>,
    pending: HashSet<String>,
    completed_tx: mpsc::UnboundedSender<ImageResult>,
    completed_rx: mpsc::UnboundedReceiver<ImageResult>,
}

struct ImageResult {
    key: String,
    result: Result<Protocol, String>,
}

impl ImageStore {
    pub fn new(picker: Option<Picker>) -> Self {
        install_crypto_provider();
        let (completed_tx, completed_rx) = mpsc::unbounded_channel();
        Self {
            picker,
            cache: Arc::new(DiskCache::new(
                default_cache_dir(),
                DISK_CACHE_BYTES,
                reqwest::Client::new(),
            )),
            protocols: LruCache::new(NonZeroUsize::new(DECODED_CACHE_ITEMS).unwrap()),
            pending: HashSet::new(),
            completed_tx,
            completed_rx,
        }
    }

    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.protocols.contains(key)
    }

    pub fn protocol(&mut self, key: &str) -> Option<&Protocol> {
        self.protocols.get(key)
    }

    pub fn request(&mut self, emote: &EmoteRef) {
        if emote.animated || self.picker.is_none() {
            return;
        }
        let key = emote.cache_key();
        if self.protocols.contains(&key) || !self.pending.insert(key.clone()) {
            return;
        }

        let cache = Arc::clone(&self.cache);
        let picker = self.picker.clone().expect("picker checked above");
        let completed = self.completed_tx.clone();
        let url = emote.image_url.clone();
        tokio::spawn(async move {
            let result = async {
                let bytes = cache
                    .get_or_fetch(&key, &url)
                    .await
                    .map_err(|error| error.to_string())?;
                tokio::task::spawn_blocking(move || decode_protocol(bytes, picker))
                    .await
                    .map_err(|error| error.to_string())?
            }
            .await;
            let _ = completed.send(ImageResult { key, result });
        });
    }

    pub fn drain_completed(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        while let Ok(completed) = self.completed_rx.try_recv() {
            self.pending.remove(&completed.key);
            match completed.result {
                Ok(protocol) => {
                    self.protocols.put(completed.key, protocol);
                }
                Err(error) => errors.push(error),
            }
        }
        errors
    }
}

fn decode_protocol(bytes: Vec<u8>, picker: Picker) -> Result<Protocol, String> {
    let image: DynamicImage = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    picker
        .new_protocol(
            image,
            Size::new(EMOTE_WIDTH, EMOTE_HEIGHT),
            Resize::Fit(None),
        )
        .map_err(|error| error.to_string())
}

fn default_cache_dir() -> PathBuf {
    ProjectDirs::from("io", "termchat", "TermChat")
        .map(|directories| directories.cache_dir().join("emotes"))
        .unwrap_or_else(|| std::env::temp_dir().join("termchat-emotes"))
}
