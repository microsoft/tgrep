/// Index builder: walks a repo, extracts trigrams, writes the on-disk index.
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use crate::external::{self, ExternalSorter, TrigramPosting};
use crate::meta::{self, IndexMeta};
use crate::ondisk::{self, LookupEntry, PostingEntry};
use crate::reader::IndexReader;
use crate::trigram::{self, TrigramMasks};
use crate::walker;
use crate::{Error, Result};

const INDEX_DIR_NAME: &str = ".tgrep";
const INDEX_BUILD_BATCH_SIZE: usize = 1024;
const POSTING_WRITE_CHUNK_ENTRIES: usize = 8192;
const LOOKUP_WRITE_CHUNK_ENTRIES: usize = 4096;

/// Cumulative bytes allowed in one extraction batch.
///
/// Batching by file count alone bounds nothing in memory: every file in a batch
/// can be read concurrently, so a run of large files puts an unbounded number of
/// whole-file buffers in flight at once. Bounding the batch's total size bounds
/// that directly, because each buffer is freed as soon as its trigrams have been
/// extracted.
///
/// The budget also bounds parallelism — a batch holding three 20 MB headers can
/// only occupy three workers — so it was measured rather than guessed. On the
/// Linux kernel this value cut peak memory 32% for roughly 4% build time, and a
/// larger budget that scales with the thread pool was tried and rejected: it was
/// both slower *and* hungrier there, because the kernel's large files are a rare
/// minority and the extra headroom bought no throughput.
///
/// Files large enough to be mapped are charged [`MAPPED_BATCH_CHARGE`] instead
/// of their length, so a tree that is mostly oversized files still fills the
/// pool. Raising this budget would trade memory on *every* tree for that;
/// charging mapped bytes honestly costs nothing on trees like the kernel, where
/// only 110 of 94,747 files are mapped at all.
const INDEX_BUILD_BATCH_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OWNED_FILE_BYTES: u64 = INDEX_BUILD_BATCH_BYTES;

/// Default arena budget for [`IndexStrategy::External`] before spilling.
pub use crate::external::DEFAULT_BUFFER_BYTES as DEFAULT_INDEX_BUFFER_BYTES;

/// How the builder accumulates postings before writing them out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IndexStrategy {
    /// Hold every posting in heap, sort once, write once.
    ///
    /// Peak memory grows linearly with repository size and is unbounded. Kept
    /// as an escape hatch for environments where spilling to disk is
    /// undesirable or impossible (read-only or full index volume), and as the
    /// independent reference implementation that [`IndexStrategy::External`]
    /// is differentially tested against.
    InMemory,
    /// Bound peak memory with an external merge sort.
    ///
    /// Postings accumulate in a fixed-size arena that spills sorted, compact
    /// segments to disk when full; the segments are then k-way merged straight
    /// into the index. Peak heap is independent of repository size. If the
    /// arena never fills this is identical to [`IndexStrategy::InMemory`], so
    /// small repositories pay nothing for the default.
    #[default]
    External,
}

/// Tunables for [`build_index_with_options`].
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub include_hidden: bool,
    pub no_ignore: bool,
    /// `--no-require-git`: respect `.gitignore` outside a git repository.
    ///
    /// The `ignore` crate gates gitignore rules on finding a `.git` directory,
    /// matching ripgrep. Enlistments that are not git checkouts (Perforce and
    /// Source Depot trees, exported source drops) therefore have their
    /// `.gitignore` files silently ignored, and the index picks up build
    /// output. This opts out of that gate.
    pub no_require_git: bool,
    pub exclude_dirs: Vec<String>,
    /// Apply leading-dot ("hidden") filtering in the walk instead of the
    /// platform's native hidden check, and treat `.gitignore` files as
    /// hidden.
    ///
    /// The two differ on Windows, where the native check reads the
    /// `FILE_ATTRIBUTE_HIDDEN` bit that git does not set on dotfiles, so a
    /// default walk indexes `.gitignore`, `.mailmap`, and friends there but
    /// not on Unix. `tgrep serve` walks with leading-dot semantics on every
    /// platform, and its file watcher skips dot-prefixed paths outright, so a
    /// build destined for a server must opt in — otherwise the server would
    /// index dotfiles it can never afterwards update or remove.
    pub collect_gitignore_files: bool,
    pub strategy: IndexStrategy,
    /// Arena budget in bytes for [`IndexStrategy::External`]. Ignored by
    /// [`IndexStrategy::InMemory`].
    pub buffer_bytes: usize,
    /// Skip files larger than this. `None` indexes files of any size.
    pub max_file_size: Option<u64>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            no_ignore: false,
            no_require_git: false,
            exclude_dirs: Vec::new(),
            collect_gitignore_files: false,
            strategy: IndexStrategy::default(),
            buffer_bytes: external::DEFAULT_BUFFER_BYTES,
            max_file_size: walker::DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// Destination for postings as files are processed.
enum PostingSink {
    InMemory(Vec<TrigramPosting>),
    External(ExternalSorter),
}

impl PostingSink {
    fn push_file(
        &mut self,
        file_id: u32,
        per_trigram: impl IntoIterator<Item = (u32, TrigramMasks)>,
    ) -> Result<()> {
        match self {
            Self::InMemory(postings) => {
                postings.extend(
                    per_trigram
                        .into_iter()
                        .map(|(trigram, masks)| TrigramPosting {
                            trigram,
                            entry: PostingEntry {
                                file_id,
                                loc_mask: masks.loc_mask,
                                next_mask: masks.next_mask,
                            },
                        }),
                );
                Ok(())
            }
            Self::External(sorter) => sorter.push_file(file_id, per_trigram),
        }
    }
}

/// What a build produced, beyond the index files themselves.
#[derive(Debug, Default)]
pub struct BuildOutcome {
    /// Number of files written to the index.
    pub num_files: usize,
    /// Absolute `.gitignore` paths seen during the walk, when
    /// [`BuildOptions::collect_gitignore_files`] was set; empty otherwise.
    ///
    /// These are absolute because `build_index_with_options` canonicalizes
    /// `root` before walking. Consumers rely on that:
    /// [`walker::build_gitignore_matcher_from_files`] anchors each nested
    /// `.gitignore` by stripping `root` from its parent directory, so
    /// repo-relative paths would leave every nested rule out of the matcher.
    ///
    /// Handing these back lets a caller that needs an ignore matcher build one
    /// with [`walker::build_gitignore_matcher_from_files`] instead of
    /// [`crate::gitignore::build_matcher`], which would repeat the whole walk —
    /// 49 s on a 289k-file repository.
    pub gitignore_files: Vec<std::path::PathBuf>,
    /// Absolute `.ignore` paths seen during the same walk, kept separate from
    /// `gitignore_files` so a matcher can apply them last and give them
    /// precedence over `.gitignore`.
    pub ignore_files: Vec<std::path::PathBuf>,
}

/// Build a trigram index for all text files under `root`.
pub fn build_index(
    root: &Path,
    index_dir: Option<&Path>,
    include_hidden: bool,
    no_ignore: bool,
    exclude_dirs: &[String],
) -> Result<()> {
    build_index_with_options(
        root,
        index_dir,
        &BuildOptions {
            include_hidden,
            no_ignore,
            exclude_dirs: exclude_dirs.to_vec(),
            ..Default::default()
        },
    )
    .map(|_| ())
}

/// A warning to print when a `.gitignore` exists but the git gate silently
/// disables it.
///
/// The `ignore` crate only applies `.gitignore` inside a git repository, which
/// is exactly ripgrep's behaviour. On a non-git enlistment (Perforce, Source
/// Depot, or a plain directory) that makes a repo-root `.gitignore` inert, and
/// the only symptom is an index far larger than expected — with nothing in the
/// log to explain why. Returns `None` when the rules are genuinely in effect,
/// or when the user has already opted out of them.
fn gitignore_gate_hint(root: &Path, opts: &BuildOptions) -> Option<String> {
    if opts.no_ignore || opts.no_require_git {
        return None;
    }
    if crate::gitignore::in_git_repo(root) {
        return None;
    }
    if !root.join(".gitignore").is_file() {
        return None;
    }
    Some(format!(
        "warning: {} has a .gitignore but is not a git repository, so it is not \
         applied (this matches ripgrep). Pass --no-require-git to apply it.",
        root.display()
    ))
}

/// Nominal heap charge for a file large enough to be memory-mapped.
///
/// A mapped file's bytes never reach the heap, so charging it its own length
/// against the heap budget is charging for memory the build does not allocate.
/// What it does cost is its extracted trigram map, and that is bounded by the
/// number of *distinct* trigrams — at most 16.7M, and in practice saturating
/// long before file size does. Saturating the charge keeps that real cost
/// bounded while letting a batch hold enough large files to fill the pool.
const MAPPED_BATCH_CHARGE: u64 = 2 * 1024 * 1024;

/// Ceiling on mapped bytes one batch may put in flight.
///
/// Mapped pages are file-backed and reclaimable, so they are not the same
/// liability as heap, but they are still resident: a batch of large files maps
/// all of them at once, and the process working set follows. This is the second
/// half of the bound — the heap budget alone would let a batch of 32 MiB files
/// map 64 MiB *per worker*.
const MAPPED_BATCH_BYTES: u64 = 256 * 1024 * 1024;

/// What one file costs a batch, split by where the bytes live.
#[derive(Clone, Copy, Default)]
struct BatchCharge {
    /// Charge against the heap budget: the read buffer for a small file, or the
    /// saturating stand-in for a mapped file's extracted trigram map.
    heap: u64,
    /// Charge against the mapped budget; zero for a file that is read.
    mapped: u64,
}

fn batch_charge(size: u64) -> BatchCharge {
    if size >= MMAP_MIN_BYTES {
        BatchCharge {
            heap: size.min(MAPPED_BATCH_CHARGE),
            mapped: size,
        }
    } else {
        BatchCharge {
            heap: size,
            mapped: 0,
        }
    }
}

/// Charge for a file whose size could not be read.
///
/// An unknown size counts as a whole batch rather than as zero. A file that
/// failed to stat may still read, and calling it empty would both let it slip
/// into an already-full batch and route it to the heap instead of a map —
/// losing the bound precisely for the file whose size is unknown.
const UNKNOWN_SIZE_CHARGE: BatchCharge = BatchCharge {
    heap: INDEX_BUILD_BATCH_BYTES,
    mapped: MAPPED_BATCH_BYTES,
};

