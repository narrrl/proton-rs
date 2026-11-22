//! Which crawled trees changed, from Drive's event feed.
//!
//! A crawl is the only way to know what a tree holds, and a full one is
//! hundreds of requests. Crawling on a timer either wastes them on a tree that
//! did not change or leaves a new file out until the timer fires. The event
//! feed of the tree's volume says what changed since a cursor, so an app polls
//! it cheaply and crawls only the trees an event touched.
//!
//! The feed is per volume and reports every change there, most of them outside
//! any one tree (another folder, or an app's own settings file). [`TreeIndex`]
//! is the crawled tree's node ids, which is enough to tell the two apart: an
//! event concerns the tree when it names a node in it or a folder in it as the
//! parent (a new or moved-in child).
//!
//! Only account trees have a feed. A public link has none and stays on
//! whatever schedule the app crawls it on.

use std::collections::HashSet;

use proton_drive_rs::{DriveEvent, DriveEventScopeId, Node, ProtonDriveClient};
use proton_sdk::error::Result;
use proton_sdk::ids::{DriveEventId, NodeUid};

/// One node an event named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    pub node: NodeUid,
    /// The node's parent, when the event reports it. For a move, the new one.
    pub parent: Option<NodeUid>,
    /// Deleted, or moved to the trash.
    pub removed: bool,
}

/// What one [`poll`] found.
#[derive(Debug, Clone)]
pub struct Poll {
    /// Where the next poll starts. Store it only once the changes are acted on.
    pub cursor: DriveEventId,
    pub touched: Vec<Touch>,
    /// Proton could not say what changed since the cursor: treat every tree on
    /// the volume as changed.
    pub gap: bool,
}

impl Poll {
    /// Nothing happened since the cursor.
    pub fn is_quiet(&self) -> bool {
        self.touched.is_empty() && !self.gap
    }
}

/// Read the feed of `scope` from `cursor`.
///
/// Without a cursor this seeds one: no changes, and a cursor at the newest
/// event. Seed *before* the crawl the cursor is meant to follow, so a change
/// made while the crawl runs is reported on the next poll rather than lost.
///
/// Every event is also handed to [`ProtonDriveClient::invalidate_caches_for_event`]:
/// the client caches node details and folder keys without expiry, and the
/// crawl that follows must not be served the node as it was before the change.
pub async fn poll(
    client: &ProtonDriveClient,
    scope: &DriveEventScopeId,
    cursor: Option<&DriveEventId>,
) -> Result<Poll> {
    let events = client.enumerate_events(scope, cursor).await?;
    let mut touched = Vec::new();
    let mut gap = false;
    for event in &events {
        client.invalidate_caches_for_event(event).await?;
        match event {
            DriveEvent::NodeUpdated {
                node_uid,
                parent_node_uid,
                is_trashed,
                ..
            } => touched.push(Touch {
                node: node_uid.clone(),
                parent: parent_node_uid.clone(),
                removed: *is_trashed,
            }),
            DriveEvent::NodeDeleted {
                node_uid,
                parent_node_uid,
                ..
            } => touched.push(Touch {
                node: node_uid.clone(),
                parent: parent_node_uid.clone(),
                removed: true,
            }),
            DriveEvent::ContinuityLost { .. } | DriveEvent::ScopeAccessLost { .. } => gap = true,
            _ => {}
        }
    }
    let cursor = events
        .last()
        .map(|event| event.id().clone())
        .or_else(|| cursor.cloned());
    let cursor = match cursor {
        Some(cursor) => cursor,
        // An empty answer to a seed request; ask for the newest event outright.
        None => client.latest_event_id(scope.volume_id()).await?,
    };
    Ok(Poll {
        cursor,
        touched,
        gap,
    })
}

/// The node ids of one crawled tree, for telling which events concern it.
#[derive(Debug, Clone, Default)]
pub struct TreeIndex {
    nodes: HashSet<NodeUid>,
    folders: HashSet<NodeUid>,
}

impl TreeIndex {
    /// The index of a crawl's result, root included.
    pub fn from_nodes<'a>(nodes: impl IntoIterator<Item = &'a Node>) -> Self {
        Self::new(nodes.into_iter().map(|node| (&node.uid, node.is_folder())))
    }

    /// The index of `(id, is_folder)` pairs.
    pub fn new<'a>(nodes: impl IntoIterator<Item = (&'a NodeUid, bool)>) -> Self {
        let mut index = Self::default();
        for (uid, folder) in nodes {
            if folder {
                index.folders.insert(uid.clone());
            }
            index.nodes.insert(uid.clone());
        }
        index
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The change concerns this tree: it names a node of it (changed, moved
    /// out, removed) or names a folder of it as the parent (added, moved in).
    pub fn is_touched_by(&self, touch: &Touch) -> bool {
        self.nodes.contains(&touch.node)
            || touch
                .parent
                .as_ref()
                .is_some_and(|parent| self.folders.contains(parent))
    }

    /// Anything in `poll` concerns this tree, or the feed lost track.
    pub fn is_affected_by(&self, poll: &Poll) -> bool {
        poll.gap || poll.touched.iter().any(|touch| self.is_touched_by(touch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new("volume".into(), link.into())
    }

    fn index() -> TreeIndex {
        let (root, season, episode) = (uid("root"), uid("season"), uid("episode"));
        TreeIndex::new([(&root, true), (&season, true), (&episode, false)])
    }

    fn touch(node: &str, parent: Option<&str>) -> Touch {
        Touch {
            node: uid(node),
            parent: parent.map(uid),
            removed: false,
        }
    }

    fn poll(touched: Vec<Touch>, gap: bool) -> Poll {
        Poll {
            cursor: DriveEventId::new("cursor"),
            touched,
            gap,
        }
    }

    #[test]
    fn a_file_added_to_a_folder_of_the_tree_touches_it() {
        assert!(index().is_touched_by(&touch("new-episode", Some("season"))));
    }

    #[test]
    fn a_change_to_a_node_of_the_tree_touches_it() {
        assert!(index().is_touched_by(&touch("episode", None)));
        assert!(index().is_touched_by(&Touch {
            removed: true,
            ..touch("season", Some("root"))
        }));
    }

    #[test]
    fn a_node_moved_out_of_the_tree_touches_it() {
        assert!(index().is_touched_by(&touch("episode", Some("elsewhere"))));
    }

    #[test]
    fn a_change_elsewhere_on_the_volume_does_not_touch_the_tree() {
        assert!(!index().is_touched_by(&touch("settings.json", Some("app-device"))));
        assert!(!index().is_touched_by(&touch("unrelated", None)));
        // A file is no folder: nothing is added "under" it.
        assert!(!index().is_touched_by(&touch("x", Some("episode"))));
    }

    #[test]
    fn a_gap_in_the_feed_affects_every_tree() {
        assert!(index().is_affected_by(&poll(Vec::new(), true)));
        assert!(TreeIndex::default().is_affected_by(&poll(Vec::new(), true)));
    }

    #[test]
    fn a_quiet_poll_affects_nothing() {
        let quiet = poll(Vec::new(), false);
        assert!(quiet.is_quiet());
        assert!(!index().is_affected_by(&quiet));
        let elsewhere = poll(vec![touch("unrelated", Some("other"))], false);
        assert!(!elsewhere.is_quiet());
        assert!(!index().is_affected_by(&elsewhere));
    }
}
