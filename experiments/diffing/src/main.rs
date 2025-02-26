use std::time::Instant;

use anyhow::anyhow;
use diff::tree::visit::{Action, Change};
use git_repository::{
    diff,
    hash::{oid, ObjectId},
    objs::{bstr::BStr, TreeRefIter},
    odb,
    prelude::*,
    refs::{file::ReferenceExt, peel},
};
use rayon::prelude::*;

const GITOXIDE_CACHED_OBJECT_DATA_PER_THREAD_IN_BYTES: usize = 60_000_000;

fn cache_size() -> usize {
    std::env::var("GITOXIDE_OBJECT_CACHE_IN_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(GITOXIDE_CACHED_OBJECT_DATA_PER_THREAD_IN_BYTES)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args();
    let repo_git_dir = args
        .nth(1)
        .ok_or_else(|| anyhow!("First argument is the .git directory to work in"))?;
    let repo = git_repository::discover(repo_git_dir)?.into_sync();
    let name = args.next().unwrap_or_else(|| "HEAD".into());
    let commit_id = repo
        .refs
        .find(&name)?
        .peel_to_id_in_place(&repo.refs, peel::none)?
        .to_owned();
    let db = &repo.objects;

    let start = Instant::now();
    let all_commits = commit_id
        .ancestors({
            let db = db
                .to_cache()
                .with_pack_cache(|| Box::new(odb::pack::cache::lru::StaticLinkedList::<64>::default()));
            move |oid, buf| db.find_commit_iter(oid, buf)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let num_diffs = all_commits.len();
    let elapsed = start.elapsed();
    println!(
        "gitoxide (uncached): collect all {} commits in {:?} ({:0.0} commits/s)",
        all_commits.len(),
        elapsed,
        all_commits.len() as f32 / elapsed.as_secs_f32()
    );

    let start = Instant::now();
    let num_deltas = do_gitoxide_tree_diff(
        &all_commits,
        || {
            let handle = db
                .to_cache()
                .with_pack_cache(|| Box::new(odb::pack::cache::lru::MemoryCappedHashmap::new(cache_size())))
                .with_object_cache(|| Box::new(odb::pack::cache::object::MemoryCappedHashmap::new(cache_size())));
            move |oid, buf: &mut Vec<u8>| handle.find(oid, buf)
        },
        Computation::MultiThreaded,
    )?;
    let elapsed = start.elapsed();
    println!(
        "gitoxide-deltas PARALLEL (cache = memory-LRU -> {:.0}MB | pack -> {:.0}MB): collect {} tree deltas of {} trees-diffs in {:?} ({:0.0} deltas/s, {:0.0} tree-diffs/s)",
        cache_size() as f32 / (1024 * 1024) as f32,
        cache_size() as f32 / (1024 * 1024) as f32,
        num_deltas,
        num_diffs,
        elapsed,
        num_deltas as f32 / elapsed.as_secs_f32(),
        num_diffs as f32 / elapsed.as_secs_f32()
    );

    let start = Instant::now();
    let num_deltas = do_gitoxide_tree_diff(
        &all_commits,
        || {
            let handle = db
                .to_cache()
                .with_object_cache(|| Box::new(odb::pack::cache::object::MemoryCappedHashmap::new(cache_size())));
            move |oid, buf: &mut Vec<u8>| handle.find(oid, buf)
        },
        Computation::MultiThreaded,
    )?;
    let elapsed = start.elapsed();
    println!(
        "gitoxide-deltas PARALLEL (cache = memory-LRU -> {:.0}MB ): collect {} tree deltas of {} trees-diffs in {:?} ({:0.0} deltas/s, {:0.0} tree-diffs/s)",
        cache_size() as f32 / (1024 * 1024) as f32,
        num_deltas,
        num_diffs,
        elapsed,
        num_deltas as f32 / elapsed.as_secs_f32(),
        num_diffs as f32 / elapsed.as_secs_f32()
    );

    let start = Instant::now();
    let num_deltas = do_gitoxide_tree_diff(
        &all_commits,
        || {
            let handle = db
                .to_cache()
                .with_object_cache(|| Box::new(odb::pack::cache::object::MemoryCappedHashmap::new(cache_size())));
            move |oid, buf: &mut Vec<u8>| handle.find(oid, buf)
        },
        Computation::SingleThreaded,
    )?;
    let elapsed = start.elapsed();
    println!(
        "gitoxide-deltas (cache = memory-LRU -> {:.0}MB): collect {} tree deltas of {} trees-diffs in {:?} ({:0.0} deltas/s, {:0.0} tree-diffs/s)",
        cache_size() as f32 / (1024 * 1024) as f32,
        num_deltas,
        num_diffs,
        elapsed,
        num_deltas as f32 / elapsed.as_secs_f32(),
        num_diffs as f32 / elapsed.as_secs_f32()
    );

    let start = Instant::now();
    let num_deltas = do_libgit2_treediff(&all_commits, repo.git_dir(), Computation::MultiThreaded)?;
    let elapsed = start.elapsed();
    println!(
        "libgit2 PARALLEL: collect {} tree deltas of {} trees-diffs in {:?} ({:0.0} deltas/s, {:0.0} tree-diffs/s)",
        num_deltas,
        num_diffs,
        elapsed,
        num_deltas as f32 / elapsed.as_secs_f32(),
        num_diffs as f32 / elapsed.as_secs_f32()
    );

    let start = Instant::now();
    let num_deltas = do_libgit2_treediff(&all_commits, repo.git_dir(), Computation::SingleThreaded)?;
    let elapsed = start.elapsed();
    println!(
        "libgit2: collect {} tree deltas of {} trees-diffs in {:?} ({:0.0} deltas/s, {:0.0} tree-diffs/s)",
        num_deltas,
        num_diffs,
        elapsed,
        num_deltas as f32 / elapsed.as_secs_f32(),
        num_diffs as f32 / elapsed.as_secs_f32()
    );

    Ok(())
}

enum Computation {
    SingleThreaded,
    MultiThreaded,
}

fn do_libgit2_treediff(commits: &[ObjectId], repo_dir: &std::path::Path, mode: Computation) -> anyhow::Result<usize> {
    Ok(match mode {
        Computation::SingleThreaded => {
            let db = git2::Repository::open(repo_dir)?;
            let mut changes = 0;
            for pair in commits.windows(2) {
                let (ca, cb) = (pair[0], pair[1]);
                let ta = db.find_commit(git2::Oid::from_bytes(ca.as_bytes())?)?.tree()?;
                let tb = db.find_commit(git2::Oid::from_bytes(cb.as_bytes())?)?.tree()?;
                let diff = db.diff_tree_to_tree(Some(&ta), Some(&tb), None)?;
                changes += diff.deltas().count();
            }
            changes
        }
        Computation::MultiThreaded => {
            let changes = std::sync::atomic::AtomicUsize::new(0);
            commits.par_windows(2).try_for_each_init::<_, _, _, anyhow::Result<_>>(
                || git2::Repository::open(repo_dir).expect("git directory is valid"),
                |db, pair| {
                    let (ca, cb) = (pair[0], pair[1]);
                    let ta = db.find_commit(git2::Oid::from_bytes(ca.as_bytes())?)?.tree()?;
                    let tb = db.find_commit(git2::Oid::from_bytes(cb.as_bytes())?)?.tree()?;
                    let diff = db.diff_tree_to_tree(Some(&ta), Some(&tb), None)?;
                    changes.fetch_add(diff.deltas().count(), std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                },
            )?;
            changes.load(std::sync::atomic::Ordering::Acquire)
        }
    })
}

fn do_gitoxide_tree_diff<C, L, E>(commits: &[ObjectId], make_find: C, mode: Computation) -> anyhow::Result<usize>
where
    C: Fn() -> L + Sync,
    L: for<'b> FnMut(&oid, &'b mut Vec<u8>) -> Result<git_repository::objs::Data<'b>, E>,
    E: std::error::Error + Send + Sync + 'static,
{
    let changes: usize = match mode {
        Computation::MultiThreaded => {
            let changes = std::sync::atomic::AtomicUsize::new(0);
            commits.par_windows(2).try_for_each_init::<_, _, _, anyhow::Result<_>>(
                || {
                    (
                        diff::tree::State::default(),
                        Vec::<u8>::new(),
                        Vec::<u8>::new(),
                        make_find(),
                    )
                },
                |(state, buf1, buf2, find), pair| {
                    let (ca, cb) = (pair[0], pair[1]);
                    let (ta, tb) = (
                        tree_iter_by_commit_res(&ca, buf1, &mut *find),
                        tree_iter_by_commit_res(&cb, buf2, &mut *find),
                    );
                    let mut count = Count::default();
                    diff::tree::Changes::from(ta).needed_to_obtain(
                        tb,
                        state,
                        |id, buf| find_tree_iter_res(id, buf, &mut *find),
                        &mut count,
                    )?;
                    changes.fetch_add(count.0, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                },
            )?;
            changes.load(std::sync::atomic::Ordering::Acquire)
        }
        Computation::SingleThreaded => {
            let mut state = diff::tree::State::default();
            let mut find = make_find();
            let mut buf: Vec<u8> = Vec::new();
            let mut buf2: Vec<u8> = Vec::new();
            let mut changes = 0;

            for pair in commits.windows(2) {
                let (ca, cb) = (pair[0], pair[1]);
                let (ta, tb) = (
                    tree_iter_by_commit_res(&ca, &mut buf, &mut find),
                    tree_iter_by_commit_res(&cb, &mut buf2, &mut find),
                );
                let mut count = Count::default();
                diff::tree::Changes::from(ta).needed_to_obtain(
                    tb,
                    &mut state,
                    |id, buf| find_tree_iter_res(id, buf, &mut find),
                    &mut count,
                )?;
                changes += count.0;
            }
            changes
        }
    };

    fn find_tree_iter_res<'b, L, E>(id: &oid, buf: &'b mut Vec<u8>, mut find: L) -> Result<TreeRefIter<'b>, E>
    where
        L: for<'a> FnMut(&oid, &'a mut Vec<u8>) -> Result<git_repository::objs::Data<'a>, E>,
        E: std::error::Error + Send + Sync + 'static,
    {
        find(id, buf).map(|o| o.try_into_tree_iter().expect("trees only"))
    }

    fn tree_iter_by_commit_res<'b, L, E>(id: &oid, buf: &'b mut Vec<u8>, mut find: L) -> TreeRefIter<'b>
    where
        L: for<'a> FnMut(&oid, &'a mut Vec<u8>) -> Result<git_repository::objs::Data<'a>, E>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let tid = find(id, buf)
            .expect("commit present")
            .try_into_commit_iter()
            .expect("a commit")
            .tree_id()
            .expect("tree id present and decodable");
        find_tree_iter_res(&tid, buf, find).expect("tree available")
    }

    #[derive(Default)]
    struct Count(usize);

    impl diff::tree::Visit for Count {
        fn pop_front_tracked_path_and_set_current(&mut self) {}

        fn push_back_tracked_path_component(&mut self, _component: &BStr) {}

        fn push_path_component(&mut self, _component: &BStr) {}

        fn pop_path_component(&mut self) {}

        fn visit(&mut self, _change: Change) -> Action {
            self.0 += 1;
            Action::Continue
        }
    }
    Ok(changes)
}
