//! Proton SDK 0.12 adapter. Protocol and cryptography remain in the SDK.
use std::sync::Arc;

use async_trait::async_trait;
use proton_drive_rs::{Node, NodeKind, ProtonDriveClient, RevisionReader};
use proton_gallery_rs::bytes::RevisionDescriptor;
use proton_gallery_rs::model::*;
use proton_gallery_rs::source::{BlockSource, MediaSource, SourceCapabilities, SourcePage};
use proton_gallery_rs::{CancellationToken, Error, Result};
use proton_sdk::ids::NodeUid;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

pub mod auth;
pub mod galleries;
pub mod photos;

/// The app-version string Proton identifies this client by.
///
/// Format is `external-drive-<name>@<version>`. The API splits the part before
/// `@` on `-`, so `<name>` must not contain one: `external-drive-proton-gallery`
/// is rejected on every request with `2064 Invalid section name`.
pub const APP_VERSION: &str = concat!(
    "external-drive-protongallery@",
    env!("CARGO_PKG_VERSION"),
    "-alpha"
);

pub fn client_configuration() -> proton_sdk::config::ProtonClientConfiguration {
    proton_sdk::config::ProtonClientConfiguration::new(APP_VERSION)
        .with_user_agent(concat!("proton-gallery/", env!("CARGO_PKG_VERSION")))
}

#[cfg(test)]
mod app_version_tests {
    #[test]
    fn app_name_section_has_no_extra_hyphen() {
        let (name, version) = super::APP_VERSION.split_once('@').unwrap();
        assert_eq!(name.split('-').count(), 3, "{name}");
        assert!(name.starts_with("external-drive-"));
        assert!(version.starts_with(env!("CARGO_PKG_VERSION")));
    }
}

struct Listing {
    nodes: Vec<NodeUid>,
    fingerprint: String,
}

/// SDK 0.12 collects child IDs before returning them. This adapter bounds node
/// decryption per page, but deliberately does not claim incremental ID listing.
pub struct ProtonFolderSource {
    spec: SourceSpec,
    client: ProtonDriveClient,
    listing: OnceCell<Listing>,
}

impl ProtonFolderSource {
    async fn listing(&self) -> Result<&Listing> {
        self.listing
            .get_or_try_init(|| async {
                let folder = NodeUid::new(
                    self.spec.volume.as_str().into(),
                    self.spec.root.as_str().into(),
                );
                let nodes = self
                    .client
                    .enumerate_folder_children_node_uids(&folder)
                    .await
                    .map_err(|_| Error::Remote("could not list Proton folder children"))?;
                let mut hash = Sha256::new();
                for node in &nodes {
                    hash.update((node.link_id.as_str().len() as u64).to_le_bytes());
                    hash.update(node.link_id.as_str().as_bytes());
                }
                Ok(Listing {
                    nodes,
                    fingerprint: hex::encode(hash.finalize()),
                })
            })
            .await
    }

    pub async fn subfolders(&self, cancel: &CancellationToken) -> Result<Vec<Node>> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = async {
                let mut folders=Vec::new();
                for batch in self.listing().await?.nodes.chunks(PageSize::MAX) {
                    let nodes=self.client.enumerate_nodes(batch).await.map_err(|_|Error::Remote("could not decrypt child albums"))?;
                    folders.extend(nodes.into_iter().filter(|n| n.is_folder() && !n.trashed));
                }
                Ok(folders)
            } => result,
        }
    }
    pub fn new(client: ProtonDriveClient, account: AccountId, folder: &Node) -> Result<Self> {
        if !folder.is_folder() || folder.trashed {
            return Err(Error::Invalid(
                "source must be an active Drive folder".into(),
            ));
        }
        Ok(Self {
            spec: SourceSpec {
                id: folder_source_id(&account, folder)?,
                account,
                volume: VolumeId::new(folder.uid.volume_id.as_str())?,
                root: NodeId::new(folder.uid.link_id.as_str())?,
                label: folder.name.clone(),
            },
            client,
            listing: OnceCell::new(),
        })
    }

    pub async fn open_revision(
        &self,
        asset: &AssetKey,
        revision: &RevisionId,
    ) -> Result<Arc<dyn BlockSource>> {
        if asset.account != self.spec.account || asset.volume != self.spec.volume {
            return Err(Error::Invalid(
                "asset is outside this account and volume".into(),
            ));
        }
        let reader = self
            .client
            .open_revision_at(&node_uid(asset), revision.as_str())
            .await
            .map_err(|_| Error::Remote("could not open the requested Proton revision"))?;
        let descriptor = RevisionDescriptor::new(
            asset.clone(),
            RevisionId::new(reader.revision_id())?,
            reader.block_sizes().to_vec(),
        )?;
        if descriptor.revision() != revision {
            return Err(Error::RevisionMismatch);
        }
        Ok(Arc::new(ProtonBlocks { reader, descriptor }))
    }
}

