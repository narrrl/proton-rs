# proton-drive-tree-rs

Walking and watching Proton Drive folder trees, shared by the Proton Drive app
family ([proton-rs](https://github.com/narrrl/proton-rs)).

- **`DriveTree`**: one folder tree behind one surface, whether it is a public
  link opened as a visitor or a folder of a signed-in account. Listing, node
  details, revisions, ranged downloads and thumbnails.
- **`walk`**: list a whole tree breadth first, a level at a time with a bounded
  number of listings in flight. A listing that fails in a way another try could
  fix (a dropped connection) is retried after 1, 3 and 9 seconds. A folder
  reached twice is listed once, and an optional folder cap stops a runaway
  walk. The order is stable between two walks of an unchanged tree. Generic
  over `TreeSource`, so it can be tested without a network.
- **`changes`**: Drive's per-volume event feed read as "which crawled trees
  changed". `poll` returns the nodes touched since a cursor (and invalidates the
  SDK's caches for them); `TreeIndex` is a crawled tree's node ids and says
  whether a poll concerns it. A gap in the feed counts as a change to every
  tree. Only account trees have a feed; a public link has none.

```rust,no_run
# async fn demo(tree: proton_drive_tree_rs::DriveTree) -> Result<(), Box<dyn std::error::Error>> {
use proton_drive_tree_rs::{WalkOptions, changes, walk};

// Seed the cursor first, so a change made during the walk is not lost.
let seeded = match tree.event_scope() {
    Some((client, scope)) => Some(changes::poll(client, &scope, None).await?.cursor),
    None => None,
};
let root = tree.root_node().await?;
let nodes = walk(&tree, root, &WalkOptions::default(), |_| {}).await?;
let index = changes::TreeIndex::from_nodes(&nodes);

// Later:
if let (Some((client, scope)), Some(cursor)) = (tree.event_scope(), seeded) {
    let poll = changes::poll(client, &scope, Some(&cursor)).await?;
    if index.is_affected_by(&poll) {
        // walk again, then keep `poll.cursor`
    }
}
# Ok(()) }
```

License: MIT.
