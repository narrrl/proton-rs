//! The normal Proton Photos timeline, with its own metadata/key routing and
//! server capture dates. Never route it through an ordinary My Files folder.
use async_trait::async_trait;
use proton_drive_rs::{Node, PhotosTimelineItem, ProtonPhotosClient, ThumbnailType};
use proton_gallery_rs::library::SourceKind;
use proton_gallery_rs::model::*;
use proton_gallery_rs::source::{MediaSource, SourceCapabilities, SourcePage};
use proton_gallery_rs::{CancellationToken, Error, Result};
use proton_sdk::ids::NodeUid;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

struct Listing {
    entries: Vec<PhotosTimelineItem>,
    fingerprint: String,
}
pub struct ProtonPhotosSource {
    spec: SourceSpec,
    client: ProtonPhotosClient,
    listing: OnceCell<Listing>,
}

impl ProtonPhotosSource {
    pub fn new(client: ProtonPhotosClient, account: AccountId, root: &Node) -> Result<Self> {
        if !root.is_folder() || root.trashed {
            return Err(Error::Invalid("invalid Photos root".into()));
        }
        let identity = serde_json::to_vec(&(&account, &root.uid.volume_id, &root.uid.link_id))?;
        Ok(Self {
            spec: SourceSpec {
                id: SourceId::new(format!("photos-{}", hex::encode(Sha256::digest(identity))))?,
                account,
                volume: VolumeId::new(root.uid.volume_id.as_str())?,
                root: NodeId::new(root.uid.link_id.as_str())?,
                label: "Proton Photos".into(),
            },
            client,
            listing: OnceCell::new(),
        })
    }

    /// Every node of the last complete listing on the Photos volume, or
    /// `None` before the timeline was listed.
    pub fn listed_nodes(&self) -> Option<Vec<NodeId>> {
        let listing = self.listing.get()?;
        listing
            .entries
            .iter()
            .filter(|e| e.uid.volume_id.as_str() == self.spec.volume.as_str())
            .map(|e| NodeId::new(e.uid.link_id.as_str()).ok())
            .collect()
    }

    pub async fn thumbnail(
        &self,
        asset: &AssetKey,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<u8>>> {
        if asset.account != self.spec.account || asset.volume != self.spec.volume {
            return Err(Error::Invalid("thumbnail outside Photos scope".into()));
        }
        let uid = super::node_uid(asset);
        tokio::select! {
            biased;
            _ = cancel.cancelled()=>Err(Error::Cancelled),
            result=self.client.download_thumbnail(&uid,ThumbnailType::Thumbnail)=> {
                let bytes=result.map_err(|_|Error::Remote("could not load Photos thumbnail"))?;
                if bytes.as_ref().is_some_and(|b|b.len()>4*1024*1024) { return Err(Error::Invalid("Photos thumbnail exceeds budget".into())); }
                Ok(bytes)
            }
        }
    }

    /// Which of `assets` have no thumbnail at all on their active revision,
    /// as Proton reports it. Reads link details in batches, and a revision's
    /// metadata only where the listing leaves its thumbnails out. Downloads
    /// no image.
    ///
    /// An asset not returned is not known to have one: a failed call, a
    /// node that went away or a folder is never reported. Only the returned
    /// keys are evidence of absence, and only for the revision active now.
    pub async fn missing_thumbnails(
        &self,
        assets: &[AssetKey],
        cancel: &CancellationToken,
    ) -> Result<Vec<AssetKey>> {
        if assets
            .iter()
            .any(|a| a.account != self.spec.account || a.volume != self.spec.volume)
        {
            return Err(Error::Invalid(
                "thumbnail check outside Photos scope".into(),
            ));
        }
        if assets.is_empty() {
            return Ok(Vec::new());
        }
        let uids: Vec<NodeUid> = assets.iter().map(super::node_uid).collect();
        let missing = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = self.client.find_missing_thumbnails(&uids) => result
                .map_err(|_| Error::Remote("could not check Photos thumbnails"))?,
        };
        Ok(matching(assets, &missing))
    }
}

/// The keys among `assets` whose node is in `missing`, in `assets`' order.
/// A uid Proton returned that was not asked about is dropped.
fn matching(assets: &[AssetKey], missing: &[NodeUid]) -> Vec<AssetKey> {
    assets
        .iter()
        .filter(|a| {
            missing.iter().any(|m| {
                m.volume_id.as_str() == a.volume.as_str() && m.link_id.as_str() == a.node.as_str()
            })
        })
        .cloned()
        .collect()
}

