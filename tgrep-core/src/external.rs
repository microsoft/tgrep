//! External merge sort for bootstrap index construction.
//!
//! The default in-memory builder accumulates one [`TrigramPosting`] (12 bytes)
//! for every (trigram, file) pair in the repository, then sorts the whole thing
//! at once. That vector is unbounded: a monorepo with ~500K files averaging a
//! few thousand distinct trigrams each needs well over 10 GB of heap before the
//! first byte is written.
//!
//! This module bounds peak heap to a fixed arena instead:
//!
//! 1. Postings accumulate in an arena sized by a byte budget.
//! 2. When the arena fills it is sorted, delta+varint encoded, and spilled to a
//!    segment file on disk; the arena's allocation is then reused.
//! 3. [`ExternalSorter::write_postings`] streams a k-way merge of the segments
//!    straight into `index.bin` / `lookup.bin`.
//!
//! Peak heap is `max(arena, merge read buffers) + largest posting list`,
//! independent of repository size. The merge shares one read-ahead budget
//! across all open segments rather than giving each a fixed buffer, so peak
//! stays tied to the caller's budget instead of growing with fan-in.
//!
//! Total sort work is unchanged: `k` sorts of `n/k` elements plus an `n·log k`
//! merge is still `O(n·log n)`, with better cache locality per sort. The added
//! cost is one spill write plus one merge read, and the segment encoding
//! roughly halves those bytes relative to the 6-byte on-disk posting layout.
//!
//! If the arena never fills — small and mid-size repositories — nothing is
//! spilled and `write_postings` degrades to exactly the in-memory path: one
//! sort followed by one streaming write.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::ondisk::{self, LookupEntry, PostingEntry};
use crate::{Error, Result};

/// A single (trigram, posting) pair prior to grouping.
#[derive(Clone, Copy)]
pub(crate) struct TrigramPosting {
    pub trigram: u32,
    pub entry: PostingEntry,
}

/// Default arena budget before spilling. Chosen so a spill segment is large
/// enough to amortize sequential write cost while keeping the merge fan-in
/// modest even for very large repositories.
pub const DEFAULT_BUFFER_BYTES: usize = 64 * 1024 * 1024;

/// Read-ahead buffer held per open segment during the merge, when fan-in is
/// low enough to afford it.
const SEGMENT_BUFFER_BYTES: usize = 256 * 1024;

/// Floor on a per-segment read buffer. Only `MAX_VARINT_LEN` bytes are ever
/// required for correctness; this is purely to keep the read syscall count
/// reasonable at high fan-in.
const MIN_SEGMENT_BUFFER_BYTES: usize = 4 * 1024;

/// Longest possible LEB128 encoding of a `u64`.
const MAX_VARINT_LEN: usize = 10;

const POSTING_WRITE_CHUNK_ENTRIES: usize = 8192;
const LOOKUP_WRITE_CHUNK_ENTRIES: usize = 4096;

/// Flush threshold for the encode scratch buffer while spilling a segment.
const SPILL_SCRATCH_FLUSH_BYTES: usize = 128 * 1024;

fn write_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Temporary directory holding spill segments, removed on drop.
///
/// Segments are written next to the index rather than under the system temp
/// directory: they can total gigabytes for a large repository, and `TEMP` is
/// frequently on a smaller (or differently quota'd) volume than the workspace.
struct SpillDir {
    path: PathBuf,
}

