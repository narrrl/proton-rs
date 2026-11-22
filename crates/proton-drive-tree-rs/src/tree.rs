//! One folder tree, whichever way it was opened.

use proton_drive_rs::{
    DriveEventScopeId, Node, ProtonDriveClient, ProtonDrivePublicLinkClient, RevisionReader,
    ThumbnailType,
};
use proton_sdk::error::{ProtonError, Result};
use proton_sdk::ids::NodeUid;

use crate::walk::{TreeNode, TreeSource};

/// A folder tree: a public link opened as a visitor, or a folder of a
/// signed-in account.
///
/// Cheap to clone. Both clients share their caches and sessions between
/// clones, so siblings of a [`walk`](crate::walk) level that need the same
/// ancestor key wait on one derivation.
#[derive(Clone)]
pub enum DriveTree {
    /// A public link. Boxed: the visitor client is several times the size of
    /// the account variant, and a map of trees is mostly links.
    PublicLink(Box<ProtonDrivePublicLinkClient>),
    /// A folder of the signed-in account, and the client of that account.
    Account {
        client: ProtonDriveClient,
        root: NodeUid,
    },
}

impl DriveTree {
    /// The tree's top folder.
    pub async fn root_node(&self) -> Result<Node> {
        match self {
            Self::PublicLink(client) => client.get_root_node().await,
            Self::Account { client, root } => client.get_node(root).await?.ok_or_else(|| {
                ProtonError::invalid_operation("the folder is no longer in your Drive")
            }),
        }
    }

    pub async fn enumerate_folder_children_node_uids(
        &self,
        folder: &NodeUid,
    ) -> Result<Vec<NodeUid>> {
        match self {
            Self::PublicLink(client) => client.enumerate_folder_children_node_uids(folder).await,
            Self::Account { client, .. } => {
                client.enumerate_folder_children_node_uids(folder).await
            }
        }
    }

    pub async fn enumerate_nodes(&self, uids: &[NodeUid]) -> Result<Vec<Node>> {
        match self {
            Self::PublicLink(client) => client.enumerate_nodes(uids).await,
            Self::Account { client, .. } => client.enumerate_nodes(uids).await,
        }
    }

    pub async fn open_revision(&self, uid: &NodeUid) -> Result<RevisionReader> {
        match self {
            Self::PublicLink(client) => client.open_revision(uid).await,
            Self::Account { client, .. } => client.open_revision(uid).await,
        }
    }

    pub async fn download_range(&self, uid: &NodeUid, offset: u64, length: u64) -> Result<Vec<u8>> {
        match self {
            Self::PublicLink(client) => client.download_range(uid, offset, length).await,
            Self::Account { client, .. } => client.download_range(uid, offset, length).await,
        }
    }

    pub async fn download_thumbnail(
        &self,
        uid: &NodeUid,
        kind: ThumbnailType,
    ) -> Result<Option<Vec<u8>>> {
        match self {
            Self::PublicLink(client) => client.download_thumbnail(uid, kind).await,
            Self::Account { client, .. } => client.download_thumbnail(uid, kind).await,
        }
    }

    /// Replay a visitor handshake. An account session refreshes its own
    /// tokens, so for a folder there is nothing to do.
    pub async fn refresh_session(&self) -> Result<()> {
        match self {
            Self::PublicLink(client) => client.refresh_session().await,
            Self::Account { .. } => Ok(()),
        }
    }

    /// The account client and event scope whose feed reports this tree's
    /// changes, for [`changes::poll`](crate::changes::poll). `None` for a
    /// public link: a visitor has no event feed.
    pub fn event_scope(&self) -> Option<(&ProtonDriveClient, DriveEventScopeId)> {
        match self {
            Self::PublicLink(_) => None,
            Self::Account { client, root } => {
                Some((client, DriveEventScopeId::new(root.volume_id.clone())))
            }
        }
    }
}

impl TreeNode for Node {
    type Id = NodeUid;

    fn id(&self) -> &NodeUid {
        &self.uid
    }

    fn is_folder(&self) -> bool {
        Node::is_folder(self)
    }
}

#[async_trait::async_trait]
impl TreeSource for DriveTree {
    type Node = Node;
    type Error = ProtonError;

    /// Two requests: the child ids, then their details. The SDK chunks and
    /// fans out the second, so a walk's concurrency multiplies that.
    async fn children(&self, folder: &NodeUid) -> Result<Vec<Node>> {
        let uids = self.enumerate_folder_children_node_uids(folder).await?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        self.enumerate_nodes(&uids).await
    }

    fn is_retriable(error: &ProtonError) -> bool {
        error.is_retriable()
    }
}