/// Split a file list into batches bounded by file count, heap bytes and mapped
/// bytes, given each file's charge in walk order.
///
/// A file whose charge alone exceeds a budget forms a batch of its own rather
/// than being split, so the bound is "one budget, plus at most one oversized
/// file".
fn batch_ranges(charges: &[BatchCharge], budget: u64) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut heap = 0u64;
    let mut mapped = 0u64;
    for (i, charge) in charges.iter().enumerate() {
        let full = i - start >= INDEX_BUILD_BATCH_SIZE
            || heap.saturating_add(charge.heap) > budget
            || mapped.saturating_add(charge.mapped) > MAPPED_BATCH_BYTES;
        if i > start && full {
            ranges.push(start..i);
            start = i;
            heap = 0;
            mapped = 0;
        }
        heap = heap.saturating_add(charge.heap);
        mapped = mapped.saturating_add(charge.mapped);
    }
    if start < charges.len() {
        ranges.push(start..charges.len());
    }
    ranges
}

/// Per-file sizes for reading, paired with their charges against the batch
/// budgets.
fn batch_sizes_and_charges(files: &[std::path::PathBuf]) -> (Vec<u64>, Vec<BatchCharge>) {
    files
        .par_iter()
        .map(
            |path| match std::fs::metadata(path).map(|meta| meta.len()) {
                Ok(size) => (size, batch_charge(size)),
                Err(_) => (INDEX_BUILD_BATCH_BYTES, UNKNOWN_SIZE_CHARGE),
            },
        )
        .unzip()
}

/// Smallest file worth memory-mapping instead of reading.
///
/// The same tradeoff the search path makes: mapping costs a syscall pair and a
/// page-table setup per file, which is a bad deal across the ~94k mostly-small
/// files of a kernel tree, but above this size it is what stops a large
/// generated header from costing its full size in heap in every worker that
/// touches one. Mapped pages are file-backed, so the kernel can reclaim them
/// under pressure instead of the process holding an allocation it cannot give
/// back.
///
/// 1 MiB is where the measurement pointed rather than a round guess. In the AMD
/// register headers — the worst case in the kernel tree — an 8 MiB threshold
/// mapped only 11 of 488 files and left 69% of the bytes on the heap, while
/// 1 MiB maps 102 files covering 87% of the bytes. It stays cheap on ordinary
/// trees because it is far above the size of real source: only 110 of the
/// kernel's 94,747 files reach it at all.
const MMAP_MIN_BYTES: u64 = 1024 * 1024;

/// The bytes of a file to index, either read onto the heap or borrowed from a
/// memory map.
enum FileBytes {
    Read {
        bytes: Vec<u8>,
        _permit: OwnedReadPermit,
    },
    Mapped(memmap2::Mmap),
}

struct OwnedReadBudget {
    capacity: u64,
    available: std::sync::Mutex<u64>,
    ready: std::sync::Condvar,
}

impl OwnedReadBudget {
    fn new(bytes: u64) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            capacity: bytes,
            available: std::sync::Mutex::new(bytes),
            ready: std::sync::Condvar::new(),
        })
    }

    fn acquire(self: &std::sync::Arc<Self>, bytes: u64) -> OwnedReadPermit {
        // One oversized fallback may exceed the ordinary budget, but it claims
        // the whole budget so no other owned read can overlap it.
        let charged = bytes.min(self.capacity);
        let mut available = self.available.lock().unwrap();
        while *available < charged {
            available = self.ready.wait(available).unwrap();
        }
        *available -= charged;
        OwnedReadPermit {
            budget: std::sync::Arc::clone(self),
            bytes: charged,
        }
    }
}

struct OwnedReadPermit {
    budget: std::sync::Arc<OwnedReadBudget>,
    bytes: u64,
}

impl Drop for OwnedReadPermit {
    fn drop(&mut self) {
        let mut available = self.budget.available.lock().unwrap();
        *available += self.bytes;
        self.budget.ready.notify_all();
    }
}

type ExtractedFile = (String, trigram::TrigramMaskMap);

/// Full-resolution identity used to validate that bytes came from one file
/// version without changing the persisted [`meta::FileStamp`] format.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    stamp: meta::FileStamp,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanos: i64,
}

impl FileVersion {
    pub fn stamp(&self) -> &meta::FileStamp {
        &self.stamp
    }
}

#[doc(hidden)]
pub fn file_version(metadata: &std::fs::Metadata) -> FileVersion {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    FileVersion {
        stamp: meta::file_stamp(metadata),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        change_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_nanos: metadata.ctime_nsec(),
    }
}

impl std::ops::Deref for FileBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            FileBytes::Read { bytes, .. } => bytes,
            FileBytes::Mapped(map) => map,
        }
    }
}

/// Get a file's bytes for indexing, mapping it when it is large enough to be
/// worth avoiding the copy.
///
/// Falls back to an owned read whenever mapping is unavailable. Ordinary reads
/// share the batch budget; an unlimited oversized fallback claims it
/// exclusively so at most one such allocation is in flight.
fn read_for_index(
    path: &Path,
    size: u64,
    owned_budget: &std::sync::Arc<OwnedReadBudget>,
    configured_limit: Option<u64>,
) -> std::io::Result<FileBytes> {
    let owned_limit = configured_limit.unwrap_or(u64::MAX);
    if size > owned_limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds the configured {} byte limit", owned_limit),
        ));
    }
    if size >= MMAP_MIN_BYTES {
        // SAFETY: the map is read-only and owned by the returned value, which
        // is dropped once the file's trigrams have been extracted. A concurrent
        // truncation would be undefined behaviour; that is inherent to mapping
        // a file being indexed, and is the same exposure the search path
        // accepts.
        let mapped =
            std::fs::File::open(path).and_then(|file| unsafe { memmap2::Mmap::map(&file) });
        if let Ok(map) = mapped
            && map.len() as u64 <= owned_limit
        {
            return Ok(FileBytes::Mapped(map));
        }
        // The file grew between the walk's stat and the map. Do not let a
        // successful mmap bypass a caller-supplied max-file-size limit.
    }
    let permit = owned_budget.acquire(size);
    match read_owned_for_index(path, size, owned_limit) {
        Ok(bytes) => Ok(FileBytes::Read {
            bytes,
            _permit: permit,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            // The file grew after it was stat'd. Release the smaller claim
            // before waiting for the retry allowance so concurrent growers
            // cannot deadlock while each holds part of the batch budget.
            drop(permit);
            match configured_limit {
                Some(limit) => {
                    let permit = owned_budget.acquire(limit);
                    read_owned_for_index(path, limit, limit)
                        .map(|bytes| FileBytes::Read {
                            bytes,
                            _permit: permit,
                        })
                        .map_err(|retry| {
                            if retry.kind() == std::io::ErrorKind::WouldBlock {
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("file exceeds the configured {} byte limit", limit),
                                )
                            } else {
                                retry
                            }
                        })
                }
                None => {
                    // No caller limit means no implicit fallback limit either.
                    // Claiming more than the budget takes it exclusively, so
                    // this unbounded read cannot overlap another owned buffer.
                    let permit = owned_budget.acquire(u64::MAX);
                    read_owned_unbounded(path, size).map(|bytes| FileBytes::Read {
                        bytes,
                        _permit: permit,
                    })
                }
            }
        }
        Err(error) => Err(error),
    }
}

fn read_owned_for_index(path: &Path, size: u64, owned_limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    if size > owned_limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds the configured {} byte limit", owned_limit),
        ));
    }
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    std::io::Read::by_ref(&mut file)
        .take(size)
        .read_to_end(&mut bytes)?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "file grew beyond its owned-buffer reservation",
        ));
    }
    Ok(bytes)
}