impl SpillDir {
    fn create(index_dir: &Path) -> Result<Self> {
        let path = index_dir.join(format!("spill-{}.tmp", std::process::id()));
        // A leftover directory from a killed build would make stale segments
        // visible to this run's merge, silently corrupting the index.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Accumulates postings, spilling sorted segments to disk when the arena fills.
pub(crate) struct ExternalSorter {
    arena: Vec<TrigramPosting>,
    /// Arena length that triggers a spill.
    capacity: usize,
    /// Caller's byte budget. Bounds the arena while accumulating, then the
    /// shared segment read-ahead while merging.
    budget_bytes: usize,
    spill: Option<SpillDir>,
    segments: Vec<PathBuf>,
    index_dir: PathBuf,
    scratch: Vec<u8>,
}

impl ExternalSorter {
    /// Create a sorter that keeps at most `budget_bytes` of postings in heap.
    pub(crate) fn new(index_dir: &Path, budget_bytes: usize) -> Self {
        let entry_size = std::mem::size_of::<TrigramPosting>();
        // Always allow at least a small arena so a pathological budget can't
        // produce a zero-capacity arena that spills on every single posting.
        let capacity = (budget_bytes / entry_size).max(1024);
        Self {
            // Allocate the arena once, exactly. Letting `Vec` grow it would
            // overshoot the budget: for the 64 MB default it doubles past the
            // target and reserves 96 MB, half again the bound this type exists
            // to enforce. Reserving up front also avoids repeatedly copying a
            // multi-megabyte buffer while filling.
            arena: Vec::with_capacity(capacity),
            capacity,
            budget_bytes,
            spill: None,
            segments: Vec::new(),
            index_dir: index_dir.to_path_buf(),
            scratch: Vec::new(),
        }
    }

    /// Number of segments spilled so far, excluding the arena tail that
    /// `write_postings` spills last. Callers that want the merged total should
    /// use the count returned by `write_postings`.
    #[cfg(test)]
    pub(crate) fn spilled_segments(&self) -> usize {
        self.segments.len()
    }

    /// Append one posting, spilling first if the arena is full.
    pub(crate) fn push(&mut self, posting: TrigramPosting) -> Result<()> {
        if self.arena.len() >= self.capacity {
            self.spill_arena()?;
        }
        self.arena.push(posting);
        Ok(())
    }

    /// Append every posting a file contributed.
    pub(crate) fn push_file<I>(&mut self, file_id: u32, per_trigram: I) -> Result<()>
    where
        I: IntoIterator<Item = (u32, crate::trigram::TrigramMasks)>,
    {
        for (trigram, masks) in per_trigram {
            self.push(TrigramPosting {
                trigram,
                entry: PostingEntry {
                    file_id,
                    loc_mask: masks.loc_mask,
                    next_mask: masks.next_mask,
                },
            })?;
        }
        Ok(())
    }

    fn sort_arena(&mut self) {
        self.arena.sort_unstable_by(|a, b| {
            a.trigram
                .cmp(&b.trigram)
                .then_with(|| a.entry.file_id.cmp(&b.entry.file_id))
        });
    }

    /// Sort the arena and write it out as a compact segment, then clear it.
    ///
    /// The arena's backing allocation is deliberately retained (`clear`, not
    /// `drop`) so steady-state indexing performs no further large allocations.
    fn spill_arena(&mut self) -> Result<()> {
        if self.arena.is_empty() {
            return Ok(());
        }
        self.sort_arena();

        let spill = match &self.spill {
            Some(dir) => dir,
            None => {
                self.spill = Some(SpillDir::create(&self.index_dir)?);
                self.spill.as_ref().unwrap()
            }
        };
        let path = spill
            .path
            .join(format!("seg-{:05}.bin", self.segments.len()));
        let mut writer = BufWriter::with_capacity(SEGMENT_BUFFER_BYTES, File::create(&path)?);

        // Segment layout, groups ordered by ascending trigram:
        //   varint(trigram - prev_trigram)
        //   varint(posting_count)
        //   posting_count x { varint(file_id - prev_file_id), loc_mask, next_mask }
        // Both deltas are non-negative because the arena is sorted by
        // (trigram, file_id), which makes the encoding substantially smaller
        // than the 6-byte fixed on-disk posting record.
        let scratch = &mut self.scratch;
        scratch.clear();
        let mut prev_trigram: u32 = 0;
        let mut idx = 0usize;
        while idx < self.arena.len() {
            let trigram = self.arena[idx].trigram;
            let mut end = idx + 1;
            while end < self.arena.len() && self.arena[end].trigram == trigram {
                end += 1;
            }

            write_varint(scratch, (trigram - prev_trigram) as u64);
            write_varint(scratch, (end - idx) as u64);
            prev_trigram = trigram;

            let mut prev_file_id: u32 = 0;
            for posting in &self.arena[idx..end] {
                let entry = posting.entry;
                write_varint(scratch, (entry.file_id - prev_file_id) as u64);
                scratch.push(entry.loc_mask);
                scratch.push(entry.next_mask);
                prev_file_id = entry.file_id;
            }

            if scratch.len() >= SPILL_SCRATCH_FLUSH_BYTES {
                writer.write_all(scratch)?;
                scratch.clear();
            }
            idx = end;
        }
        if !scratch.is_empty() {
            writer.write_all(scratch)?;
            scratch.clear();
        }
        writer.flush()?;
        // Release the encode buffer's capacity — it is only needed during a
        // spill and holding it would count against the caller's budget.
        self.scratch = Vec::new();

        self.segments.push(path);
        self.arena.clear();
        Ok(())
    }

    /// Write `index.bin` and `lookup.bin`.
    ///
    /// Returns `(distinct trigram count, spill segments merged)`. Consumes the
    /// sorter so the arena and every spill segment are released before the
    /// caller proceeds.
    pub(crate) fn write_postings(mut self, index_dir: &Path) -> Result<(usize, usize)> {
        if self.segments.is_empty() {
            // Never exceeded the budget: identical to the in-memory path.
            self.sort_arena();
            let arena = std::mem::take(&mut self.arena);
            return Ok((write_sorted_arena(index_dir, &arena)?, 0));
        }

        // Fold the tail into a final segment so the merge has a single
        // uniform input kind.
        self.spill_arena()?;
        self.arena = Vec::new();

        let segments = self.segments.len();
        eprintln!("Merging {segments} spill segment(s) into the index...");
        Ok((
            merge_segments(index_dir, &self.segments, self.budget_bytes)?,
            segments,
        ))
    }
}

/// Shared writer for `index.bin` + `lookup.bin`.
struct IndexWriter {
    postings: BufWriter<File>,
    lookup: BufWriter<File>,
    posting_scratch: Vec<u8>,
    lookup_scratch: Vec<u8>,
    offset: u64,
    trigram_count: usize,
}

impl IndexWriter {
    fn create(index_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(index_dir)?;
        Ok(Self {
            postings: BufWriter::new(File::create(index_dir.join("index.bin"))?),
            lookup: BufWriter::new(File::create(index_dir.join("lookup.bin"))?),
            posting_scratch: Vec::with_capacity(
                POSTING_WRITE_CHUNK_ENTRIES * ondisk::POSTING_ENTRY_SIZE,
            ),
            lookup_scratch: Vec::with_capacity(
                LOOKUP_WRITE_CHUNK_ENTRIES * ondisk::LOOKUP_ENTRY_SIZE,
            ),
            offset: 0,
            trigram_count: 0,
        })
    }

    /// Write one trigram's complete, file-id-sorted posting list.
    fn write_group(&mut self, trigram: u32, entries: &[PostingEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let length = u32::try_from(entries.len()).map_err(|_| {
            Error::IndexCorrupted(format!(
                "posting list for trigram {trigram} exceeds the u32 length limit"
            ))
        })?;

        if self.lookup_scratch.len() == self.lookup_scratch.capacity() {
            self.lookup.write_all(&self.lookup_scratch)?;
            self.lookup_scratch.clear();
        }
        let lookup_entry = LookupEntry {
            trigram,
            offset: self.offset,
            length,
        };
        self.lookup_scratch
            .extend_from_slice(&lookup_entry.trigram.to_le_bytes());
        self.lookup_scratch
            .extend_from_slice(&lookup_entry.offset.to_le_bytes());
        self.lookup_scratch
            .extend_from_slice(&lookup_entry.length.to_le_bytes());

        for chunk in entries.chunks(POSTING_WRITE_CHUNK_ENTRIES) {
            self.posting_scratch.clear();
            for entry in chunk {
                self.posting_scratch
                    .extend_from_slice(&entry.file_id.to_le_bytes());
                self.posting_scratch.push(entry.loc_mask);
                self.posting_scratch.push(entry.next_mask);
            }
            self.postings.write_all(&self.posting_scratch)?;
        }

        self.offset += length as u64 * ondisk::POSTING_ENTRY_SIZE as u64;
        self.trigram_count += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<usize> {
        if !self.lookup_scratch.is_empty() {
            self.lookup.write_all(&self.lookup_scratch)?;
            self.lookup_scratch.clear();
        }
        self.postings.flush()?;
        self.lookup.flush()?;
        Ok(self.trigram_count)
    }
}

/// Write an already-sorted arena directly, without any spill round trip.
fn write_sorted_arena(index_dir: &Path, arena: &[TrigramPosting]) -> Result<usize> {
    let mut writer = IndexWriter::create(index_dir)?;
    let mut group: Vec<PostingEntry> = Vec::new();
    let mut idx = 0usize;
    while idx < arena.len() {
        let trigram = arena[idx].trigram;
        let mut end = idx + 1;
        while end < arena.len() && arena[end].trigram == trigram {
            end += 1;
        }
        group.clear();
        group.extend(arena[idx..end].iter().map(|p| p.entry));
        writer.write_group(trigram, &group)?;
        idx = end;
    }
    writer.finish()
}

/// Buffered byte reader with a sliding window, used to decode segments.
struct ByteSource {
    file: File,
    buf: Vec<u8>,
    pos: usize,
    filled: usize,
}

impl ByteSource {
    fn open(path: &Path, capacity: usize) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            buf: vec![0u8; capacity.max(MAX_VARINT_LEN)],
            pos: 0,
            filled: 0,
        })
    }

    /// Make at least `want` bytes available if the file still has them.
    /// Returns the number actually available, which is short only at EOF.
    fn ensure(&mut self, want: usize) -> Result<usize> {
        debug_assert!(want <= self.buf.len());
        if self.filled - self.pos >= want {
            return Ok(self.filled - self.pos);
        }
        self.buf.copy_within(self.pos..self.filled, 0);
        self.filled -= self.pos;
        self.pos = 0;
        while self.filled < want {
            let read = self.file.read(&mut self.buf[self.filled..])?;
            if read == 0 {
                break;
            }
            self.filled += read;
        }
        Ok(self.filled)
    }

    /// Decode one varint. `Ok(None)` means a clean end of segment.
    fn read_varint(&mut self) -> Result<Option<u64>> {
        if self.ensure(MAX_VARINT_LEN)? == 0 {
            return Ok(None);
        }
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            if self.pos >= self.filled {
                return Err(Error::IndexCorrupted(
                    "spill segment truncated mid-varint".into(),
                ));
            }
            let byte = self.buf[self.pos];
            self.pos += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(Some(value));
            }
            shift += 7;
            if shift >= 64 {
                return Err(Error::IndexCorrupted(
                    "spill segment contains an overlong varint".into(),
                ));
            }
        }
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.ensure(1)? == 0 {
            return Err(Error::IndexCorrupted(
                "spill segment truncated mid-posting".into(),
            ));
        }
        let byte = self.buf[self.pos];
        self.pos += 1;
        Ok(byte)
    }
}

