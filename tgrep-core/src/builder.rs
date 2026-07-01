/// Index builder: walks a repo, extracts trigrams, writes the on-disk index.
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

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

#[derive(Clone, Copy)]
struct TrigramPosting {
    trigram: u32,
    entry: PostingEntry,
}

/// Build a trigram index for all text files under `root`.
pub fn build_index(
    root: &Path,
    index_dir: Option<&Path>,
    include_hidden: bool,
    exclude_dirs: &[String],
) -> Result<()> {
    let root = std::fs::canonicalize(root)?;
    let index_dir = match index_dir {
        Some(d) => d.to_path_buf(),
        None => root.join(INDEX_DIR_NAME),
    };
    std::fs::create_dir_all(&index_dir)?;

    eprintln!("Walking {}...", root.display());
    let walk = walker::walk_dir(
        &root,
        &walker::WalkOptions {
            include_hidden,
            exclude_dirs: exclude_dirs.to_vec(),
            ..Default::default()
        },
    );
    eprintln!(
        "Found {} text files ({} binary skipped, {} errors)",
        walk.files.len(),
        walk.skipped_binary,
        walk.skipped_error
    );

    // Read files and extract trigrams with masks in bounded parallel batches.
    // Binary content check is done here (not in walker) to avoid an extra
    // 8KB read per file — we're already reading the full file anyway.
    eprintln!("Extracting trigrams...");
    let binary_skipped = std::sync::atomic::AtomicUsize::new(0);

    // Assign file IDs and collect posting entries. Batching avoids
    // retaining every file's per-trigram HashMap at once for large repos.
    let mut file_id_map: Vec<(u32, String)> = Vec::with_capacity(walk.files.len());
    let mut postings: Vec<TrigramPosting> = Vec::new();

    for batch in walk.files.chunks(INDEX_BUILD_BATCH_SIZE) {
        let batch_data: Vec<(String, HashMap<u32, TrigramMasks>)> = batch
            .par_iter()
            .filter_map(|path| {
                let data = std::fs::read(path).ok()?;
                if trigram::is_binary(&data) {
                    binary_skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return None;
                }
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let per_tri = trigram::extract_merged_masks(&data);
                Some((rel, per_tri))
            })
            .collect();

        for (path, per_tri) in batch_data {
            let file_id = file_id_map.len() as u32;
            file_id_map.push((file_id, path));

            for (tri, masks) in per_tri {
                postings.push(TrigramPosting {
                    trigram: tri,
                    entry: PostingEntry {
                        file_id,
                        loc_mask: masks.loc_mask,
                        next_mask: masks.next_mask,
                    },
                });
            }
        }
    }

    let extra_binary = binary_skipped.into_inner();
    if extra_binary > 0 {
        eprintln!(
            "Skipped {} additional binary files (detected by content)",
            extra_binary
        );
    }

    write_index_v2_from_postings(&index_dir, &root, &file_id_map, &mut postings)?;

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
    Ok(())
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

    #[test]
    fn build_index_writes_readable_round_trip_index() {
        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "hello world\nneedle one\n").unwrap();
        std::fs::write(src.join("b.txt"), "needle two\nother content\n").unwrap();

        let index = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(index.path()), false, &[]).unwrap();

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
    fn append_overlay_merges_new_files_into_complete_index() {
        use crate::live::LiveIndex;

        // Base index with two files.
        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "hello world\nneedle one\n").unwrap();
        std::fs::write(src.join("b.txt"), "needle two\nother content\n").unwrap();

        let base_dir = tempfile::tempdir().unwrap();
        build_index(repo.path(), Some(base_dir.path()), false, &[]).unwrap();
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
}
