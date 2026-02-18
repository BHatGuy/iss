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

/// Type alias to describe the config file
type Config = HashMap<String, Peer>;

/// Peer entry in the config file
#[derive(Deserialize, Debug)]
struct Peer {
    /// Link to the shared album
    shared_link: String,

    /// List of names of peers that this peer should download its assets from
    sync_with: Vec<String>,
}

/// Command line arguments to be parsed by clap
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the config file
    #[arg(short, long)]
    config: String,

    /// Only print missing assets
    #[arg(short, long, default_value_t = false)]
    dry_run: bool,
}

/// A shared link which can be used to download an upload assets
#[derive(Deserialize, Debug)]
struct SharedLink {
    album: Album,

    /// Access key, parsed from a share link
    key: String,

    /// Base url of an immich instance, parsed from a share link
    #[serde(skip)]
    base_url: String,
}

/// An shared album that holds a list of its assets
#[derive(Deserialize, Debug)]
struct Album {
    #[serde(alias = "albumName")]
    name: String,
    id: String,

    #[serde(skip)]
    assets: Vec<Asset>,
}

/// An asses (e.g. image or video)
#[derive(Deserialize, Debug, Eq, PartialEq, Hash, Clone)]
struct Asset {
    /// Will be parsed from a json response
    id: String,

    /// Will be parsed from a json response
    checksum: String,

    /// Will be parsed from a json response
    #[serde(alias = "originalFileName")]
    file_name: String,

    /// Will be parsed from a json response
    #[serde(alias = "deviceAssetId")]
    device_asset_id: String,

    /// Will be parsed from a json response
    #[serde(alias = "deviceId")]
    device_id: String,

    /// Will be parsed from a json response
    #[serde(alias = "fileCreatedAt")]
    file_created_at: String,

    /// Will be parsed from a json response
    #[serde(alias = "fileModifiedAt")]
    file_modified_at: String,

    /// The location of this asset after it has been downloaded
    path: Option<PathBuf>,
}

/// Struct to deserialize responses containing assets
#[derive(Deserialize, Debug)]
struct AssetResponse {
    assets: Vec<Asset>,
}

/// Struct to serialize responses from uploading assets
#[derive(Deserialize, Debug)]
struct UploadResponse {
    id: String,
}

impl SharedLink {
    /// Create a SharedLink by parsing the given link
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

    /// Fill the list of asset that are currently contained in the shared album
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

    /// Download the given list of assets. The dowload path will be stored in the assets.
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

    /// Upload the given list of assets. The assets will be added to the album afterwards
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

    /// Upload all assets that are contained in the other SharedLink to this SharedLink.
    async fn upload_missing(
        &mut self,
        other: &Self,
        dry_run: bool,
        client: &Client,
        dir: &Path,
    ) -> Result<()> {
        self.get_assets(client).await?;
        let mut missing = other.album.missing_from_other(&self.album);
        if missing.is_empty() {
            println!("No assets to synchronize");
        } else if dry_run {
            println!("Assets that would be synced:");
            for asset in &missing {
                println!("{}", asset.file_name);
            }
            return Ok(());
        } else {
            println!("Uploading {} missing assets", missing.len());
            other.download_assets(&mut missing, client, dir).await?;
            self.upload_assets(client, &missing).await?;
        }

        Ok(())
    }
}

impl Album {
    /// Get all assets that are in the other Album but not in this album
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