fn read_owned_unbounded(path: &Path, expected_size: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let initial_capacity = expected_size.min(MAX_OWNED_FILE_BYTES);
    let mut bytes = Vec::with_capacity(usize::try_from(initial_capacity).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Build a trigram index, choosing how postings are accumulated.
pub fn build_index_with_options(
    root: &Path,
    index_dir: Option<&Path>,
    opts: &BuildOptions,
) -> Result<BuildOutcome> {
    let root = std::fs::canonicalize(root)?;
    let ignorecase = (!opts.no_ignore)
        .then(|| crate::gitignore::CaseInsensitiveIgnore::new(&root, true, true, true))
        .flatten()
        .map(std::sync::Arc::new);
    build_index_with_options_and_ignorecase(&root, index_dir, opts, ignorecase)
}

/// Build an index using an immutable tracked-file exemption that the caller
/// can also use for watcher matcher publication.
pub fn build_index_with_options_and_ignorecase(
    root: &Path,
    index_dir: Option<&Path>,
    opts: &BuildOptions,
    ignorecase: Option<std::sync::Arc<crate::gitignore::CaseInsensitiveIgnore>>,
) -> Result<BuildOutcome> {
    let include_hidden = opts.include_hidden;
    let no_ignore = opts.no_ignore;
    let exclude_dirs = opts.exclude_dirs.as_slice();
    let root = std::fs::canonicalize(root)?;
    let index_dir = match index_dir {
        Some(d) => d.to_path_buf(),
        None => root.join(INDEX_DIR_NAME),
    };
    std::fs::create_dir_all(&index_dir)?;

    eprintln!("Walking {}...", root.display());
    if let Some(hint) = gitignore_gate_hint(&root, opts) {
        eprintln!("{hint}");
    }
    let walk = walker::walk_dir_with_ignorecase(
        &root,
        &walker::WalkOptions {
            include_hidden,
            no_ignore,
            no_require_git: opts.no_require_git,
            collect_gitignore_files: opts.collect_gitignore_files,
            exclude_dirs: exclude_dirs.to_vec(),
            max_file_size: opts.max_file_size,
            ..Default::default()
        },
        ignorecase,
    );
    eprintln!(
        "Found {} text files ({} binary skipped, {} too large, {} errors)",
        walk.files.len(),
        walk.skipped_binary,
        walk.skipped_too_large,
        walk.skipped_error
    );
    let gitignore_files = walk.gitignore_files;
    let ignore_files = walk.ignore_files;

    // Read files and extract trigrams with masks in bounded parallel batches.
    // Binary content check is done here (not in walker) to avoid an extra
    // 8KB read per file — we're already reading the full file anyway.
    eprintln!("Extracting trigrams...");
    let binary_skipped = std::sync::atomic::AtomicUsize::new(0);

    // Assign file IDs and collect posting entries. Batching avoids
    // retaining every file's per-trigram HashMap at once for large repos, and
    // bounding each batch by cumulative bytes caps how much raw file content is
    // resident at once, since the whole batch is read concurrently.
    let mut file_id_map: Vec<(u32, String)> = Vec::with_capacity(walk.files.len());
    let mut sink = match opts.strategy {
        IndexStrategy::InMemory => PostingSink::InMemory(Vec::new()),
        IndexStrategy::External => {
            std::fs::create_dir_all(&index_dir)?;
            PostingSink::External(ExternalSorter::new(&index_dir, opts.buffer_bytes))
        }
    };

    // The walk already stats every entry but discards the size, so recover it
    // here rather than widening WalkResult into the search and serve paths.
    let (sizes, charges) = batch_sizes_and_charges(&walk.files);

    for range in batch_ranges(&charges, INDEX_BUILD_BATCH_BYTES) {
        let batch = &walk.files[range.clone()];
        let batch_sizes = &sizes[range];
        let owned_limit_error = std::sync::Mutex::new(None);
        let owned_budget = OwnedReadBudget::new(INDEX_BUILD_BATCH_BYTES);
        let batch_data: Vec<ExtractedFile> = batch
            .par_iter()
            .zip(batch_sizes.par_iter())
            .filter_map(|(path, &size)| {
                let data = match read_for_index(path, size, &owned_budget, opts.max_file_size) {
                    Ok(data) => data,
                    Err(error) => {
                        if error.kind() == std::io::ErrorKind::InvalidData {
                            let mut first = owned_limit_error.lock().unwrap();
                            if first.is_none() {
                                *first = Some((path.clone(), error));
                            }
                        }
                        return None;
                    }
                };
                let text = crate::encoding::decode_for_index(&data);
                if trigram::is_binary(&text) {
                    binary_skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return None;
                }
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let per_tri = trigram::extract_merged_masks(&text);
                Some((rel, per_tri))
            })
            .collect();
        if let Some((path, error)) = owned_limit_error.into_inner().unwrap() {
            return Err(Error::Io(std::io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            )));
        }

        for (path, per_tri) in batch_data {
            let file_id = file_id_map.len() as u32;
            file_id_map.push((file_id, path));
            sink.push_file(file_id, per_tri)?;
        }
    }

    let extra_binary = binary_skipped.into_inner();
    if extra_binary > 0 {
        eprintln!(
            "Skipped {} additional binary files (detected by content)",
            extra_binary
        );
    }

    match sink {
        PostingSink::InMemory(mut postings) => {
            write_index_v2_from_postings(&index_dir, &root, &file_id_map, &mut postings)?;
        }
        PostingSink::External(sorter) => {
            let (trigram_count, segments) = sorter.write_postings(&index_dir)?;
            eprintln!(
                "Writing index ({} trigrams, {} files, {} spill segment(s))...",
                trigram_count,
                file_id_map.len(),
                segments
            );
            write_files_and_meta(
                &index_dir,
                &root,
                file_id_map.len(),
                file_id_map.iter().map(|(_, p)| p.as_str()),
                trigram_count,
                None,
            )?;
        }
    }

    // Write per-file stamps for ALL walked files (including those later
    // rejected as binary-by-content) so the stale check on next startup
    // won't re-process unchanged files that aren't in the index.
    let all_walked: Vec<String> = walk
        .files
        .iter()
        .filter_map(|p| p.strip_prefix(&root).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let stamps = meta::collect_filestamps(&root, &all_walked);
    meta::write_filestamps(&stamps, &index_dir)?;

    eprintln!("Index built successfully at {}", index_dir.display());
    Ok(BuildOutcome {
        num_files: file_id_map.len(),
        gitignore_files,
        ignore_files,
    })
}

/// Outcome of [`build_index_for_files`].
#[derive(Debug, Default)]
pub struct FileDeltaOutcome {
    /// Number of text files written into the delta index.
    pub indexed: usize,
    /// Paths that could not be read, and are therefore absent from the delta.
    ///
    /// The caller must drop these from the filestamp set it publishes. A stamp
    /// asserts "this file is indexed at this version"; publishing one for a
    /// file that was skipped makes the miss permanent, because every later
    /// reconcile sees a current stamp and never revisits the file. Withholding
    /// the stamp instead leaves the file looking new, so the next pass retries
    /// it — which is what should happen for a transient lock or permission
    /// error.
    ///
    /// Binary files are *not* listed here. They are skipped deliberately and
    /// permanently, so their stamps should be published as normal.
    pub unreadable: Vec<std::path::PathBuf>,
}

/// Build an external-sort index for an exact list of absolute file paths.
///
/// This is used by `tgrep serve` to build a bounded delta for files that are
/// absent from an otherwise-complete index. It deliberately skips the directory
/// walk and filestamp write: the caller already has both from its stale-state
/// scan and publishes the complete stamp set with the merged index.
///
/// Files that become unreadable or binary after the stale scan are omitted
/// rather than fatal, matching a normal index build. Aborting instead would be
/// far worse than it looks: for `tgrep serve` this delta is how the index is
/// persisted, so a single locked or deleted file would fail every save attempt
/// for the life of the process and no work would ever reach disk. See
/// [`FileDeltaOutcome::unreadable`] for the caller's obligation.
pub fn build_index_for_files(
    root: &Path,
    index_dir: &Path,
    files: &[std::path::PathBuf],
    buffer_bytes: usize,
) -> Result<FileDeltaOutcome> {
    let input_root = root;
    let root = std::fs::canonicalize(root)?;
    std::fs::create_dir_all(index_dir)?;

    let (sizes, charges) = batch_sizes_and_charges(files);
    let mut file_id_map: Vec<(u32, String)> = Vec::with_capacity(files.len());
    let mut sorter = ExternalSorter::new(index_dir, buffer_bytes);
    let mut unreadable: Vec<std::path::PathBuf> = Vec::new();

    for range in batch_ranges(&charges, INDEX_BUILD_BATCH_BYTES) {
        let batch = &files[range.clone()];
        let batch_sizes = &sizes[range];
        let owned_budget = OwnedReadBudget::new(INDEX_BUILD_BATCH_BYTES);
        let batch_data: Vec<std::result::Result<ExtractedFile, std::path::PathBuf>> = batch
            .par_iter()
            .zip(batch_sizes.par_iter())
            .filter_map(|(path, &size)| {
                let data = match read_for_index(path, size, &owned_budget, None) {
                    Ok(data) => data,
                    Err(error) => {
                        eprintln!("tgrep: skipping {}: {error}", path.display());
                        return Some(Err(path.clone()));
                    }
                };
                let text = crate::encoding::decode_for_index(&data);
                if trigram::is_binary(&text) {
                    return None;
                }
                let rel = path
                    .strip_prefix(&root)
                    .or_else(|_| path.strip_prefix(input_root))
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                Some(Ok((rel, trigram::extract_merged_masks(&text))))
            })
            .collect();

        for entry in batch_data {
            let (path, per_tri) = match entry {
                Ok(extracted) => extracted,
                Err(skipped) => {
                    unreadable.push(skipped);
                    continue;
                }
            };
            let file_id = u32::try_from(file_id_map.len()).map_err(|_| {
                Error::IndexCorrupted("file count exceeds the u32 file-id limit".into())
            })?;
            file_id_map.push((file_id, path));
            sorter.push_file(file_id, per_tri)?;
        }
    }

    let (trigram_count, _) = sorter.write_postings(index_dir)?;
    write_files_and_meta(
        index_dir,
        &root,
        file_id_map.len(),
        file_id_map.iter().map(|(_, path)| path.as_str()),
        trigram_count,
        Some(true),
    )?;
    Ok(FileDeltaOutcome {
        indexed: file_id_map.len(),
        unreadable,
    })
}

/// Return the default index directory for a given repo root.
pub fn default_index_dir(root: &Path) -> std::path::PathBuf {
    root.join(INDEX_DIR_NAME)
}

/// Internal: write the on-disk index files from a pre-computed inverted map.
/// `paths` provides the file list with IDs assigned by position: 0, 1, 2, …
fn write_index_files<'a>(
    index_dir: &Path,
    root: &Path,
    path_count: usize,
    paths: impl IntoIterator<Item = &'a str>,
    inverted: &HashMap<u32, Vec<PostingEntry>>,
    complete: Option<bool>,
) -> Result<()> {
    std::fs::create_dir_all(index_dir)?;

    let mut sorted_trigrams: Vec<u32> = inverted.keys().copied().collect();
    sorted_trigrams.sort_unstable();

    // Write index.bin — v2 posting entries with masks
    let mut postings_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("index.bin"))?);
    let mut lookup_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("lookup.bin"))?);
    let mut lookup_scratch =
        Vec::with_capacity(LOOKUP_WRITE_CHUNK_ENTRIES * ondisk::LOOKUP_ENTRY_SIZE);
    let mut posting_scratch =
        Vec::with_capacity(POSTING_WRITE_CHUNK_ENTRIES * ondisk::POSTING_ENTRY_SIZE);
    let mut offset: u64 = 0;

    for &tri in &sorted_trigrams {
        let posting_list = inverted.get(&tri).unwrap();
        let length = posting_list.len() as u32;

        write_lookup_entry(
            &mut lookup_file,
            LookupEntry {
                trigram: tri,
                offset,
                length,
            },
            &mut lookup_scratch,
        )?;

        write_posting_entries(&mut postings_file, posting_list, &mut posting_scratch)?;
        offset += length as u64 * ondisk::POSTING_ENTRY_SIZE as u64;
    }
    flush_lookup_entries(&mut lookup_file, &mut lookup_scratch)?;
    postings_file.flush()?;
    lookup_file.flush()?;

    write_files_and_meta(
        index_dir,
        root,
        path_count,
        paths,
        sorted_trigrams.len(),
        complete,
    )
}

fn write_posting_entries(
    writer: &mut impl Write,
    entries: &[PostingEntry],
    scratch: &mut Vec<u8>,
) -> Result<()> {
    for chunk in entries.chunks(POSTING_WRITE_CHUNK_ENTRIES) {
        scratch.clear();
        for entry in chunk {
            scratch.extend_from_slice(&entry.file_id.to_le_bytes());
            scratch.push(entry.loc_mask);
            scratch.push(entry.next_mask);
        }
        writer.write_all(scratch)?;
    }
    Ok(())
}

fn write_lookup_entry(
    writer: &mut impl Write,
    entry: LookupEntry,
    scratch: &mut Vec<u8>,
) -> Result<()> {
    if scratch.len() == scratch.capacity() {
        flush_lookup_entries(writer, scratch)?;
    }
    scratch.extend_from_slice(&entry.trigram.to_le_bytes());
    scratch.extend_from_slice(&entry.offset.to_le_bytes());
    scratch.extend_from_slice(&entry.length.to_le_bytes());
    Ok(())
}

fn flush_lookup_entries(writer: &mut impl Write, scratch: &mut Vec<u8>) -> Result<()> {
    if !scratch.is_empty() {
        writer.write_all(scratch)?;
        scratch.clear();
    }
    Ok(())
}