#[async_trait]
impl MediaSource for ProtonFolderSource {
    fn spec(&self) -> &SourceSpec {
        &self.spec
    }
    fn kind(&self) -> proton_gallery_rs::library::SourceKind {
        proton_gallery_rs::library::SourceKind::Drive
    }

    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            incremental_listing: false,
            revision_reads: true,
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
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = async {
                let listing = self.listing().await?;
                let offset = match cursor {
                    None => 0,
                    Some(cursor) => {
                        let (fingerprint, offset) = cursor.split_once(':').ok_or_else(|| Error::Invalid("invalid Proton scan cursor".into()))?;
                        if fingerprint != listing.fingerprint {
                            return Err(Error::Invalid("folder changed since the scan checkpoint; refresh with --restart".into()));
                        }
                        offset.parse::<usize>().map_err(|_| Error::Invalid("invalid Proton scan offset".into()))?
                    }
                };
                if offset > listing.nodes.len() { return Err(Error::Invalid("Proton cursor exceeds folder listing".into())); }
                let end = offset.saturating_add(limit.get()).min(listing.nodes.len());
                let nodes = self.client.enumerate_nodes(&listing.nodes[offset..end]).await
                    .map_err(|_| Error::Remote("could not decrypt a Proton metadata page"))?;
                let assets = nodes.into_iter().filter_map(|node| match to_asset(&self.spec.account, node) {
                    Ok(Some(asset)) => Some(Ok(asset)), Ok(None) => None, Err(e) => Some(Err(e)),
                }).collect::<Result<Vec<_>>>()?;
                Ok(SourcePage { assets, next: (end < listing.nodes.len()).then(|| format!("{}:{end}", listing.fingerprint)) })
            } => result,
        }
    }
}

/// The catalog id of a Drive folder, without needing a client.
pub fn folder_source_id(account: &AccountId, folder: &Node) -> Result<SourceId> {
    let identity = serde_json::to_vec(&(account, &folder.uid.volume_id, &folder.uid.link_id))?;
    SourceId::new(format!("drive-{}", hex::encode(Sha256::digest(identity))))
}

pub fn node_uid(asset: &AssetKey) -> NodeUid {
    NodeUid::new(asset.volume.as_str().into(), asset.node.as_str().into())
}

pub(crate) fn to_asset(account: &AccountId, node: Node) -> Result<Option<Asset>> {
    if node.trashed {
        return Ok(None);
    }
    let favorite = node
        .photo
        .as_ref()
        .is_some_and(|photo| photo.tags.contains(&proton_drive_rs::PhotoTag::Favorite));
    let NodeKind::File {
        media_type,
        claimed_size,
        active_revision_id,
        ..
    } = node.kind
    else {
        return Ok(None);
    };
    let Some(kind) = MediaKind::classify(&media_type, &node.name) else {
        return Ok(None);
    };
    Ok(Some(Asset {
        key: AssetKey {
            account: account.clone(),
            volume: VolumeId::new(node.uid.volume_id.as_str())?,
            node: NodeId::new(node.uid.link_id.as_str())?,
        },
        name: node.name,
        kind,
        mime: media_type,
        revision: active_revision_id.map(RevisionId::new).transpose()?,
        size_bytes: claimed_size.and_then(|size| u64::try_from(size).ok()),
        sort_time: node.modification_time,
        favorite,
    }))
}

struct ProtonBlocks {
    reader: RevisionReader,
    descriptor: RevisionDescriptor,
}

#[async_trait]
impl BlockSource for ProtonBlocks {
    fn descriptor(&self) -> &RevisionDescriptor {
        &self.descriptor
    }

    async fn read_block(&self, index: usize, cancel: &CancellationToken) -> Result<Vec<u8>> {
        let offset = self
            .descriptor
            .offset_of(index)
            .ok_or_else(|| Error::Invalid("block index out of bounds".into()))?;
        let length = self.descriptor.block_sizes()[index];
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            bytes = self.reader.read_at(offset, length) => {
                let bytes = bytes.map_err(|_| Error::Remote("Proton range read failed"))?;
                if bytes.len() as u64 != length { return Err(Error::RevisionMismatch); }
                Ok(bytes)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn file(mime: &str, name: &str, size: Option<i64>) -> Node {
        serde_json::from_value(serde_json::json!({
            "uid": { "volume_id": "volume", "link_id": "node" },
            "parent_uid": null, "name": name, "creation_time": 123,
            "modification_time": 456, "trashed": false, "signature_email": null,
            "kind": { "File": {
                "media_type": mime, "total_size_on_storage": 9999,
                "active_revision_id": "revision", "claimed_size": size,
                "claimed_modification_time": null, "content_sha1": null
            } }
        }))
        .unwrap()
    }

    #[test]
    fn plaintext_size_and_revision_identity_are_preserved_without_using_encrypted_size() {
        let account = AccountId::new("account").unwrap();
        let asset = to_asset(&account, file("image/jpeg", "photo.jpg", Some(123)))
            .unwrap()
            .unwrap();
        assert_eq!(asset.key.account, account);
        assert_eq!(asset.size_bytes, Some(123));
        assert_eq!(asset.revision.unwrap().as_str(), "revision");
        assert_eq!(asset.sort_time, 456);
        let unknown = to_asset(&account, file("video/mp4", "clip.mp4", None))
            .unwrap()
            .unwrap();
        assert_eq!(unknown.size_bytes, None);
    }

    #[test]
    fn trashed_and_non_media_nodes_are_not_added_to_the_gallery() {
        let account = AccountId::new("account").unwrap();
        assert!(
            to_asset(&account, file("application/pdf", "document.jpg", Some(123)))
                .unwrap()
                .is_none()
        );
        let mut trashed = file("image/jpeg", "photo.jpg", Some(123));
        trashed.trashed = true;
        assert!(to_asset(&account, trashed).unwrap().is_none());
    }
}
