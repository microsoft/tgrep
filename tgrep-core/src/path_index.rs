//! Persisted paths that are listable by `--files` but absent from the content index.
//!
//! `files.bin` already stores every path that has searchable trigram postings.
//! This sidecar stores only the remaining admitted paths (binary files, binary
//! extensions, and files that could not be read while indexing), keeping both
//! disk and resident-memory overhead proportional to that usually small delta.

use std::io::{BufWriter, Write};
use std::path::Path;

use crate::{Error, Result};

pub const EXTRA_PATHS_FILENAME: &str = "files-extra.bin";

const MAGIC: &[u8; 8] = b"TGRPXP01";
const HEADER_LEN: usize = MAGIC.len() + size_of::<u64>();
const PATH_LEN_SIZE: usize = size_of::<u32>();

/// Write a complete filename-only path set.
pub fn write_extra_paths(index_dir: &Path, paths: &[String]) -> Result<()> {
    std::fs::create_dir_all(index_dir)?;
    let mut writer = BufWriter::new(std::fs::File::create(index_dir.join(EXTRA_PATHS_FILENAME))?);
    writer.write_all(MAGIC)?;
    writer.write_all(&(paths.len() as u64).to_le_bytes())?;

    for path in paths {
        let bytes = path.as_bytes();
        let len = u32::try_from(bytes.len()).map_err(|_| {
            Error::IndexCorrupted(format!(
                "path is too long for {EXTRA_PATHS_FILENAME} ({} bytes)",
                bytes.len()
            ))
        })?;
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(bytes)?;
    }
    writer.flush()?;
    Ok(())
}

/// Read the filename-only path set.
///
/// `Ok(None)` identifies a legacy index that predates the sidecar. Callers can
/// then preserve correctness by falling back to a filesystem walk.
pub fn read_extra_paths(index_dir: &Path) -> Result<Option<Vec<String>>> {
    let path = index_dir.join(EXTRA_PATHS_FILENAME);
    if !path.exists() {
        return Ok(None);
    }

    let data = std::fs::read(path)?;
    if data.len() < HEADER_LEN {
        return Err(corrupted(format!(
            "header is truncated ({} bytes, expected at least {HEADER_LEN})",
            data.len()
        )));
    }
    if &data[..MAGIC.len()] != MAGIC {
        return Err(corrupted("invalid header".to_string()));
    }

    let count = u64::from_le_bytes(data[MAGIC.len()..HEADER_LEN].try_into().unwrap());
    let max_records = (data.len() - HEADER_LEN) / PATH_LEN_SIZE;
    if count > max_records as u64 {
        return Err(corrupted(format!(
            "declares {count} paths but the file can contain at most {max_records}"
        )));
    }

    let mut paths = Vec::with_capacity(count as usize);
    let mut pos = HEADER_LEN;
    for index in 0..count {
        if pos + PATH_LEN_SIZE > data.len() {
            return Err(corrupted(format!("path {index} has a truncated length")));
        }
        let len = u32::from_le_bytes(data[pos..pos + PATH_LEN_SIZE].try_into().unwrap()) as usize;
        pos += PATH_LEN_SIZE;
        let end = pos
            .checked_add(len)
            .filter(|&end| end <= data.len())
            .ok_or_else(|| corrupted(format!("path {index} exceeds the file bounds")))?;
        let value = std::str::from_utf8(&data[pos..end])
            .map_err(|_| corrupted(format!("path {index} is not valid UTF-8")))?;
        paths.push(value.to_string());
        pos = end;
    }

    if pos != data.len() {
        return Err(corrupted(format!(
            "{} trailing bytes after the declared path set",
            data.len() - pos
        )));
    }
    Ok(Some(paths))
}

/// Remove a previous sidecar before rebuilding in place.
pub fn remove_extra_paths(index_dir: &Path) -> Result<()> {
    match std::fs::remove_file(index_dir.join(EXTRA_PATHS_FILENAME)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn corrupted(message: String) -> Error {
    Error::IndexCorrupted(format!("{EXTRA_PATHS_FILENAME}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sidecar_identifies_a_legacy_index() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_extra_paths(dir.path()).unwrap().is_none());
    }

    #[test]
    fn round_trips_empty_and_nonempty_path_sets() {
        let dir = tempfile::tempdir().unwrap();
        write_extra_paths(dir.path(), &[]).unwrap();
        assert_eq!(read_extra_paths(dir.path()).unwrap(), Some(Vec::new()));

        let paths = vec![
            "assets/image.bin".to_string(),
            "name\nwith-newline.dat".to_string(),
        ];
        write_extra_paths(dir.path(), &paths).unwrap();
        assert_eq!(read_extra_paths(dir.path()).unwrap(), Some(paths));
    }

    #[test]
    fn rejects_truncated_and_trailing_data() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(EXTRA_PATHS_FILENAME), MAGIC).unwrap();
        assert!(read_extra_paths(dir.path()).is_err());

        write_extra_paths(dir.path(), &["one.bin".to_string()]).unwrap();
        let path = dir.path().join(EXTRA_PATHS_FILENAME);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0);
        std::fs::write(path, bytes).unwrap();
        assert!(read_extra_paths(dir.path()).is_err());
    }
}
