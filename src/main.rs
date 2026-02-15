use anyhow::{Result, bail};
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
    url: String,
    key: String,
    sync_with: Vec<String>,
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
}

impl SharedLink {
    async fn new(base_url: &str, key: &str, client: &Client) -> Result<Self> {
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

    async fn download_asset(&self, id: &str, client: &Client, dir: &Path) -> Result<PathBuf> {
        let url = format!(
            "{}/api/assets/{id}/original?key={}&edited=true",
            self.base_url, self.key
        );
        let res = client.get(url).send().await?;

        let reg = regex::Regex::new(r#"filename\*\s*=\s*[^']*''([^;]+)"#)?;

        let filename = if let Some(caps) =
            reg.captures(res.headers()[reqwest::header::CONTENT_DISPOSITION].to_str()?)
        {
            caps.get(1).unwrap().as_str()
        } else {
            id
        };

        let dest_path = dir.join(filename);

        let mut dest_file = File::create(&dest_path)?;

        let content = res.bytes().await?;
        dest_file.write_all(&content)?;

        Ok(dest_path.to_owned())
    }

    async fn upload_asset(&self, client: &Client, asset_path: &Path) -> Result<()> {
        // TODO: fill fields
        let form = reqwest::multipart::Form::new()
            .text("deviceId", "TODO")
            .text("deviceAssetId", "TODO")
            .text("fileCreatedAt", "2025-12-04T18:49:20.532Z")
            .text("fileModifiedAt", "2025-12-04T18:49:20.532Z")
            .file("assetData", asset_path)
            .await?;

        let url = format!("{}/api/assets?key={}", self.base_url, self.key);
        let res = client.post(url).multipart(form).send().await?;

        if res.status() != reqwest::StatusCode::OK {
            bail!("Upload failed: {}", res.text().await?);
        }

        let res = res.json::<UploadResponse>().await?;

        let url = format!(
            "{}/api/albums/{}/assets?key={}",
            self.base_url, self.album.id, self.key
        );
        let mut map = HashMap::new();
        map.insert("ids", vec![res.id]);
        let res = client.put(url).json(&map).send().await?;
        if res.status() != reqwest::StatusCode::OK {
            bail!(
                "Adding to album {} failed: {}",
                self.album.name,
                res.text().await?
            );
        }

        Ok(())
    }

    async fn upload_missing(&mut self, other: &Self, client: &Client, dir: &Path) -> Result<()> {
        self.get_assets(client).await?;
        let missing = other.album.missing_from_other(&self.album);
        for asset in missing {
            let asset_path = other.download_asset(&asset.id, client, dir).await?;
            self.upload_asset(client, &asset_path).await?;
        }
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
    let raw_config = fs::read_to_string("./config.toml")?;
    let config: Config = toml::from_str(&raw_config)?;

    let client = reqwest::Client::new();

    for (name, peer) in &config {
        let mut this = SharedLink::new(&peer.url, &peer.key, &client).await?;
        this.get_assets(&client).await?;

        for other_name in &peer.sync_with {
            println!("Syncing {name} with {other_name} ...");
            let other = &config[other_name];
            let mut other = SharedLink::new(&other.url, &other.key, &client).await?;
            other.get_assets(&client).await?;

            let tmp_dir = tempfile::Builder::new().prefix("iss").tempdir()?;
            let path = tmp_dir.path();

            this.upload_missing(&other, &client, path).await?;
        }
    }

    Ok(())
}