fn write_index_files_from_postings<'a>(
    index_dir: &Path,
    root: &Path,
    path_count: usize,
    paths: impl IntoIterator<Item = &'a str>,
    postings: &[TrigramPosting],
    trigram_count: usize,
    complete: Option<bool>,
) -> Result<()> {
    std::fs::create_dir_all(index_dir)?;

    let mut postings_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("index.bin"))?);
    let mut lookup_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("lookup.bin"))?);
    let mut lookup_scratch =
        Vec::with_capacity(LOOKUP_WRITE_CHUNK_ENTRIES * ondisk::LOOKUP_ENTRY_SIZE);
    let mut posting_scratch =
        Vec::with_capacity(POSTING_WRITE_CHUNK_ENTRIES * ondisk::POSTING_ENTRY_SIZE);

    let mut offset: u64 = 0;
    let mut written_trigrams = 0usize;
    let mut start = 0usize;
    while start < postings.len() {
        let trigram = postings[start].trigram;
        let mut end = start + 1;
        while end < postings.len() && postings[end].trigram == trigram {
            end += 1;
        }
        let length = (end - start) as u32;
        write_lookup_entry(
            &mut lookup_file,
            LookupEntry {
                trigram,
                offset,
                length,
            },
            &mut lookup_scratch,
        )?;
        write_flat_posting_entries(
            &mut postings_file,
            &postings[start..end],
            &mut posting_scratch,
        )?;
        offset += length as u64 * ondisk::POSTING_ENTRY_SIZE as u64;
        written_trigrams += 1;
        start = end;
    }
    debug_assert_eq!(written_trigrams, trigram_count);
    flush_lookup_entries(&mut lookup_file, &mut lookup_scratch)?;
    postings_file.flush()?;
    lookup_file.flush()?;

    write_files_and_meta(index_dir, root, path_count, paths, trigram_count, complete)
}

fn write_flat_posting_entries(
    writer: &mut impl Write,
    postings: &[TrigramPosting],
    scratch: &mut Vec<u8>,
) -> Result<()> {
    for chunk in postings.chunks(POSTING_WRITE_CHUNK_ENTRIES) {
        scratch.clear();
        for posting in chunk {
            let entry = posting.entry;
            scratch.extend_from_slice(&entry.file_id.to_le_bytes());
            scratch.push(entry.loc_mask);
            scratch.push(entry.next_mask);
        }
        writer.write_all(scratch)?;
    }
    Ok(())
}

fn write_files_and_meta<'a>(
    index_dir: &Path,
    root: &Path,
    path_count: usize,
    paths: impl IntoIterator<Item = &'a str>,
    trigram_count: usize,
    complete: Option<bool>,
) -> Result<()> {
    let mut files_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("files.bin"))?);
    for (id, path) in paths.into_iter().enumerate() {
        ondisk::write_file_entry(&mut files_file, id as u32, path)?;
    }
    files_file.flush()?;

    let canon_root = std::fs::canonicalize(root)?;
    let mut meta = IndexMeta::new(
        &canon_root.to_string_lossy(),
        path_count as u64,
        trigram_count as u64,
    );
    if let Some(c) = complete {
        meta.complete = c;
    }
    meta.save(index_dir)?;

    Ok(())
}

/// Preserves mask data (loc_mask/next_mask) from the snapshot so Bloom-filter
/// optimizations survive flush cycles.
pub fn write_index_from_snapshot(
    root: &Path,
    index_dir: &Path,
    paths: &[String],
    inverted: &HashMap<u32, Vec<PostingEntry>>,
    complete: bool,
) -> Result<()> {
    write_index_files(
        index_dir,
        root,
        paths.len(),
        paths.iter().map(String::as_str),
        inverted,
        Some(complete),
    )
}

/// Append a live overlay of **brand-new** files onto an existing on-disk index,
/// writing a fresh index into `out_dir` without ever materializing the existing
/// postings on the heap.
///
/// This is the memory-bounded flush used by the bulk indexer: rather than
/// merging reader + overlay into a single in-heap `HashMap` (which costs
/// O(total index size) memory and would defeat a memory cap), it streams a
/// 2-way merge of the reader's sorted lookup table (read straight from its mmap)
/// with the overlay's sorted trigram postings. The reader's posting bytes are
/// copied **verbatim** — they have the identical on-disk layout — so peak heap
/// stays bounded to the size of the overlay snapshot plus small write buffers,
/// independent of how large the existing index already is.
///
/// ## Append-only precondition
/// Every overlay file must be **new** (not already present in `reader`, and not
/// a deletion/supersession of a reader file). The bulk indexer guarantees this:
/// the file watcher and auto-save are both suppressed while the initial build is
/// in progress, so the overlay only ever accumulates fresh files. Under this
/// precondition the merge is a pure append:
/// - existing files keep their IDs `[0, base)`,
/// - overlay files take IDs `[base, base + overlay_paths.len())` in the order
///   given by `overlay_paths`,
/// - for any trigram, `reader_postings (ids < base) ++ overlay_postings
///   (ids >= base)` is already globally sorted by `file_id`.
///
/// `overlay_inverted` maps each trigram to the overlay's sorted, **zero-based**
/// file indices (as produced by [`crate::live::LiveIndex::snapshot_for_disk`]);
/// each index `k` refers to `overlay_paths[k]` and is written with disk ID
/// `base + k`. Overlay entries carry the no-filter sentinel masks
/// `(u8::MAX, u8::MAX)`, matching the bulk indexer's mask-free fast path.
pub fn append_overlay_to_index(
    root: &Path,
    out_dir: &Path,
    reader: &IndexReader,
    overlay_paths: &[String],
    overlay_inverted: &HashMap<u32, Vec<u32>>,
    complete: bool,
) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;

    // File IDs are `u32` on disk. Fail loudly rather than truncate.
    let base = u32::try_from(reader.num_files()).map_err(|_| {
        Error::IndexCorrupted(format!(
            "reader has {} files, exceeding the u32 file-id limit",
            reader.num_files()
        ))
    })?;

    // Overlay trigrams in ascending order for the 2-way merge.
    let mut overlay_trigrams: Vec<u32> = overlay_inverted.keys().copied().collect();
    overlay_trigrams.sort_unstable();

    let mut postings_file =
        std::io::BufWriter::new(std::fs::File::create(out_dir.join("index.bin"))?);
    let mut lookup_file =
        std::io::BufWriter::new(std::fs::File::create(out_dir.join("lookup.bin"))?);
    let mut lookup_scratch =
        Vec::with_capacity(LOOKUP_WRITE_CHUNK_ENTRIES * ondisk::LOOKUP_ENTRY_SIZE);
    let mut posting_scratch =
        Vec::with_capacity(POSTING_WRITE_CHUNK_ENTRIES * ondisk::POSTING_ENTRY_SIZE);

    let reader_trigram_count = reader.num_trigrams();
    let mut ri = 0usize;
    let mut oi = 0usize;
    let mut offset: u64 = 0;
    let mut trigram_count = 0usize;

    // Standard 2-way merge over two ascending trigram streams.
    while ri < reader_trigram_count || oi < overlay_trigrams.len() {
        let reader_next = (ri < reader_trigram_count)
            .then(|| reader.nth_trigram_raw(ri))
            .flatten();

        // A reader entry that exists in the lookup table but whose raw posting
        // bytes can't be read (truncated/corrupt mmap) yields `None` here while
        // `ri` is still in range. Silently skipping would drop that trigram yet
        // still publish an index, turning reader corruption into silent data
        // loss. Fail the flush instead so the caller keeps the previous reader
        // plus the live overlay as a safe fallback.
        if ri < reader_trigram_count && reader_next.is_none() {
            return Err(Error::IndexCorrupted(format!(
                "reader trigram entry {ri} of {reader_trigram_count} has unreadable \
                 postings; refusing to publish an incomplete merged index"
            )));
        }

        let overlay_next = overlay_trigrams.get(oi).copied();

        let (trigram, reader_bytes, overlay_seq) = match (reader_next, overlay_next) {
            (Some((rt, rbytes)), Some(ot)) => match rt.cmp(&ot) {
                std::cmp::Ordering::Less => {
                    ri += 1;
                    (rt, Some(rbytes), None)
                }
                std::cmp::Ordering::Greater => {
                    oi += 1;
                    (ot, None, overlay_inverted.get(&ot))
                }
                std::cmp::Ordering::Equal => {
                    ri += 1;
                    oi += 1;
                    (rt, Some(rbytes), overlay_inverted.get(&rt))
                }
            },
            (Some((rt, rbytes)), None) => {
                ri += 1;
                (rt, Some(rbytes), None)
            }
            (None, Some(ot)) => {
                oi += 1;
                (ot, None, overlay_inverted.get(&ot))
            }
            // Reader exhausted (in-range unreadable entries already errored above).
            (None, None) => break,
        };

        let reader_len = reader_bytes.map_or(0, |b| b.len() / ondisk::POSTING_ENTRY_SIZE);
        let overlay_len = overlay_seq.map_or(0, |v| v.len());
        let length = u32::try_from(
            reader_len
                .checked_add(overlay_len)
                .ok_or_else(|| Error::IndexCorrupted("posting list length overflow".into()))?,
        )
        .map_err(|_| {
            Error::IndexCorrupted(format!(
                "posting list for trigram {trigram} exceeds the u32 length limit"
            ))
        })?;
        if length == 0 {
            continue;
        }

        write_lookup_entry(
            &mut lookup_file,
            LookupEntry {
                trigram,
                offset,
                length,
            },
            &mut lookup_scratch,
        )?;

        // Reader postings: copy the on-disk bytes verbatim (zero decode).
        if let Some(rbytes) = reader_bytes {
            postings_file.write_all(rbytes)?;
        }
        // Overlay postings: encode with sentinel masks, IDs offset by `base`.
        if let Some(seq) = overlay_seq {
            for chunk in seq.chunks(POSTING_WRITE_CHUNK_ENTRIES) {
                posting_scratch.clear();
                for &k in chunk {
                    let file_id = base.checked_add(k).ok_or_else(|| {
                        Error::IndexCorrupted("overlay file id overflow beyond u32".into())
                    })?;
                    let entry = PostingEntry {
                        file_id,
                        loc_mask: u8::MAX,
                        next_mask: u8::MAX,
                    };
                    posting_scratch.extend_from_slice(&entry.encode());
                }
                postings_file.write_all(&posting_scratch)?;
            }
        }

        offset += length as u64 * ondisk::POSTING_ENTRY_SIZE as u64;
        trigram_count += 1;
    }

    flush_lookup_entries(&mut lookup_file, &mut lookup_scratch)?;
    postings_file.flush()?;
    lookup_file.flush()?;

    // files.bin + meta.json: existing files keep IDs [0, base), overlay files
    // follow at [base, base + N). Reader paths stream from its already-loaded
    // file table; overlay paths from the snapshot.
    let paths = reader
        .all_paths()
        .iter()
        .map(String::as_str)
        .chain(overlay_paths.iter().map(String::as_str));
    write_files_and_meta(
        out_dir,
        root,
        base as usize + overlay_paths.len(),
        paths,
        trigram_count,
        Some(complete),
    )
}

