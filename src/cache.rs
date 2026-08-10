use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use tokio::fs;

const MAX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DiskCache {
    root: PathBuf,
    max_bytes: u64,
    client: reqwest::Client,
}

impl DiskCache {
    pub fn new(root: PathBuf, max_bytes: u64, client: reqwest::Client) -> Self {
        Self {
            root,
            max_bytes,
            client,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn get_or_fetch(&self, key: &str, url: &str) -> Result<Vec<u8>> {
        fs::create_dir_all(&self.root)
            .await
            .context("create image cache directory")?;
        let path = self.path_for(key);
        if let Ok(bytes) = fs::read(&path).await {
            return Ok(bytes);
        }

        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("download emote")?
            .error_for_status()
            .context("emote server returned an error")?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_DOWNLOAD_BYTES as u64)
        {
            bail!("emote is larger than {MAX_DOWNLOAD_BYTES} bytes");
        }
        let bytes = response.bytes().await.context("read emote response")?;
        if bytes.len() > MAX_DOWNLOAD_BYTES {
            bail!("emote is larger than {MAX_DOWNLOAD_BYTES} bytes");
        }

        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&temporary, &bytes)
            .await
            .context("write cached emote")?;
        if let Err(error) = fs::rename(&temporary, &path).await {
            let _ = fs::remove_file(&temporary).await;
            if !path.exists() {
                return Err(error).context("commit cached emote");
            }
        }
        self.prune().await?;
        Ok(bytes.to_vec())
    }

    pub async fn prune(&self) -> Result<()> {
        let mut entries = Vec::new();
        let mut total = 0_u64;
        let mut directory = match fs::read_dir(&self.root).await {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("read image cache"),
        };
        while let Some(entry) = directory.next_entry().await.context("read cache entry")? {
            let metadata = entry.metadata().await.context("read cache metadata")?;
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total = total.saturating_add(metadata.len());
            entries.push((modified, metadata.len(), entry.path()));
        }
        if total <= self.max_bytes {
            return Ok(());
        }
        entries.sort_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in entries {
            fs::remove_file(path).await.context("prune cached emote")?;
            total = total.saturating_sub(size);
            if total <= self.max_bytes {
                break;
            }
        }
        Ok(())
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let safe: String = key
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(format!("{safe}.img"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prune_removes_oldest_files_until_under_limit() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("old.img"), vec![0; 8])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        fs::write(directory.path().join("new.img"), vec![0; 8])
            .await
            .unwrap();
        let cache = DiskCache::new(directory.path().to_owned(), 8, reqwest::Client::new());

        cache.prune().await.unwrap();

        assert!(!directory.path().join("old.img").exists());
        assert!(directory.path().join("new.img").exists());
    }
}