/// Streaming cursor over one spill segment.
struct SegmentCursor {
    src: ByteSource,
    prev_trigram: u32,
    /// Postings remaining in the group whose header has been read.
    pending_count: usize,
}

impl SegmentCursor {
    fn open(path: &Path, buffer_bytes: usize) -> Result<Self> {
        Ok(Self {
            src: ByteSource::open(path, buffer_bytes)?,
            prev_trigram: 0,
            pending_count: 0,
        })
    }

    /// Read the next group header. `Ok(None)` means the segment is exhausted.
    fn advance(&mut self) -> Result<Option<u32>> {
        let Some(delta) = self.src.read_varint()? else {
            return Ok(None);
        };
        let trigram = u32::try_from(u64::from(self.prev_trigram) + delta)
            .map_err(|_| Error::IndexCorrupted("spill segment trigram delta overflow".into()))?;
        let count = self
            .src
            .read_varint()?
            .ok_or_else(|| Error::IndexCorrupted("spill segment missing group count".into()))?;
        self.prev_trigram = trigram;
        self.pending_count = usize::try_from(count)
            .map_err(|_| Error::IndexCorrupted("spill segment group count overflow".into()))?;
        Ok(Some(trigram))
    }

    /// Decode the pending group's postings, appending them to `out`.
    fn take_group(&mut self, out: &mut Vec<PostingEntry>) -> Result<()> {
        let mut prev_file_id: u32 = 0;
        for _ in 0..self.pending_count {
            let delta = self
                .src
                .read_varint()?
                .ok_or_else(|| Error::IndexCorrupted("spill segment truncated in group".into()))?;
            let file_id = u32::try_from(u64::from(prev_file_id) + delta)
                .map_err(|_| Error::IndexCorrupted("spill segment file id overflow".into()))?;
            let loc_mask = self.src.read_u8()?;
            let next_mask = self.src.read_u8()?;
            out.push(PostingEntry {
                file_id,
                loc_mask,
                next_mask,
            });
            prev_file_id = file_id;
        }
        self.pending_count = 0;
        Ok(())
    }
}

