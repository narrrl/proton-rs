//! Walking and watching Proton Drive folder trees.
//!
//! Every Drive app ends up listing a whole folder tree: Proton Stream turns a
//! share into a video library, Proton Gallery turns a folder into albums. This
//! crate is the part of that which does not care what the files are:
//!
//! - [`DriveTree`]: one folder tree behind one surface, whether it is a public
//!   link opened as a visitor or a folder of a signed-in account.
//! - [`walk`]: list a whole tree, breadth first and a level at a time, with
//!   each listing retried while the failure is one another try could fix. A
//!   folder reached twice is listed once, and a folder cap stops a runaway
//!   walk.
//! - [`changes`]: Drive's event feed, read as "which of these crawled trees
//!   changed", so a tree is crawled again when it changed rather than on a
//!   timer.
//!
//! ```no_run
//! # async fn demo(tree: proton_drive_tree_rs::DriveTree) -> Result<(), Box<dyn std::error::Error>> {
//! use proton_drive_tree_rs::{WalkOptions, changes, walk};
//!
//! let root = tree.root_node().await?;
//! let nodes = walk(&tree, root, &WalkOptions::default(), |found| {
//!     println!("{found} nodes so far");
//! })
//! .await?;
//! let index = changes::TreeIndex::from_nodes(&nodes);
//!
//! if let Some((client, scope)) = tree.event_scope() {
//!     let seeded = changes::poll(client, &scope, None).await?;
//!     // Later, from the stored cursor:
//!     let poll = changes::poll(client, &scope, Some(&seeded.cursor)).await?;
//!     if index.is_affected_by(&poll) {
//!         // crawl again
//!     }
//! }
//! # Ok(()) }
//! ```

pub mod changes;
mod tree;
mod walk;

pub use tree::DriveTree;
pub use walk::{
    DEFAULT_CONCURRENCY, DEFAULT_RETRY_DELAYS, TreeNode, TreeSource, WalkError, WalkOptions, retry,
    walk,
};
