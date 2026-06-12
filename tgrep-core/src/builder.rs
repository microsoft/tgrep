/// Index builder: walks a repo, extracts trigrams, writes the on-disk index.
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use crate::Result;
use crate::meta::{self, IndexMeta};
use crate::ondisk::{self, LookupEntry, PostingEntry};
use crate::trigram::{self, TrigramMasks};
use crate::walker;

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
        let batch_data: Vec<(String, HashMap<u32, TrigramMasks>)> =
            crate::parallel::install(|| {
                batch
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
                    .collect()
            });

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
}
