use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::{StreamExt, TryStreamExt, stream};
use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

type Config = HashMap<String, Peer>;

#[derive(Deserialize, Debug)]
struct Peer {
    shared_link: String,
    sync_with: Vec<String>,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// path to the config file
    #[arg(short, long)]
    config: String,

    /// only print missing assets
    #[arg(short, long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Deserialize, Debug)]
struct SharedLink {
    album: Album,
    key: String,

    #[serde(skip)]
    base_url: String,
}

#[derive(Deserialize, Debug)]
struct Album {
    #[serde(alias = "albumName")]
    name: String,
    id: String,

    #[serde(skip)]
    assets: Vec<Asset>,
}

#[derive(Deserialize, Debug)]
struct AssetResponse {
    assets: Vec<Asset>,
}

#[derive(Deserialize, Debug)]
struct UploadResponse {
    id: String,
    // status: String,
}

#[derive(Deserialize, Debug, Eq, PartialEq, Hash, Clone)]
struct Asset {
    id: String,
    checksum: String,

    #[serde(alias = "originalFileName")]
    file_name: String,

    #[serde(alias = "deviceAssetId")]
    device_asset_id: String,

    #[serde(alias = "deviceId")]
    device_id: String,

    #[serde(alias = "fileCreatedAt")]
    file_created_at: String,

    #[serde(alias = "fileModifiedAt")]
    file_modified_at: String,

    path: Option<PathBuf>,
}

impl SharedLink {
    async fn new(shared_link: &str, client: &Client) -> Result<Self> {
        let mut s = shared_link.split("/share/");
        let base_url = s.next().context("Invalid share link")?;
        let key = s.next().context("Invalid share link")?;
        let url = format!("{base_url}/api/shared-links/me?key={key}");
        let res = client.get(url).send().await?;

        let mut shared_link = res.json::<SharedLink>().await?;
        shared_link.base_url = base_url.to_owned();
        Ok(shared_link)
    }
    async fn get_assets(&mut self, client: &Client) -> Result<()> {
        let url = format!(
            "{}/api/albums/{}?key={}",
            &self.base_url, &self.album.id, &self.key
        );
        let res = client.get(&url).send().await?;

        let asset_res = res.json::<AssetResponse>().await?;
        self.album.assets = asset_res.assets;

        Ok(())
    }

    async fn download_assets(
        &self,
        assets: &mut [Asset],
        client: &Client,
        dir: &Path,
    ) -> Result<()> {
        let mut download_stream = stream::iter(assets.iter_mut().map(|asset| {
            let url = format!(
                "{}/api/assets/{}/original?key={}&edited=true",
                self.base_url, asset.id, self.key
            );
            let asset_file_name = asset.file_name.clone();
            let dir = dir.to_path_buf();
            async move {
                let res = client.get(&url).send().await?;
                if !res.status().is_success() {
                    bail!("Download failed for {}: {}", asset_file_name, res.status());
                }
                let bytes = res.bytes().await?;

                let dest_path = dir.join(&asset_file_name);
                asset.path = Some(dest_path.clone());
                let mut dest_file = File::create(&dest_path)?;
                dest_file.write_all(&bytes)?;

                Ok(())
            }
        }))
        .buffer_unordered(4);
        while let Some(result) = download_stream.next().await {
            result?;
        }

        Ok(())
    }

    async fn upload_assets(&self, client: &Client, assets: &[Asset]) -> Result<()> {
        let upload_stream = stream::iter(assets.iter().map(|original_asset| {
            let url = format!("{}/api/assets?key={}", self.base_url, self.key);
            async move {
                let form = reqwest::multipart::Form::new()
                    .text("deviceId", original_asset.device_id.clone())
                    .text("deviceAssetId", original_asset.device_asset_id.clone())
                    .text("fileCreatedAt", original_asset.file_created_at.clone())
                    .text("fileModifiedAt", original_asset.file_modified_at.clone())
                    .file(
                        "assetData",
                        original_asset
                            .path
                            .clone()
                            .context("Asset not downloaded")?,
                    )
                    .await?;

                let res = client.post(url).multipart(form).send().await?;
                if !res.status().is_success() {
                    bail!(
                        "Upload failed with status {}: {}",
                        res.status(),
                        res.text().await?
                    );
                }
                let response = res.json::<UploadResponse>().await?;

                Ok(response)
            }
        }))
        .buffer_unordered(4);

        let ids: Vec<String> = upload_stream
            .map(|response| response.map(|r| r.id))
            .try_collect()
            .await?;

        let url = format!(
            "{}/api/albums/{}/assets?key={}",
            self.base_url, self.album.id, self.key
        );
        let mut map = HashMap::new();
        map.insert("ids", ids);
        let res = client.put(url).json(&map).send().await?;
        if !res.status().is_success() {
            bail!(
                "Adding to album {} failed: {}",
                self.album.name,
                res.text().await?
            );
        }

        Ok(())
    }

    async fn upload_missing(
        &mut self,
        other: &Self,
        dry_run: bool,
        client: &Client,
        dir: &Path,
    ) -> Result<()> {
        self.get_assets(client).await?;
        let mut missing = other.album.missing_from_other(&self.album);
        println!("Uploading {} missing assets", missing.len());
        if dry_run {
            for asset in &missing {
                println!("Uploading asset {}", asset.file_name);
            }
        }
        other.download_assets(&mut missing, client, dir).await?;
        self.upload_assets(client, &missing).await?;
        Ok(())
    }
}

impl Album {
    fn missing_from_other(&self, other: &Self) -> Vec<Asset> {
        let other_checksums: HashSet<_> = other.assets.iter().map(|a| &a.checksum).collect();
        let missing_ids: Vec<Asset> = self
            .assets
            .iter()
            .filter(|asset| !other_checksums.contains(&asset.checksum))
            .cloned()
            .collect();
        missing_ids
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let raw_config = fs::read_to_string(args.config)?;
    let config: Config = toml::from_str(&raw_config)?;

    let client = reqwest::Client::new();

    for (name, peer) in &config {
        let mut this = SharedLink::new(&peer.shared_link, &client).await?;
        this.get_assets(&client).await?;

        for other_name in &peer.sync_with {
            let other = &config[other_name];
            let mut other = SharedLink::new(&other.shared_link, &client).await?;
            other.get_assets(&client).await?;

            println!(
                "Adding assets from {} ({}) to {} ({}) ...",
                other_name, other.album.name, name, this.album.name,
            );

            let tmp_dir = tempfile::Builder::new().prefix("iss").tempdir()?;
            let path = tmp_dir.path();

            this.upload_missing(&other, args.dry_run, &client, path)
                .await?;
        }
    }

    Ok(())
}