/// Per-segment read-ahead when `read_budget_bytes` is shared across `segments`
/// open cursors.
///
/// Above the floor this keeps total merge memory at roughly the caller's
/// budget regardless of fan-in. Below it — a tiny budget against a very large
/// repository — total degrades to `segments * MIN_SEGMENT_BUFFER_BYTES`, still
/// 64x below a fixed 256 KiB buffer per segment. Bounding that last case
/// absolutely would need a multi-pass merge; at realistic budgets the floor is
/// never reached.
fn segment_buffer_bytes(read_budget_bytes: usize, segments: usize) -> usize {
    (read_budget_bytes / segments.max(1)).clamp(MIN_SEGMENT_BUFFER_BYTES, SEGMENT_BUFFER_BYTES)
}

/// k-way merge of sorted segments into `index.bin` / `lookup.bin`.
///
/// `read_budget_bytes` is shared across every open segment. A fixed per-segment
/// buffer would make merge memory grow with fan-in, which is backwards: a
/// smaller arena spills more segments, so it would make a *tighter* budget cost
/// *more* peak memory. Splitting one budget keeps peak tied to what was asked
/// for. The arena has already been released by this point, so the full budget
/// is available.
fn merge_segments(
    index_dir: &Path,
    segments: &[PathBuf],
    read_budget_bytes: usize,
) -> Result<usize> {
    let per_segment = segment_buffer_bytes(read_budget_bytes, segments.len());
    let mut cursors: Vec<SegmentCursor> = Vec::with_capacity(segments.len());
    // Min-heap keyed by (trigram, segment index). Ordering by segment index as
    // the tiebreak matters: file IDs are assigned in walk order and segments
    // are spilled in that same order, so visiting a trigram's groups in
    // ascending segment order yields globally ascending file IDs and the
    // concatenation below needs no re-sort.
    let mut heap: BinaryHeap<Reverse<(u32, usize)>> = BinaryHeap::with_capacity(segments.len());

    for (idx, path) in segments.iter().enumerate() {
        let mut cursor = SegmentCursor::open(path, per_segment)?;
        if let Some(trigram) = cursor.advance()? {
            heap.push(Reverse((trigram, idx)));
        }
        cursors.push(cursor);
    }

    let mut writer = IndexWriter::create(index_dir)?;
    let mut group: Vec<PostingEntry> = Vec::new();

    while let Some(&Reverse((trigram, _))) = heap.peek() {
        group.clear();
        let mut needs_sort = false;
        let mut last_file_id: Option<u32> = None;

        while let Some(&Reverse((next_trigram, idx))) = heap.peek() {
            if next_trigram != trigram {
                break;
            }
            heap.pop();

            let before = group.len();
            cursors[idx].take_group(&mut group)?;
            // Defensive: if the append-in-segment-order invariant is ever
            // broken (e.g. a caller assigns file IDs out of walk order), fall
            // back to sorting rather than emitting an unsorted posting list,
            // which query execution assumes is ascending.
            if let Some(last) = last_file_id
                && group.get(before).is_some_and(|first| first.file_id <= last)
            {
                needs_sort = true;
            }
            last_file_id = group.last().map(|entry| entry.file_id);

            if let Some(next) = cursors[idx].advance()? {
                heap.push(Reverse((next, idx)));
            }
        }

        if needs_sort {
            group.sort_unstable_by_key(|entry| entry.file_id);
        }
        writer.write_group(trigram, &group)?;
    }

    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::IndexReader;

    fn posting(trigram: u32, file_id: u32) -> TrigramPosting {
        TrigramPosting {
            trigram,
            entry: PostingEntry {
                file_id,
                loc_mask: (file_id as u8) | 1,
                next_mask: (trigram as u8) | 1,
            },
        }
    }

    // `Vec`'s own doubling would reserve up to 2x the budget (96 MB for the
    // 64 MB default), so the arena could hold half again as much as the caller
    // asked for. The allocation must stay inside the budget, both on the first
    // fill and after a spill reuses the buffer.
    #[test]
    fn arena_allocation_never_exceeds_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        // 64 KB budget: large enough to exercise several fills, small enough
        // that the entry capacity is not clamped by the 1024-entry floor.
        let budget = 64 * 1024;
        let mut sorter = ExternalSorter::new(dir.path(), budget);
        let capacity = sorter.capacity;
        assert!(
            capacity > 1024,
            "budget should set capacity, not the floor: {capacity}"
        );
        assert_eq!(
            sorter.arena.capacity(),
            capacity,
            "arena should be reserved exactly once, up front"
        );

        // Push well past one full arena so growth and post-spill reuse are
        // both covered.
        for i in 0..(capacity as u32 * 3) {
            sorter.push(posting(i % 977, i)).unwrap();
            assert!(
                sorter.arena.capacity() <= capacity,
                "arena capacity {} exceeded budget capacity {capacity}",
                sorter.arena.capacity()
            );
        }
        assert!(sorter.spilled_segments() > 0, "expected at least one spill");
    }

    /// Build both ways and assert the resulting index files are byte-identical.
    fn assert_paths_match(postings: &[TrigramPosting], budget_bytes: usize) {
        let in_memory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();

        let mut direct: Vec<TrigramPosting> = postings.to_vec();
        direct.sort_unstable_by(|a, b| {
            a.trigram
                .cmp(&b.trigram)
                .then_with(|| a.entry.file_id.cmp(&b.entry.file_id))
        });
        let expected_trigrams = write_sorted_arena(in_memory.path(), &direct).unwrap();

        let mut sorter = ExternalSorter::new(external.path(), budget_bytes);
        for posting in postings {
            sorter.push(*posting).unwrap();
        }
        let spilled = sorter.spilled_segments();
        let (actual_trigrams, merged_segments) = sorter.write_postings(external.path()).unwrap();

        assert_eq!(expected_trigrams, actual_trigrams, "trigram count mismatch");
        assert!(
            merged_segments == 0 || merged_segments > spilled,
            "the arena tail should be folded into a final segment"
        );
        for name in ["index.bin", "lookup.bin"] {
            let a = std::fs::read(in_memory.path().join(name)).unwrap();
            let b = std::fs::read(external.path().join(name)).unwrap();
            assert_eq!(a, b, "{name} differs (spilled {spilled} segment(s))");
        }
    }

    #[test]
    fn external_matches_in_memory_without_spilling() {
        let postings: Vec<TrigramPosting> = (0..64u32)
            .flat_map(|file_id| (0..32u32).map(move |t| posting(t * 7 + 1, file_id)))
            .collect();
        assert_paths_match(&postings, DEFAULT_BUFFER_BYTES);
    }

    #[test]
    fn external_matches_in_memory_across_many_spills() {
        // A tiny budget still clamps to a 1024-entry arena, so use enough
        // postings to force a double-digit number of segments.
        let postings: Vec<TrigramPosting> = (0..400u32)
            .flat_map(|file_id| (0..64u32).map(move |t| posting(t * 3, file_id)))
            .collect();
        let sorter = ExternalSorter::new(Path::new("."), 1);
        assert_eq!(sorter.capacity, 1024, "budget should clamp to a floor");

        assert_paths_match(&postings, 1);
    }

    #[test]
    fn spilling_actually_happens_under_a_small_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = ExternalSorter::new(dir.path(), 1);
        for file_id in 0..50u32 {
            for t in 0..500u32 {
                sorter.push(posting(t, file_id)).unwrap();
            }
        }
        assert!(
            sorter.spilled_segments() >= 20,
            "expected many segments, got {}",
            sorter.spilled_segments()
        );
        sorter.write_postings(dir.path()).unwrap();
    }

    #[test]
    fn spill_directory_is_removed_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = ExternalSorter::new(dir.path(), 1);
        for file_id in 0..20u32 {
            for t in 0..500u32 {
                sorter.push(posting(t, file_id)).unwrap();
            }
        }
        assert!(sorter.spilled_segments() > 0);
        sorter.write_postings(dir.path()).unwrap();

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("spill-"))
            .collect();
        assert!(leftover.is_empty(), "spill directory should be cleaned up");
    }

    #[test]
    fn merged_posting_lists_are_readable_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = ExternalSorter::new(dir.path(), 1);
        // Interleave so each trigram's postings land across many segments.
        for file_id in 0..300u32 {
            for t in 0..40u32 {
                sorter.push(posting(t, file_id)).unwrap();
            }
        }
        assert!(sorter.spilled_segments() > 1);
        let (trigram_count, _) = sorter.write_postings(dir.path()).unwrap();
        assert_eq!(trigram_count, 40);

        // files.bin/meta.json are written by the builder; synthesize the
        // minimum the reader needs to resolve posting lists.
        let mut files = Vec::new();
        for id in 0..300u32 {
            ondisk::write_file_entry(&mut files, id, &format!("f{id}.txt")).unwrap();
        }
        std::fs::write(dir.path().join("files.bin"), files).unwrap();
        crate::meta::IndexMeta::new("/tmp/root", 300, trigram_count as u64)
            .save(dir.path())
            .unwrap();

        let reader = IndexReader::open(dir.path()).unwrap();
        for t in 0..40u32 {
            let entries = reader.lookup_trigram_with_masks(t);
            assert_eq!(entries.len(), 300, "trigram {t} should list every file");
            let ids: Vec<u32> = entries.iter().map(|e| e.file_id).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            assert_eq!(ids, sorted, "posting list for {t} must be file-id sorted");
            for entry in &entries {
                assert_eq!(entry.loc_mask, (entry.file_id as u8) | 1);
                assert_eq!(entry.next_mask, (t as u8) | 1);
            }
        }
    }

    #[test]
    fn empty_input_writes_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let sorter = ExternalSorter::new(dir.path(), DEFAULT_BUFFER_BYTES);
        assert_eq!(sorter.write_postings(dir.path()).unwrap(), (0, 0));
        assert_eq!(
            std::fs::read(dir.path().join("index.bin")).unwrap().len(),
            0
        );
        assert_eq!(
            std::fs::read(dir.path().join("lookup.bin")).unwrap().len(),
            0
        );
    }

    #[test]
    fn varint_roundtrip_covers_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.bin");
        let values = [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64];
        let mut buf = Vec::new();
        for &v in &values {
            write_varint(&mut buf, v);
        }
        std::fs::write(&path, &buf).unwrap();

        let mut src = ByteSource::open(&path, SEGMENT_BUFFER_BYTES).unwrap();
        for &v in &values {
            assert_eq!(src.read_varint().unwrap(), Some(v));
        }
        assert_eq!(src.read_varint().unwrap(), None);
    }

    /// A tiny buffer must still decode correctly: `ensure` compacts its sliding
    /// window, so the only hard requirement is room for one varint.
    #[test]
    fn varint_roundtrip_survives_a_minimal_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.bin");
        let values = [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64];
        let mut buf = Vec::new();
        for &v in &values {
            write_varint(&mut buf, v);
        }
        std::fs::write(&path, &buf).unwrap();

        let mut src = ByteSource::open(&path, 1).unwrap();
        for &v in &values {
            assert_eq!(src.read_varint().unwrap(), Some(v));
        }
        assert_eq!(src.read_varint().unwrap(), None);
    }

    /// Merge read-ahead is a shared budget, not a per-segment allocation.
    /// Without this, shrinking the arena raises segment count and drives peak
    /// memory *up*, defeating the point of a tighter budget. Measured on the
    /// Linux kernel: a 1 MiB arena spilled 1946 segments, and fixed 256 KiB
    /// buffers put 486 MiB of read-ahead on the heap.
    #[test]
    fn merge_read_ahead_is_bounded_by_the_budget_not_by_fan_in() {
        let budget = 64 * 1024 * 1024;

        // Low fan-in: each segment can afford the full read-ahead.
        assert_eq!(segment_buffer_bytes(budget, 31), SEGMENT_BUFFER_BYTES);

        // High fan-in: buffers shrink so the total tracks the budget.
        for segments in [64usize, 256, 1024, 4096] {
            let total = segment_buffer_bytes(budget, segments) * segments;
            assert!(
                total <= budget,
                "{segments} segments used {total} bytes against a {budget} budget"
            );
        }

        // Past the floor the total grows again, but 64x slower than a fixed
        // per-segment buffer would. This is the Linux 1 MiB case.
        let segments = 1946;
        let total = segment_buffer_bytes(1024 * 1024, segments) * segments;
        assert_eq!(
            segment_buffer_bytes(1024 * 1024, segments),
            MIN_SEGMENT_BUFFER_BYTES
        );
        assert!(
            total < 8 * 1024 * 1024,
            "floor regime should stay small, got {total}"
        );
    }

    /// The merge must stay correct when read buffers are squeezed to the floor
    /// across many segments.
    #[test]
    fn merge_is_correct_with_minimal_read_buffers() {
        let dir = tempfile::tempdir().unwrap();
        // Budget of 1 byte floors the arena at 1024 entries, forcing many spills.
        let mut sorter = ExternalSorter::new(dir.path(), 1);
        for trigram in 0..6_000u32 {
            sorter
                .push(TrigramPosting {
                    trigram,
                    entry: PostingEntry {
                        file_id: trigram / 4,
                        loc_mask: 1,
                        next_mask: 2,
                    },
                })
                .unwrap();
        }
        assert!(sorter.spilled_segments() > 1);

        let (trigrams, merged) = sorter.write_postings(dir.path()).unwrap();
        assert_eq!(trigrams, 6_000);
        assert!(merged > 1, "expected a real merge, got {merged} segment(s)");

        // files.bin/meta.json are written by the builder; synthesize the
        // minimum the reader needs to resolve posting lists.
        let file_count = 6_000u32 / 4;
        let mut files = Vec::new();
        for id in 0..file_count {
            ondisk::write_file_entry(&mut files, id, &format!("f{id}.txt")).unwrap();
        }
        std::fs::write(dir.path().join("files.bin"), files).unwrap();
        crate::meta::IndexMeta::new("/tmp/root", file_count as u64, trigrams as u64)
            .save(dir.path())
            .unwrap();

        let reader = IndexReader::open(dir.path()).unwrap();
        for trigram in 0..6_000u32 {
            assert_eq!(
                reader.lookup_trigram(trigram),
                vec![trigram / 4],
                "postings mismatch for trigram {trigram}"
            );
        }
    }
}
