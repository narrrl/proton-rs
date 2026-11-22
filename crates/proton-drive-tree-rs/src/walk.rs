//! Listing a whole folder tree.

use std::collections::HashSet;
use std::hash::Hash;
use std::time::Duration;

use futures::stream::{StreamExt as _, TryStreamExt as _};

/// A node a walk can descend into or record.
pub trait TreeNode {
    type Id: Clone + Eq + Hash;

    fn id(&self) -> &Self::Id;
    fn is_folder(&self) -> bool;
}

/// Something whose folders can be listed.
#[async_trait::async_trait]
pub trait TreeSource: Sync {
    type Node: TreeNode + Send;
    type Error: Send;

    /// One folder's children, with their details.
    async fn children(
        &self,
        folder: &<Self::Node as TreeNode>::Id,
    ) -> Result<Vec<Self::Node>, Self::Error>;

    /// Whether another try could fix `error`. Nothing is retried by default.
    fn is_retriable(_error: &Self::Error) -> bool {
        false
    }
}

/// How many folders of one level to list at once.
///
/// Each listing fans out again inside the SDK (up to four detail batches per
/// folder), so this is a multiplier on the requests in flight, not their
/// number. Six keeps the ceiling in the low tens: enough to hide the round trip
/// on a wide level, well short of a burst a share server would read as abuse.
pub const DEFAULT_CONCURRENCY: usize = 6;

/// How long a walk waits before listing a folder again: about a quarter of a
/// minute in all. Long enough for a network to come back, short enough that a
/// tree that is really gone still says so.
pub const DEFAULT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(9),
];

/// How a [`walk`] runs.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Folders of one level listed at once.
    pub concurrency: usize,
    /// The wait before each further try of a listing that failed retriably.
    pub retry_delays: Vec<Duration>,
    /// Fail with [`WalkError::TooManyFolders`] past this many folders. `None`
    /// walks any tree to the end.
    pub max_folders: Option<usize>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_CONCURRENCY,
            retry_delays: DEFAULT_RETRY_DELAYS.to_vec(),
            max_folders: None,
        }
    }
}

impl WalkOptions {
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    pub fn with_retry_delays(mut self, delays: impl Into<Vec<Duration>>) -> Self {
        self.retry_delays = delays.into();
        self
    }

    pub fn with_max_folders(mut self, max: usize) -> Self {
        self.max_folders = Some(max);
        self
    }
}

/// Why a walk stopped.
#[derive(Debug, thiserror::Error)]
pub enum WalkError<E> {
    /// A listing failed, after any retries.
    #[error(transparent)]
    Source(E),
    /// The tree has more folders than [`WalkOptions::max_folders`].
    #[error("the folder tree has more than {0} folders")]
    TooManyFolders(usize),
}

/// List the whole tree under `root`, `root` first.
///
/// **Breadth first and a level at a time, because media trees are wide.** A
/// library is typically a root holding a folder per series, each holding a
/// folder per season: hundreds of folders, almost all siblings, each a listing
/// request plus a detail fetch. One folder at a time spends the walk waiting on
/// round trips it could have overlapped. Taking a level at once collapses that.
///
/// Order is stable: `buffered`, not `buffer_unordered`, so two walks of an
/// unchanged tree return the same order and their results diff cleanly.
///
/// A folder reached a second time (a cycle, or a node listed under two
/// parents) is recorded once and listed once. `progress` gets the count of
/// nodes found so far after every listing.
pub async fn walk<S: TreeSource>(
    source: &S,
    root: S::Node,
    options: &WalkOptions,
    progress: impl Fn(usize),
) -> Result<Vec<S::Node>, WalkError<S::Error>> {
    let mut seen: HashSet<<S::Node as TreeNode>::Id> = HashSet::new();
    seen.insert(root.id().clone());
    let mut level = if root.is_folder() {
        vec![root.id().clone()]
    } else {
        Vec::new()
    };
    let mut folders = level.len();
    let mut found = vec![root];

    while !level.is_empty() {
        let listings = level.into_iter().map(|folder| async move {
            retry(&options.retry_delays, S::is_retriable, || {
                source.children(&folder)
            })
            .await
        });
        let mut listings = futures::stream::iter(listings).buffered(options.concurrency.max(1));

        let mut next = Vec::new();
        while let Some(children) = listings.try_next().await.map_err(WalkError::Source)? {
            for child in children {
                if !seen.insert(child.id().clone()) {
                    tracing::debug!("a node was listed twice; keeping the first");
                    continue;
                }
                if child.is_folder() {
                    folders += 1;
                    if let Some(max) = options.max_folders
                        && folders > max
                    {
                        return Err(WalkError::TooManyFolders(max));
                    }
                    next.push(child.id().clone());
                }
                found.push(child);
            }
            progress(found.len());
        }
        level = next;
    }

    Ok(found)
}