/// Stream-merge replacements and deletions into an existing index.
///
/// `delta` contains every new or replacement file. `removed_paths` identifies
/// reader files to omit; replacement paths therefore appear in both
/// `removed_paths` and `delta`. Reader file IDs are compacted through a dense
/// `u32` remap while each posting list is streamed, so heap use is O(files)
/// rather than O(all postings).
pub fn merge_index_with_delta(
    root: &Path,
    out_dir: &Path,
    reader: &IndexReader,
    delta: &IndexReader,
    removed_paths: &std::collections::HashSet<String>,
    complete: bool,
) -> Result<()> {
    let append_only = removed_paths.is_empty();
    std::fs::create_dir_all(out_dir)?;

    const DROPPED: u32 = u32::MAX;
    let mut next_id = 0u32;
    let mut reader_id_map = Vec::with_capacity(reader.num_files());
    for path in reader.all_paths() {
        if removed_paths.contains(path) {
            reader_id_map.push(DROPPED);
        } else {
            reader_id_map.push(next_id);
            next_id = next_id.checked_add(1).ok_or_else(|| {
                Error::IndexCorrupted("file count exceeds the u32 file-id limit".into())
            })?;
        }
    }
    let delta_base = next_id;
    let total_files = (delta_base as usize)
        .checked_add(delta.num_files())
        .ok_or_else(|| Error::IndexCorrupted("file count overflow".into()))?;
    u32::try_from(total_files)
        .map_err(|_| Error::IndexCorrupted("file count exceeds the u32 file-id limit".into()))?;

    let mut postings_file =
        std::io::BufWriter::new(std::fs::File::create(out_dir.join("index.bin"))?);
    let mut lookup_file =
        std::io::BufWriter::new(std::fs::File::create(out_dir.join("lookup.bin"))?);
    let mut lookup_scratch =
        Vec::with_capacity(LOOKUP_WRITE_CHUNK_ENTRIES * ondisk::LOOKUP_ENTRY_SIZE);
    let mut posting_scratch =
        Vec::with_capacity(POSTING_WRITE_CHUNK_ENTRIES * ondisk::POSTING_ENTRY_SIZE);

    let reader_trigram_count = reader.num_trigrams();
    let delta_trigram_count = delta.num_trigrams();
    let mut ri = 0usize;
    let mut di = 0usize;
    let mut offset = 0u64;
    let mut trigram_count = 0usize;

    while ri < reader_trigram_count || di < delta_trigram_count {
        let reader_next = (ri < reader_trigram_count)
            .then(|| reader.nth_trigram_raw(ri))
            .flatten();
        if ri < reader_trigram_count && reader_next.is_none() {
            return Err(Error::IndexCorrupted(format!(
                "reader trigram entry {ri} of {reader_trigram_count} has unreadable postings"
            )));
        }
        let delta_next = (di < delta_trigram_count)
            .then(|| delta.nth_trigram_raw(di))
            .flatten();
        if di < delta_trigram_count && delta_next.is_none() {
            return Err(Error::IndexCorrupted(format!(
                "delta trigram entry {di} of {delta_trigram_count} has unreadable postings"
            )));
        }

        let (trigram, reader_bytes, delta_bytes) = match (reader_next, delta_next) {
            (Some((rt, rbytes)), Some((dt, dbytes))) => match rt.cmp(&dt) {
                std::cmp::Ordering::Less => {
                    ri += 1;
                    (rt, Some(rbytes), None)
                }
                std::cmp::Ordering::Greater => {
                    di += 1;
                    (dt, None, Some(dbytes))
                }
                std::cmp::Ordering::Equal => {
                    ri += 1;
                    di += 1;
                    (rt, Some(rbytes), Some(dbytes))
                }
            },
            (Some((rt, rbytes)), None) => {
                ri += 1;
                (rt, Some(rbytes), None)
            }
            (None, Some((dt, dbytes))) => {
                di += 1;
                (dt, None, Some(dbytes))
            }
            (None, None) => break,
        };

        let mut reader_len = reader_bytes.map_or(0, |bytes| {
            usize::from(append_only) * (bytes.len() / ondisk::POSTING_ENTRY_SIZE)
        });
        if !append_only && let Some(bytes) = reader_bytes {
            for raw in bytes.as_chunks::<{ ondisk::POSTING_ENTRY_SIZE }>().0.iter() {
                let old_id = u32::from_le_bytes(raw[0..4].try_into().expect("four-byte file id"));
                let new_id = *reader_id_map.get(old_id as usize).ok_or_else(|| {
                    Error::IndexCorrupted(format!(
                        "posting for trigram {trigram} references missing file id {old_id}"
                    ))
                })?;
                reader_len += usize::from(new_id != DROPPED);
            }
        }
        let delta_len = delta_bytes.map_or(0, |bytes| bytes.len() / ondisk::POSTING_ENTRY_SIZE);
        let length = u32::try_from(
            reader_len
                .checked_add(delta_len)
                .ok_or_else(|| Error::IndexCorrupted("posting list length overflow".into()))?,
        )
        .map_err(|_| {
            Error::IndexCorrupted(format!(
                "posting list for trigram {trigram} exceeds the u32 length limit"
            ))
        })?;
        if length == 0 {
            continue;
        }

        write_lookup_entry(
            &mut lookup_file,
            LookupEntry {
                trigram,
                offset,
                length,
            },
            &mut lookup_scratch,
        )?;
        if let Some(bytes) = reader_bytes {
            if append_only {
                postings_file.write_all(bytes)?;
            } else {
                for chunk in bytes.chunks(POSTING_WRITE_CHUNK_ENTRIES * ondisk::POSTING_ENTRY_SIZE)
                {
                    posting_scratch.clear();
                    for raw in chunk.as_chunks::<{ ondisk::POSTING_ENTRY_SIZE }>().0.iter() {
                        let old_id =
                            u32::from_le_bytes(raw[0..4].try_into().expect("four-byte file id"));
                        let new_id = reader_id_map[old_id as usize];
                        if new_id != DROPPED {
                            posting_scratch.extend_from_slice(&new_id.to_le_bytes());
                            posting_scratch.extend_from_slice(&raw[4..]);
                        }
                    }
                    postings_file.write_all(&posting_scratch)?;
                }
            }
        }
        if let Some(bytes) = delta_bytes {
            for chunk in bytes.chunks(POSTING_WRITE_CHUNK_ENTRIES * ondisk::POSTING_ENTRY_SIZE) {
                posting_scratch.clear();
                for raw in chunk.as_chunks::<{ ondisk::POSTING_ENTRY_SIZE }>().0.iter() {
                    let file_id =
                        u32::from_le_bytes(raw[0..4].try_into().expect("four-byte file id"));
                    if file_id as usize >= delta.num_files() {
                        return Err(Error::IndexCorrupted(format!(
                            "delta posting references missing file id {file_id}"
                        )));
                    }
                    let file_id = delta_base.checked_add(file_id).ok_or_else(|| {
                        Error::IndexCorrupted("delta file id overflow beyond u32".into())
                    })?;
                    posting_scratch.extend_from_slice(&file_id.to_le_bytes());
                    posting_scratch.extend_from_slice(&raw[4..]);
                }
                postings_file.write_all(&posting_scratch)?;
            }
        }

        offset += length as u64 * ondisk::POSTING_ENTRY_SIZE as u64;
        trigram_count += 1;
    }

    flush_lookup_entries(&mut lookup_file, &mut lookup_scratch)?;
    postings_file.flush()?;
    lookup_file.flush()?;

    let paths = reader
        .all_paths()
        .iter()
        .filter(|path| !removed_paths.contains(path.as_str()))
        .map(String::as_str)
        .chain(delta.all_paths().iter().map(String::as_str));
    write_files_and_meta(
        out_dir,
        root,
        total_files,
        paths,
        trigram_count,
        Some(complete),
    )
}

fn write_index_v2_from_postings(
    index_dir: &Path,
    root: &Path,
    file_id_map: &[(u32, String)],
    postings: &mut [TrigramPosting],
) -> Result<()> {
    postings.sort_unstable_by(|a, b| {
        a.trigram
            .cmp(&b.trigram)
            .then_with(|| a.entry.file_id.cmp(&b.entry.file_id))
    });
    let trigram_count = count_sorted_trigrams(postings);
    eprintln!(
        "Writing index ({} trigrams, {} files)...",
        trigram_count,
        file_id_map.len()
    );
    write_index_files_from_postings(
        index_dir,
        root,
        file_id_map.len(),
        file_id_map.iter().map(|(_, p)| p.as_str()),
        postings,
        trigram_count,
        None,
    )?;
    Ok(())
}

