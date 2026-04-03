/// Index builder: walks a repo, extracts trigrams, writes the on-disk index.
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use crate::Result;
use crate::meta::IndexMeta;
use crate::ondisk::{self, LookupEntry};
use crate::trigram;
use crate::walker;

const INDEX_DIR_NAME: &str = ".tgrep";

/// Build a trigram index for all text files under `root`.
pub fn build_index(root: &Path, index_dir: Option<&Path>, include_hidden: bool) -> Result<()> {
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
            ..Default::default()
        },
    );
    eprintln!(
        "Found {} text files ({} binary skipped, {} errors)",
        walk.files.len(),
        walk.skipped_binary,
        walk.skipped_error
    );

    // Read all files and extract trigrams in parallel
    eprintln!("Extracting trigrams...");
    let file_data: Vec<(String, Vec<u32>)> = walk
        .files
        .par_iter()
        .filter_map(|path| {
            let data = std::fs::read(path).ok()?;
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            // Extract trigrams from both original and lowercased content
            // so case-insensitive queries can use the index effectively.
            let mut trigrams = trigram::extract(&data);
            let lower = data.to_ascii_lowercase();
            if lower != data {
                let lower_tris = trigram::extract(&lower);
                trigrams.extend(lower_tris);
            }
            Some((rel, trigrams))
        })
        .collect();

    // Assign file IDs and build inverted index
    let mut file_id_map: Vec<(u32, String)> = Vec::with_capacity(file_data.len());
    let mut inverted: HashMap<u32, Vec<u32>> = HashMap::new();

    for (id, (path, trigrams)) in file_data.iter().enumerate() {
        let file_id = id as u32;
        file_id_map.push((file_id, path.clone()));
        for &tri in trigrams {
            inverted.entry(tri).or_default().push(file_id);
        }
    }

    // Sort trigrams for binary-searchable lookup table
    let mut sorted_trigrams: Vec<u32> = inverted.keys().copied().collect();
    sorted_trigrams.sort_unstable();

    // Write index.bin and lookup.bin
    eprintln!(
        "Writing index ({} trigrams, {} files)...",
        sorted_trigrams.len(),
        file_id_map.len()
    );

    let mut postings_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("index.bin"))?);
    let mut lookup_entries: Vec<LookupEntry> = Vec::with_capacity(sorted_trigrams.len());
    let mut offset: u64 = 0;

    for &tri in &sorted_trigrams {
        let posting_list = inverted.get(&tri).unwrap();
        let length = posting_list.len() as u32;

        lookup_entries.push(LookupEntry {
            trigram: tri,
            offset,
            length,
        });

        for &fid in posting_list {
            postings_file.write_all(&fid.to_le_bytes())?;
        }
        offset += length as u64 * ondisk::POSTING_ENTRY_SIZE as u64;
    }
    postings_file.flush()?;

    // Write lookup.bin
    let mut lookup_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("lookup.bin"))?);
    for entry in &lookup_entries {
        lookup_file.write_all(&entry.encode())?;
    }
    lookup_file.flush()?;

    // Write files.bin
    let mut files_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("files.bin"))?);
    for (id, path) in &file_id_map {
        files_file.write_all(&ondisk::encode_file_entry(*id, path))?;
    }
    files_file.flush()?;

    // Write meta.json
    let meta = IndexMeta::new(
        &root.to_string_lossy(),
        file_id_map.len() as u64,
        sorted_trigrams.len() as u64,
    );
    meta.save(&index_dir)?;

    eprintln!("Index built successfully at {}", index_dir.display());
    Ok(())
}

/// Return the default index directory for a given repo root.
pub fn default_index_dir(root: &Path) -> std::path::PathBuf {
    root.join(INDEX_DIR_NAME)
}

/// Write the on-disk index from a pre-computed snapshot (paths + inverted index).
/// This allows the caller to snapshot under a brief lock, then write without holding it.
pub fn write_index_from_snapshot(
    root: &Path,
    index_dir: &Path,
    paths: &[String],
    inverted: &HashMap<u32, Vec<u32>>,
    complete: bool,
) -> Result<()> {
    std::fs::create_dir_all(index_dir)?;

    let mut sorted_trigrams: Vec<u32> = inverted.keys().copied().collect();
    sorted_trigrams.sort_unstable();

    // Write index.bin
    let mut postings_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("index.bin"))?);
    let mut lookup_entries: Vec<LookupEntry> = Vec::with_capacity(sorted_trigrams.len());
    let mut offset: u64 = 0;

    for &tri in &sorted_trigrams {
        let posting_list = inverted.get(&tri).unwrap();
        let length = posting_list.len() as u32;

        lookup_entries.push(LookupEntry {
            trigram: tri,
            offset,
            length,
        });

        for &fid in posting_list {
            postings_file.write_all(&fid.to_le_bytes())?;
        }
        offset += length as u64 * ondisk::POSTING_ENTRY_SIZE as u64;
    }
    postings_file.flush()?;

    // Write lookup.bin
    let mut lookup_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("lookup.bin"))?);
    for entry in &lookup_entries {
        lookup_file.write_all(&entry.encode())?;
    }
    lookup_file.flush()?;

    // Write files.bin
    let mut files_file =
        std::io::BufWriter::new(std::fs::File::create(index_dir.join("files.bin"))?);
    for (id, path) in paths.iter().enumerate() {
        files_file.write_all(&ondisk::encode_file_entry(id as u32, path))?;
    }
    files_file.flush()?;

    // Write meta.json
    let root = std::fs::canonicalize(root)?;
    let mut meta = IndexMeta::new(
        &root.to_string_lossy(),
        paths.len() as u64,
        sorted_trigrams.len() as u64,
    );
    meta.complete = complete;
    meta.save(index_dir)?;

    Ok(())
}