/// Run `operation`, and again after each delay while it fails in a way
/// `retriable` says another try could fix.
///
/// A walk is hundreds of listings, and one failure ends it. The SDK replays a
/// timeout or a refused connection itself, but not a connection dropped
/// mid-request, which is what a phone sees on a hand-over from Wi-Fi to mobile
/// data or a pooled connection the OS tore down in the background. For a
/// listing, replaying that is harmless.
pub async fn retry<T, E, O, F>(
    delays: &[Duration],
    retriable: impl Fn(&E) -> bool,
    operation: O,
) -> Result<T, E>
where
    O: Fn() -> F,
    F: Future<Output = Result<T, E>>,
{
    let mut delays = delays.iter();
    loop {
        match operation().await {
            Err(error) if retriable(&error) => match delays.next() {
                Some(delay) => tokio::time::sleep(*delay).await,
                None => return Err(error),
            },
            done => return done,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Entry {
        id: &'static str,
        folder: bool,
    }

    impl TreeNode for Entry {
        type Id = &'static str;
        fn id(&self) -> &&'static str {
            &self.id
        }
        fn is_folder(&self) -> bool {
            self.folder
        }
    }

    /// A tree in memory. A folder named in `flaky` fails that many listings
    /// first, retriably.
    #[derive(Default)]
    struct Fake {
        children: HashMap<&'static str, Vec<Entry>>,
        flaky: Mutex<HashMap<&'static str, usize>>,
        fatal: Option<&'static str>,
        listings: AtomicUsize,
    }

    #[derive(Debug, PartialEq)]
    enum Failure {
        Dropped,
        Gone,
    }

    impl Fake {
        fn folder(mut self, id: &'static str, children: &[(&'static str, bool)]) -> Self {
            self.children.insert(
                id,
                children
                    .iter()
                    .map(|&(id, folder)| Entry { id, folder })
                    .collect(),
            );
            self
        }
    }

    #[async_trait::async_trait]
    impl TreeSource for Fake {
        type Node = Entry;
        type Error = Failure;

        async fn children(&self, folder: &&'static str) -> Result<Vec<Entry>, Failure> {
            self.listings.fetch_add(1, Ordering::SeqCst);
            if self.fatal == Some(*folder) {
                return Err(Failure::Gone);
            }
            if let Some(left) = self.flaky.lock().unwrap().get_mut(folder)
                && *left > 0
            {
                *left -= 1;
                return Err(Failure::Dropped);
            }
            Ok(self.children.get(folder).cloned().unwrap_or_default())
        }

        fn is_retriable(error: &Failure) -> bool {
            *error == Failure::Dropped
        }
    }

    fn root() -> Entry {
        Entry {
            id: "root",
            folder: true,
        }
    }

    fn fast() -> WalkOptions {
        WalkOptions::default().with_retry_delays([Duration::ZERO; 3])
    }

    fn ids(nodes: &[Entry]) -> Vec<&'static str> {
        nodes.iter().map(|node| node.id).collect()
    }

    fn library() -> Fake {
        Fake::default()
            .folder(
                "root",
                &[("show-a", true), ("show-b", true), ("notes.txt", false)],
            )
            .folder("show-a", &[("a-s1", true)])
            .folder("show-b", &[("b-e1.mkv", false), ("b-e2.mkv", false)])
            .folder("a-s1", &[("a-e1.mkv", false)])
    }

    #[tokio::test]
    async fn a_walk_lists_the_whole_tree_level_by_level_root_first() {
        let tree = library();
        let nodes = walk(&tree, root(), &fast(), |_| {}).await.unwrap();
        assert_eq!(
            ids(&nodes),
            [
                "root",
                "show-a",
                "show-b",
                "notes.txt",
                "a-s1",
                "b-e1.mkv",
                "b-e2.mkv",
                "a-e1.mkv"
            ]
        );
        assert_eq!(tree.listings.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn progress_counts_the_nodes_found_after_every_listing() {
        let seen = Mutex::new(Vec::new());
        walk(&library(), root(), &fast(), |found| {
            seen.lock().unwrap().push(found)
        })
        .await
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), [4, 5, 7, 8]);
    }

    #[tokio::test]
    async fn a_folder_reached_twice_is_listed_once() {
        // `loop` lists its own ancestor: a cycle.
        let tree = Fake::default()
            .folder("root", &[("loop", true)])
            .folder("loop", &[("root", true), ("file", false)]);
        let nodes = walk(&tree, root(), &fast(), |_| {}).await.unwrap();
        assert_eq!(ids(&nodes), ["root", "loop", "file"]);
        assert_eq!(tree.listings.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_tree_past_the_folder_cap_stops_the_walk() {
        let error = walk(&library(), root(), &fast().with_max_folders(2), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(error, WalkError::TooManyFolders(2)), "{error:?}");
    }

    #[tokio::test]
    async fn a_listing_that_drops_is_tried_again_and_the_walk_completes() {
        let tree = library();
        tree.flaky.lock().unwrap().insert("show-b", 2);
        let nodes = walk(&tree, root(), &fast(), |_| {}).await.unwrap();
        assert_eq!(nodes.len(), 8);
        assert_eq!(tree.listings.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn a_listing_that_fails_for_good_ends_the_walk_with_its_error() {
        let tree = Fake {
            fatal: Some("show-a"),
            ..library()
        };
        let error = walk(&tree, root(), &fast(), |_| {}).await.unwrap_err();
        assert!(
            matches!(error, WalkError::Source(Failure::Gone)),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_file_as_root_is_the_whole_walk() {
        let file = Entry {
            id: "only.mkv",
            folder: false,
        };
        let tree = Fake::default();
        let nodes = walk(&tree, file, &fast(), |_| {}).await.unwrap();
        assert_eq!(ids(&nodes), ["only.mkv"]);
        assert_eq!(tree.listings.load(Ordering::SeqCst), 0);
    }

    fn flaky(
        failures: usize,
        calls: &AtomicUsize,
    ) -> impl Fn() -> std::future::Ready<Result<(), ()>> {
        move || {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(if call < failures { Err(()) } else { Ok(()) })
        }
    }

    #[tokio::test]
    async fn an_operation_that_keeps_failing_reports_its_failure_once_the_delays_run_out() {
        let calls = AtomicUsize::new(0);
        let result = retry(&[Duration::ZERO; 3], |_| true, flaky(9, &calls)).await;
        assert_eq!(result, Err(()));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn a_failure_another_try_cannot_fix_is_reported_at_once() {
        let calls = AtomicUsize::new(0);
        let result = retry(&[Duration::ZERO; 3], |_| false, flaky(1, &calls)).await;
        assert_eq!(result, Err(()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