#[async_trait]
impl MediaSource for ProtonPhotosSource {
    fn spec(&self) -> &SourceSpec {
        &self.spec
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Photos
    }
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            incremental_listing: false,
            revision_reads: false,
        }
    }
    async fn list_page(
        &self,
        cursor: Option<&str>,
        limit: PageSize,
        cancel: &CancellationToken,
    ) -> Result<SourcePage> {
        tokio::select! {
            biased;
            _=cancel.cancelled()=>Err(Error::Cancelled),
            result=async {
                let listing=self.listing.get_or_try_init(||async {
                    let mut entries=self.client.enumerate_timeline().await.map_err(|_|Error::Remote("could not list Proton Photos"))?;
                    entries.sort_by(|a,b| b.capture_time.cmp(&a.capture_time).then_with(||b.uid.volume_id.as_str().cmp(a.uid.volume_id.as_str())).then_with(||b.uid.link_id.as_str().cmp(a.uid.link_id.as_str())));
                    let mut hash=Sha256::new();
                    for entry in &entries {
                        hash.update(entry.capture_time.to_le_bytes());
                        for part in [entry.uid.volume_id.as_str(),entry.uid.link_id.as_str()] {
                            hash.update((part.len() as u64).to_le_bytes());
                            hash.update(part.as_bytes());
                        }
                    }
                    let fingerprint=hex::encode(hash.finalize());
                    Ok::<_,Error>(Listing{entries,fingerprint})
                }).await?;
                let offset=match cursor {
                    None=>0,
                    Some(c)=> {
                        let (fingerprint,offset)=c.split_once(':').ok_or_else(||Error::Invalid("invalid Photos cursor".into()))?;
                        if fingerprint!=listing.fingerprint {return Err(Error::Invalid("Photos timeline changed; refresh with --restart".into()));}
                        offset.parse::<usize>().map_err(|_|Error::Invalid("invalid Photos offset".into()))?
                    }
                };
                if offset>listing.entries.len() { return Err(Error::Invalid("Photos cursor exceeds listing".into())); }
                let end=offset.saturating_add(limit.get()).min(listing.entries.len());
                let entries=&listing.entries[offset..end];
                let uids:Vec<NodeUid>=entries.iter().map(|e|e.uid.clone()).collect();
                let nodes=self.client.enumerate_nodes(&uids).await.map_err(|_|Error::Remote("could not decrypt Photos metadata"))?;
                let mut assets=Vec::new();
                for node in nodes {
                    let time=entries.iter().find(|e|e.uid==node.uid).ok_or(Error::Remote("Photos returned an unexpected node"))?.capture_time;
                    if let Some(asset)=project(&self.spec.account,node,time)? { assets.push(asset); }
                }
                Ok(SourcePage{assets,next:(end<listing.entries.len()).then(||format!("{}:{end}",listing.fingerprint))})
            }=>result,
        }
    }
}

/// The catalog record of a Photos node, sorted by its capture time.
pub fn photo_asset(account: &AccountId, node: Node, capture_time: i64) -> Result<Option<Asset>> {
    project(account, node, capture_time)
}

fn project(account: &AccountId, node: Node, capture_time: i64) -> Result<Option<Asset>> {
    Ok(super::to_asset(account, node)?.map(|mut asset| {
        asset.sort_time = capture_time;
        asset
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_asked_assets_proton_returned_count_as_missing() {
        let account = AccountId::new("account").unwrap();
        let key = |node: &str| AssetKey {
            account: account.clone(),
            volume: VolumeId::new("volume").unwrap(),
            node: NodeId::new(node).unwrap(),
        };
        let uid = |node: &str| NodeUid::new("volume".into(), node.into());
        let asked = [key("a"), key("b"), key("c")];
        let found = matching(&asked, &[uid("c"), uid("stranger"), uid("a")]);
        assert_eq!(found, [key("a"), key("c")]);
        assert!(matching(&asked, &[]).is_empty());
    }

    #[test]
    fn photos_use_capture_time_instead_of_drive_modification_time() {
        let node = super::super::tests::file("image/jpeg", "photo.jpg", Some(123));
        let asset = project(&AccountId::new("account").unwrap(), node, 777)
            .unwrap()
            .unwrap();
        assert_eq!(asset.sort_time, 777);
        assert_eq!(asset.revision.unwrap().as_str(), "revision");
    }
}