fn count_sorted_trigrams(postings: &[TrigramPosting]) -> usize {
    let mut count = 0usize;
    let mut previous = None;
    for posting in postings {
        if previous != Some(posting.trigram) {
            count += 1;
            previous = Some(posting.trigram);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::IndexReader;

    const MB: u64 = 1024 * 1024;

    fn charges(sizes: &[u64]) -> Vec<BatchCharge> {
        sizes.iter().copied().map(batch_charge).collect()
    }

    #[test]
    fn batches_are_bounded_by_cumulative_heap_bytes() {
        // Files below the mapping threshold are read, so their whole length is
        // heap: five 900 KiB reads against a 2 MiB budget fit two at a time.
        let charges = charges(&[900 * 1024; 5]);
        assert_eq!(batch_ranges(&charges, 2 * MB), vec![0..2, 2..4, 4..5]);
    }

    #[test]
    fn large_mapped_files_still_fill_a_batch() {
        // Regression: charging a mapped file its own length against the heap
        // budget made a batch of large files degenerate to one or two, leaving
        // every worker but one idle. Mapped bytes never reach the heap, so only
        // the mapped budget bounds these: 256 MiB / 32 MiB is eight per batch.
        let charges = charges(&[32 * MB; 16]);
        assert_eq!(
            batch_ranges(&charges, INDEX_BUILD_BATCH_BYTES),
            vec![0..8, 8..16]
        );
    }

    #[test]
    fn mapped_bytes_in_flight_stay_bounded() {
        // The other half of the bound: a batch may not map more than the mapped
        // budget, whatever mix of sizes it is handed.
        let sizes: Vec<u64> = (0..200).map(|i| (i % 9 + 1) * 8 * MB).collect();
        let charges = charges(&sizes);
        for range in batch_ranges(&charges, INDEX_BUILD_BATCH_BYTES) {
            let single = range.end - range.start == 1;
            let mapped: u64 = charges[range.clone()].iter().map(|c| c.mapped).sum();
            let heap: u64 = charges[range.clone()].iter().map(|c| c.heap).sum();
            assert!(
                single || mapped <= MAPPED_BATCH_BYTES,
                "batch {range:?} maps {mapped} bytes"
            );
            assert!(
                single || heap <= INDEX_BUILD_BATCH_BYTES,
                "batch {range:?} reads {heap} bytes"
            );
        }
    }

    #[test]
    fn batches_are_still_bounded_by_file_count() {
        // Tiny files never reach either byte budget, so the count bound applies.
        let charges = charges(&[1u64; INDEX_BUILD_BATCH_SIZE * 2 + 5]);
        let ranges = batch_ranges(&charges, 64 * MB);
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0], 0..INDEX_BUILD_BATCH_SIZE);
    }

    #[test]
    fn a_file_larger_than_the_budget_gets_its_own_batch() {
        // The oversized file is never split, and never drags neighbours along.
        let charges = charges(&[MB, 500 * MB, MB]);
        assert_eq!(
            batch_ranges(&charges, INDEX_BUILD_BATCH_BYTES),
            vec![0..1, 1..2, 2..3]
        );
    }

    #[test]
    fn a_file_whose_size_is_unknown_gets_its_own_batch() {
        // A failed `metadata()` is recorded as a whole budget rather than as
        // zero, so the unstattable file is isolated instead of being waved into
        // a full batch as if it were empty.
        let charges = vec![batch_charge(MB), UNKNOWN_SIZE_CHARGE, batch_charge(MB)];
        assert_eq!(
            batch_ranges(&charges, INDEX_BUILD_BATCH_BYTES),
            vec![0..1, 1..2, 2..3]
        );
    }

    #[test]
    fn owned_read_rejects_files_beyond_the_batch_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sparse.txt");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_OWNED_FILE_BYTES + 1)
            .unwrap();

        let error = read_owned_for_index(&path, MAX_OWNED_FILE_BYTES + 1, MAX_OWNED_FILE_BYTES)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn configured_limit_is_checked_before_mmap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("too-large.txt");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MMAP_MIN_BYTES)
            .unwrap();
        let budget = OwnedReadBudget::new(INDEX_BUILD_BATCH_BYTES);

        let error = match read_for_index(&path, MMAP_MIN_BYTES, &budget, Some(MMAP_MIN_BYTES - 1)) {
            Ok(_) => panic!("configured max-file-size must be checked before mmap"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn owned_read_reports_growth_beyond_its_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("growing.txt");
        std::fs::write(&path, b"four").unwrap();

        let error = read_owned_for_index(&path, 2, 2).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn indexed_read_retries_growth_with_the_full_batch_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("growing.txt");
        std::fs::write(&path, b"four").unwrap();
        let budget = OwnedReadBudget::new(INDEX_BUILD_BATCH_BYTES);

        let bytes = read_for_index(&path, 2, &budget, None).unwrap();
        assert_eq!(&*bytes, b"four");
    }

    #[test]
    fn owned_read_permit_releases_batch_capacity() {
        let budget = OwnedReadBudget::new(10);
        let permit = budget.acquire(7);
        assert_eq!(*budget.available.lock().unwrap(), 3);
        drop(permit);
        assert_eq!(*budget.available.lock().unwrap(), 10);
    }

    #[test]
    fn oversized_owned_read_claims_the_budget_exclusively() {
        let budget = OwnedReadBudget::new(10);
        let permit = budget.acquire(100);
        assert_eq!(*budget.available.lock().unwrap(), 0);
        drop(permit);
        assert_eq!(*budget.available.lock().unwrap(), 10);
    }

    #[test]
    fn owned_read_budget_serializes_fallbacks_that_exceed_the_remaining_capacity() {
        let budget = OwnedReadBudget::new(10);
        let first = budget.acquire(100);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let waiting_budget = std::sync::Arc::clone(&budget);
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _second = waiting_budget.acquire(100);
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "a second fallback must wait rather than exceed the shared budget"
        );
        drop(first);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn file_version_distinguishes_subsecond_modified_times() {
        let stamp = meta::FileStamp { mtime: 1, size: 10 };
        let base = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        let first = FileVersion {
            stamp: stamp.clone(),
            modified: Some(base + std::time::Duration::from_nanos(100)),
            created: Some(base),
            #[cfg(unix)]
            device: 1,
            #[cfg(unix)]
            inode: 2,
            #[cfg(unix)]
            change_seconds: 1,
            #[cfg(unix)]
            change_nanos: 0,
        };
        let second = FileVersion {
            modified: Some(base + std::time::Duration::from_nanos(200)),
            ..first.clone()
        };

        assert_ne!(first, second);
        assert_eq!(first.stamp, second.stamp);
    }

    fn write_test_git_index(root: &Path, tracked: &[&str]) {
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), "[core]\n\tignorecase = true\n").unwrap();
        let mut index = Vec::new();
        index.extend_from_slice(b"DIRC");
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&(tracked.len() as u32).to_be_bytes());
        for path in tracked {
            let start = index.len();
            index.extend_from_slice(&[0u8; 60]);
            index.extend_from_slice(&(path.len() as u16).to_be_bytes());
            index.extend_from_slice(path.as_bytes());
            index.push(0);
            while (index.len() - start) % 8 != 0 {
                index.push(0);
            }
        }
        std::fs::write(git.join("index"), index).unwrap();
    }

    #[test]
    fn default_builder_constructs_snapshot_but_explicit_none_is_preserved() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir(root.join("ignored")).unwrap();
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        std::fs::write(root.join("ignored/tracked.rs"), "fn tracked() {}\n").unwrap();
        std::fs::write(root.join("ignored/untracked.rs"), "fn untracked() {}\n").unwrap();
        write_test_git_index(root, &[".gitignore", "ignored/tracked.rs"]);

        let default_index = tempfile::tempdir().unwrap();
        build_index_with_options(root, Some(default_index.path()), &BuildOptions::default())
            .unwrap();
        let default_reader = IndexReader::open(default_index.path()).unwrap();
        assert!(
            default_reader.contains_path("ignored/tracked.rs"),
            "the default builder must construct the tracked-file exemption"
        );
        assert!(
            !default_reader.contains_path("ignored/untracked.rs"),
            "the default builder must apply case-insensitive root rules"
        );

        let none_index = tempfile::tempdir().unwrap();
        build_index_with_options_and_ignorecase(
            root,
            Some(none_index.path()),
            &BuildOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            IndexReader::open(none_index.path())
                .unwrap()
                .contains_path("ignored/untracked.rs"),
            "an explicit None must not reconstruct the exemption"
        );
    }

    #[test]
    fn batches_cover_every_file_exactly_once() {
        let sizes: Vec<u64> = (0..500).map(|i| (i as u64 % 7) * 9 * MB).collect();
        let charges = charges(&sizes);
        let ranges = batch_ranges(&charges, 64 * MB);
        let mut next = 0usize;
        for range in &ranges {
            assert_eq!(range.start, next, "gap or overlap between batches");
            assert!(range.start < range.end, "empty batch");
            next = range.end;
        }
        assert_eq!(next, sizes.len(), "batches must cover the whole walk");
    }

    #[test]
    fn an_empty_walk_produces_no_batches() {
        assert!(batch_ranges(&[], 64 * MB).is_empty());
    }

    /// Build a repo with enough varied content that a small arena spills.
    fn write_sample_repo(root: &Path, file_count: usize) {
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..file_count {
            let mut state = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
            let mut data = format!("// File {i}\nfn Handler{i}() {{ needle(); }}\n").into_bytes();
            while data.len() < 2048 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                data.push(32 + ((state >> 33) % 94) as u8);
            }
            std::fs::write(src.join(format!("f{i:04}.rs")), data).unwrap();
        }
    }

    /// Canonical, walk-order-independent view of an index: every trigram
    /// mapped to its postings resolved to *paths* rather than file IDs.
    ///
    /// The parallel walker yields files in nondeterministic order, so two
    /// builds of the same repo legitimately assign different file IDs and
    /// produce different bytes. Resolving through paths compares what the
    /// index actually means. (Byte-for-byte equivalence of the two write
    /// paths for identical input is covered by `external`'s unit tests.)
    fn index_fingerprint(
        index_dir: &Path,
    ) -> std::collections::BTreeMap<u32, Vec<(String, u8, u8)>> {
        let reader = IndexReader::open(index_dir).unwrap();
        let mut fingerprint = std::collections::BTreeMap::new();
        for (trigram, entries) in reader.all_trigram_postings_with_masks() {
            let ids: Vec<u32> = entries.iter().map(|e| e.file_id).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            assert_eq!(
                ids, sorted,
                "posting list for trigram {trigram:#08x} is not file-id sorted"
            );

            let mut resolved: Vec<(String, u8, u8)> = entries
                .iter()
                .map(|e| {
                    let path = reader
                        .file_path(e.file_id)
                        .unwrap_or_else(|| panic!("unresolved file id {}", e.file_id))
                        .to_string();
                    (path, e.loc_mask, e.next_mask)
                })
                .collect();
            resolved.sort();
            fingerprint.insert(trigram, resolved);
        }
        fingerprint
    }

    // The memory-bounded path is the default; a refactor that silently
    // reverted it would reintroduce unbounded peak memory on large repos
    // without failing any other test.
    #[test]
    fn external_is_the_default_strategy() {
        assert_eq!(IndexStrategy::default(), IndexStrategy::External);
        assert_eq!(BuildOptions::default().strategy, IndexStrategy::External);
    }

    // `tgrep serve` walks with leading-dot hidden semantics and its watcher
    // skips dot-prefixed paths, so a build that feeds a server must exclude
    // dotfiles. On Windows the default walk keeps them (the native hidden
    // check reads an attribute git never sets), which would leave the server
    // holding entries it can never update or delete.
    #[test]
    fn collect_gitignore_files_excludes_dotfiles_from_the_index() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("main.rs"),
            "fn main() { needle(); }\n",
        )
        .unwrap();
        std::fs::write(root.join(".gitignore"), "# needle marker\n*.log\n").unwrap();
        std::fs::write(root.join(".mailmap"), "needle <needle@example.com>\n").unwrap();

        let for_serve = tempfile::tempdir().unwrap();
        build_index_with_options(
            root,
            Some(for_serve.path()),
            &BuildOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        )
        .unwrap();

        let reader = IndexReader::open(for_serve.path()).unwrap();
        let indexed: Vec<String> = (0..reader.num_files() as u32)
            .filter_map(|id| reader.file_path(id).map(|p| p.to_string()))
            .collect();
        assert_eq!(
            indexed,
            vec!["src/main.rs".to_string()],
            "a build destined for the server must skip dot-prefixed files"
        );
    }

    #[test]
    fn external_strategy_matches_in_memory_index_contents() {
        let repo = tempfile::tempdir().unwrap();
        write_sample_repo(repo.path(), 40);

        let in_memory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();

        build_index_with_options(
            repo.path(),
            Some(in_memory.path()),
            &BuildOptions {
                strategy: IndexStrategy::InMemory,
                ..Default::default()
            },
        )
        .unwrap();

        // A 1-byte budget clamps to the minimum arena, forcing many spills.
        build_index_with_options(
            repo.path(),
            Some(external.path()),
            &BuildOptions {
                strategy: IndexStrategy::External,
                buffer_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();

        // Same total bytes: identical posting count, identical layout.
        let expected_len = std::fs::metadata(in_memory.path().join("index.bin"))
            .unwrap()
            .len();
        let actual_len = std::fs::metadata(external.path().join("index.bin"))
            .unwrap()
            .len();
        assert!(expected_len > 0, "fixture should produce a non-empty index");
        assert_eq!(expected_len, actual_len, "index.bin size differs");

        let expected = index_fingerprint(in_memory.path());
        let actual = index_fingerprint(external.path());
        assert_eq!(
            expected.len(),
            actual.len(),
            "distinct trigram count differs"
        );
        for (trigram, expected_postings) in &expected {
            let actual_postings = actual
                .get(trigram)
                .unwrap_or_else(|| panic!("trigram {trigram:#08x} missing from external index"));
            assert!(
                expected_postings == actual_postings,
                "postings differ for trigram {trigram:#08x}: \
                 in-memory has {} entries, external has {}",
                expected_postings.len(),
                actual_postings.len()
            );
        }
    }

    #[test]
    fn external_strategy_index_is_searchable() {
        let repo = tempfile::tempdir().unwrap();
        write_sample_repo(repo.path(), 60);

        let index = tempfile::tempdir().unwrap();
        build_index_with_options(
            repo.path(),
            Some(index.path()),
            &BuildOptions {
                strategy: IndexStrategy::External,
                buffer_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let reader = IndexReader::open(index.path()).unwrap();
        reader.validate_lookup().unwrap();
        assert_eq!(reader.num_files(), 60);

        // "needle" appears in every file, so its trigram must list all of them
        // in ascending file-id order after the merge.
        let entries = reader.lookup_trigram_with_masks(crate::trigram::hash(b'n', b'e', b'e'));
        assert_eq!(entries.len(), 60);
        let ids: Vec<u32> = entries.iter().map(|e| e.file_id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "merged posting lists must be file-id sorted");
    }

    #[test]
    fn external_strategy_leaves_no_spill_directory_behind() {
        let repo = tempfile::tempdir().unwrap();
        write_sample_repo(repo.path(), 40);

        let index = tempfile::tempdir().unwrap();
        build_index_with_options(
            repo.path(),
            Some(index.path()),
            &BuildOptions {
                strategy: IndexStrategy::External,
                buffer_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let leftover: Vec<String> = std::fs::read_dir(index.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("spill-"))
            .collect();
        assert!(leftover.is_empty(), "spill dirs left behind: {leftover:?}");
    }

    #[test]
    fn build_index_writes_readable_round_trip_index() {
        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "hello world\nneedle one\n").unwrap();
        std::fs::write(src.join("b.txt"), "needle two\nother content\n").unwrap();

        let index = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(index.path()), false, false, &[]).unwrap();

        let reader = IndexReader::open(index.path()).unwrap();
        reader.validate_lookup().unwrap();
        assert_eq!(reader.num_files(), 2);

        let hello = reader.lookup_trigram(crate::trigram::hash(b'h', b'e', b'l'));
        let hello_paths: Vec<&str> = hello
            .iter()
            .filter_map(|&file_id| reader.file_path(file_id))
            .collect();
        assert_eq!(hello_paths, vec!["src/a.txt"]);

        let needle = reader.lookup_trigram(crate::trigram::hash(b'n', b'e', b'e'));
        let mut needle_paths: Vec<&str> = needle
            .iter()
            .filter_map(|&file_id| reader.file_path(file_id))
            .collect();
        needle_paths.sort_unstable();
        assert_eq!(needle_paths, vec!["src/a.txt", "src/b.txt"]);
    }

    #[test]
    fn build_index_no_ignore_includes_p4ignored_files() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("keep.txt"), "keep searchable\n").unwrap();
        std::fs::write(repo.path().join("asset.txt"), "asset searchable\n").unwrap();
        std::fs::write(repo.path().join("p4ignore.ini"), "asset.txt\n").unwrap();

        let ignored_index = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(ignored_index.path()), false, false, &[]).unwrap();
        let ignored_reader = IndexReader::open(ignored_index.path()).unwrap();
        assert!(
            !(0..ignored_reader.num_files() as u32)
                .filter_map(|id| ignored_reader.file_path(id))
                .any(|path| path == "asset.txt")
        );

        let unrestricted_index = tempfile::tempdir().unwrap();
        build_index(
            repo.path(),
            Some(unrestricted_index.path()),
            false,
            true,
            &[],
        )
        .unwrap();
        let unrestricted_reader = IndexReader::open(unrestricted_index.path()).unwrap();
        assert!(
            (0..unrestricted_reader.num_files() as u32)
                .filter_map(|id| unrestricted_reader.file_path(id))
                .any(|path| path == "asset.txt")
        );
    }

    #[test]
    fn append_overlay_merges_new_files_into_complete_index() {
        use crate::live::LiveIndex;

        // Base index with two files.
        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "hello world\nneedle one\n").unwrap();
        std::fs::write(src.join("b.txt"), "needle two\nother content\n").unwrap();

        let base_dir = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(base_dir.path()), false, false, &[]).unwrap();
        let base_reader = IndexReader::open(base_dir.path()).unwrap();
        assert_eq!(base_reader.num_files(), 2);

        // Build a live overlay of two brand-new files (append-only invariant).
        let mut live = LiveIndex::new();
        live.upsert_file_with_trigrams("src/c.txt", crate::trigram::extract(b"needle three\n"));
        live.upsert_file_with_trigrams("src/d.txt", crate::trigram::extract(b"zzz unique\n"));
        let (overlay_paths, overlay_inverted) = live.snapshot_for_disk();

        // Stream-merge overlay onto the base index into a fresh dir.
        let merged_dir = tempfile::tempdir().unwrap();
        append_overlay_to_index(
            repo.path(),
            merged_dir.path(),
            &base_reader,
            &overlay_paths,
            &overlay_inverted,
            true,
        )
        .unwrap();

        let merged = IndexReader::open(merged_dir.path()).unwrap();
        merged.validate_lookup().unwrap();
        assert_eq!(merged.num_files(), 4, "all base + overlay files present");

        // Base file IDs are preserved (copied verbatim at the front).
        assert_eq!(merged.file_path(0), base_reader.file_path(0));
        assert_eq!(merged.file_path(1), base_reader.file_path(1));
        // Overlay files follow in insertion order.
        assert_eq!(merged.file_path(2), Some("src/c.txt"));
        assert_eq!(merged.file_path(3), Some("src/d.txt"));

        // A trigram shared by base + overlay returns all three files, sorted.
        let needle = crate::trigram::hash(b'n', b'e', b'e');
        let mut needle_paths: Vec<&str> = merged
            .lookup_trigram(needle)
            .iter()
            .filter_map(|&id| merged.file_path(id))
            .collect();
        needle_paths.sort_unstable();
        assert_eq!(needle_paths, vec!["src/a.txt", "src/b.txt", "src/c.txt"]);

        // An overlay-only trigram resolves to just the overlay file.
        let uni = crate::trigram::hash(b'u', b'n', b'i');
        let uni_paths: Vec<&str> = merged
            .lookup_trigram(uni)
            .iter()
            .filter_map(|&id| merged.file_path(id))
            .collect();
        assert_eq!(uni_paths, vec!["src/d.txt"]);

        // Posting lists stay globally sorted by file_id after the merge.
        let needle_ids = merged.lookup_trigram(needle);
        let mut sorted = needle_ids.clone();
        sorted.sort_unstable();
        assert_eq!(needle_ids, sorted, "merged posting list must be sorted");

        // Masks: base entries keep their real masks; overlay entries carry the
        // no-filter sentinel (the bulk path stores no masks).
        let c_id = (0..merged.num_files() as u32)
            .find(|&id| merged.file_path(id) == Some("src/c.txt"))
            .unwrap();
        let entries = merged.lookup_trigram_with_masks(needle);
        let c_entry = entries.iter().find(|e| e.file_id == c_id).unwrap();
        assert_eq!(c_entry.loc_mask, u8::MAX);
        assert_eq!(c_entry.next_mask, u8::MAX);
        let base_entry = entries.iter().find(|e| e.file_id < 2).unwrap();
        assert_ne!(
            base_entry.loc_mask,
            u8::MAX,
            "base file's real loc_mask must be preserved verbatim"
        );
    }

    /// Incremental flushes stream new files onto whatever index is already on
    /// disk, so the append path must work identically on an external-built
    /// base. `external` only changes how postings are accumulated in memory,
    /// not the on-disk format, but that is exactly the kind of assumption worth
    /// pinning down: a base built through the spill/merge path must remain a
    /// valid input to `append_overlay_to_index`.
    #[test]
    fn append_overlay_merges_onto_an_external_built_index() {
        use crate::live::LiveIndex;

        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "hello world\nneedle one\n").unwrap();
        std::fs::write(src.join("b.txt"), "needle two\nother content\n").unwrap();

        // Build the base through the spill/merge path. A 1-byte budget floors
        // the arena at its minimum so this is a real merge, not the fast path.
        let base_dir = tempfile::tempdir().unwrap();
        build_index_with_options(
            repo.path(),
            Some(base_dir.path()),
            &BuildOptions {
                strategy: IndexStrategy::External,
                buffer_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let base_reader = IndexReader::open(base_dir.path()).unwrap();
        assert_eq!(base_reader.num_files(), 2);

        let mut live = LiveIndex::new();
        live.upsert_file_with_trigrams("src/c.txt", crate::trigram::extract(b"needle three\n"));
        live.upsert_file_with_trigrams("src/d.txt", crate::trigram::extract(b"zzz unique\n"));
        let (overlay_paths, overlay_inverted) = live.snapshot_for_disk();

        let merged_dir = tempfile::tempdir().unwrap();
        append_overlay_to_index(
            repo.path(),
            merged_dir.path(),
            &base_reader,
            &overlay_paths,
            &overlay_inverted,
            true,
        )
        .unwrap();

        let merged = IndexReader::open(merged_dir.path()).unwrap();
        merged.validate_lookup().unwrap();
        assert_eq!(merged.num_files(), 4);

        // Base IDs preserved, overlay appended after them.
        assert_eq!(merged.file_path(0), base_reader.file_path(0));
        assert_eq!(merged.file_path(1), base_reader.file_path(1));
        assert_eq!(merged.file_path(2), Some("src/c.txt"));
        assert_eq!(merged.file_path(3), Some("src/d.txt"));

        // A trigram spanning base and overlay resolves across both halves and
        // stays file-id sorted, which is what query execution relies on.
        let needle = crate::trigram::hash(b'n', b'e', b'e');
        let ids = merged.lookup_trigram(needle);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "merged posting list must be sorted");
        let mut paths: Vec<&str> = ids.iter().filter_map(|&id| merged.file_path(id)).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["src/a.txt", "src/b.txt", "src/c.txt"]);
    }

    /// An unreadable input must not sink the whole delta. For `tgrep serve`
    /// this build *is* index persistence, so aborting on one locked or
    /// vanished file would stop anything from ever reaching disk. The file is
    /// skipped and reported so the caller can withhold its stamp and retry.
    #[test]
    fn exact_file_delta_skips_and_reports_an_unreadable_input() {
        let repo = tempfile::tempdir().unwrap();
        let readable = repo.path().join("present.txt");
        std::fs::write(&readable, "needle one\n").unwrap();
        let missing = repo.path().join("vanished.txt");
        let delta = tempfile::tempdir().unwrap();

        let outcome = build_index_for_files(
            repo.path(),
            delta.path(),
            &[readable, missing.clone()],
            DEFAULT_INDEX_BUFFER_BYTES,
        )
        .unwrap();

        assert_eq!(
            outcome.indexed, 1,
            "the readable file must still be indexed"
        );
        assert_eq!(outcome.unreadable, vec![missing]);

        // The delta is usable, not a half-written casualty of the failure.
        let reader = IndexReader::open(delta.path()).unwrap();
        assert_eq!(reader.num_files(), 1);
        assert_eq!(reader.file_path(0), Some("present.txt"));
    }

    /// A binary file is skipped deliberately and permanently, so it must *not*
    /// be reported as unreadable - doing so would make the caller re-read it
    /// on every reconcile forever.
    #[test]
    fn exact_file_delta_does_not_report_binary_files_as_unreadable() {
        let repo = tempfile::tempdir().unwrap();
        let binary = repo.path().join("blob.bin");
        std::fs::write(&binary, [0u8, 1, 2, 0, 3, 0]).unwrap();
        let delta = tempfile::tempdir().unwrap();

        let outcome = build_index_for_files(
            repo.path(),
            delta.path(),
            &[binary],
            DEFAULT_INDEX_BUFFER_BYTES,
        )
        .unwrap();

        assert_eq!(outcome.indexed, 0);
        assert!(outcome.unreadable.is_empty());
    }

    #[test]
    fn exact_file_delta_streams_onto_an_existing_index() {
        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "hello world\nneedle one\n").unwrap();
        std::fs::write(src.join("b.txt"), "needle two\nother content\n").unwrap();

        let base_dir = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(base_dir.path()), false, false, &[]).unwrap();
        let base_reader = IndexReader::open(base_dir.path()).unwrap();

        let c = src.join("c.txt");
        let d = src.join("d.txt");
        let binary = src.join("binary.dat");
        std::fs::write(&c, "Needle three\n").unwrap();
        std::fs::write(&d, "zzz unique\n").unwrap();
        std::fs::write(&binary, b"text prefix\0binary tail").unwrap();

        let delta_dir = tempfile::tempdir().unwrap();
        let delta_count = build_index_for_files(repo.path(), delta_dir.path(), &[c, d, binary], 1)
            .unwrap()
            .indexed;
        assert_eq!(delta_count, 2, "binary files must stay out of the delta");
        let delta_reader = IndexReader::open(delta_dir.path()).unwrap();

        let merged_dir = tempfile::tempdir().unwrap();
        merge_index_with_delta(
            repo.path(),
            merged_dir.path(),
            &base_reader,
            &delta_reader,
            &std::collections::HashSet::new(),
            true,
        )
        .unwrap();

        let merged = IndexReader::open(merged_dir.path()).unwrap();
        merged.validate_lookup().unwrap();
        assert_eq!(merged.num_files(), 4);
        assert_eq!(merged.file_path(0), base_reader.file_path(0));
        assert_eq!(merged.file_path(1), base_reader.file_path(1));
        assert_eq!(merged.file_path(2), Some("src/c.txt"));
        assert_eq!(merged.file_path(3), Some("src/d.txt"));

        let needle = crate::trigram::hash(b'n', b'e', b'e');
        let entries = merged.lookup_trigram_with_masks(needle);
        let mut paths: Vec<&str> = entries
            .iter()
            .filter_map(|entry| merged.file_path(entry.file_id))
            .collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec!["src/a.txt", "src/b.txt", "src/c.txt"],
            "delta postings must be offset and merged with base postings"
        );
        let c_entry = entries
            .iter()
            .find(|entry| merged.file_path(entry.file_id) == Some("src/c.txt"))
            .unwrap();
        assert_ne!(
            (c_entry.loc_mask, c_entry.next_mask),
            (u8::MAX, u8::MAX),
            "the external delta must preserve real masks"
        );
    }

    #[test]
    fn streamed_delta_can_replace_and_delete_reader_files() {
        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "keep alpha needle\n").unwrap();
        std::fs::write(src.join("b.txt"), "old beta needle\n").unwrap();
        std::fs::write(src.join("c.txt"), "delete gamma needle\n").unwrap();

        let base_dir = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(base_dir.path()), false, false, &[]).unwrap();
        let base_reader = IndexReader::open(base_dir.path()).unwrap();

        let b = src.join("b.txt");
        let d = src.join("d.txt");
        std::fs::write(&b, "replacement beta unique\n").unwrap();
        std::fs::remove_file(src.join("c.txt")).unwrap();
        std::fs::write(&d, "new delta unique\n").unwrap();

        let delta_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            build_index_for_files(repo.path(), delta_dir.path(), &[b, d], 1)
                .unwrap()
                .indexed,
            2
        );
        let delta_reader = IndexReader::open(delta_dir.path()).unwrap();
        let removed =
            std::collections::HashSet::from(["src/b.txt".to_string(), "src/c.txt".to_string()]);

        let merged_dir = tempfile::tempdir().unwrap();
        merge_index_with_delta(
            repo.path(),
            merged_dir.path(),
            &base_reader,
            &delta_reader,
            &removed,
            true,
        )
        .unwrap();

        let merged = IndexReader::open(merged_dir.path()).unwrap();
        merged.validate_lookup().unwrap();
        assert_eq!(merged.num_files(), 3);
        assert_eq!(merged.file_path(0), Some("src/a.txt"));
        assert_eq!(merged.file_path(1), Some("src/b.txt"));
        assert_eq!(merged.file_path(2), Some("src/d.txt"));
        assert!(
            merged.all_paths().iter().all(|path| path != "src/c.txt"),
            "deleted reader paths must not survive the merge"
        );

        let needle = crate::trigram::hash(b'n', b'e', b'e');
        let needle_paths: Vec<&str> = merged
            .lookup_trigram(needle)
            .iter()
            .filter_map(|&id| merged.file_path(id))
            .collect();
        assert_eq!(
            needle_paths,
            vec!["src/a.txt"],
            "the replacement must not retain the old reader posting"
        );
        let unique = crate::trigram::hash(b'u', b'n', b'i');
        let mut unique_paths: Vec<&str> = merged
            .lookup_trigram(unique)
            .iter()
            .filter_map(|&id| merged.file_path(id))
            .collect();
        unique_paths.sort_unstable();
        assert_eq!(unique_paths, vec!["src/b.txt", "src/d.txt"]);
    }

    #[test]
    fn streamed_delta_supports_deletion_without_new_files() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("keep.txt"), "keep needle\n").unwrap();
        std::fs::write(repo.path().join("delete.txt"), "delete needle\n").unwrap();

        let base_dir = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(base_dir.path()), false, false, &[]).unwrap();
        let base_reader = IndexReader::open(base_dir.path()).unwrap();

        let delta_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            build_index_for_files(repo.path(), delta_dir.path(), &[], 1)
                .unwrap()
                .indexed,
            0
        );
        let delta_reader = IndexReader::open(delta_dir.path()).unwrap();
        let removed = std::collections::HashSet::from(["delete.txt".to_string()]);

        let merged_dir = tempfile::tempdir().unwrap();
        merge_index_with_delta(
            repo.path(),
            merged_dir.path(),
            &base_reader,
            &delta_reader,
            &removed,
            true,
        )
        .unwrap();

        let merged = IndexReader::open(merged_dir.path()).unwrap();
        merged.validate_lookup().unwrap();
        assert_eq!(merged.num_files(), 1);
        assert_eq!(merged.file_path(0), Some("keep.txt"));
    }

    #[test]
    fn gitignore_gate_hint_fires_only_when_rules_are_silently_inert() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();

        // Non-git enlistment with a .gitignore: the rules are silently dropped,
        // which is the case that needs explaining.
        let opts = BuildOptions::default();
        assert!(gitignore_gate_hint(root, &opts).is_some());

        // Already opted out either way, so nothing is a surprise.
        assert!(
            gitignore_gate_hint(
                root,
                &BuildOptions {
                    no_require_git: true,
                    ..Default::default()
                }
            )
            .is_none()
        );
        assert!(
            gitignore_gate_hint(
                root,
                &BuildOptions {
                    no_ignore: true,
                    ..Default::default()
                }
            )
            .is_none()
        );

        // A real git repo applies the rules, so there is nothing to warn about.
        std::fs::create_dir(root.join(".git")).unwrap();
        assert!(gitignore_gate_hint(root, &opts).is_none());
    }

    #[test]
    fn gitignore_gate_hint_is_silent_without_a_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(gitignore_gate_hint(tmp.path(), &BuildOptions::default()).is_none());
    }
}
