//! Guest-block → native-code lookup.
//!
//! The cache also maintains a per-page translated-code presence index so the
//! write barrier (`GuestMemory::set_write_barrier`) can reject writes far
//! from any translated code in a handful of cycles, and drop exactly the
//! blocks a colliding write overlapped. Structural mapping changes (module
//! load/unmap, permission splits) are coarser: the dispatcher snapshots
//! `GuestMemory::structure_epoch` and calls [`BlockCache::clear`] whenever it
//! advances.

use std::collections::HashMap;

use crate::exec::CodePtr;

/// Guest-page granularity of the translated-code index. Only affects the
/// bitmap's false-positive rate, not correctness; kept at the conventional
/// 4 KiB to match `skyengine-arm`'s `GuestMemory`.
const PAGE_SHIFT: u32 = 12;
const PAGE_COUNT: u32 = 1 << (32 - PAGE_SHIFT);
const CODE_PAGE_WORDS: usize = (PAGE_COUNT / 64) as usize;

/// A translated guest basic block published into the code arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEntry {
    /// Entry offset inside the owning [`crate::exec::ExecMemory`].
    pub code: CodePtr,
    /// Guest PC of the block's first instruction (address bits only; the
    /// Thumb state is tracked separately so one address can carry both an
    /// ARM and a Thumb translation).
    pub guest_pc: u32,
    /// Guest address just past the block's last translated instruction.
    /// Write-barrier invalidation uses it to decide whether a guest write
    /// actually overlapped this block's code.
    pub guest_end: u32,
    /// How many guest instructions the block translated; the dispatcher adds
    /// this to its instruction budget after each execution.
    pub instructions: u32,
    /// Whether the block was translated from Thumb state.
    pub thumb: bool,
}

/// Guest-PC → translated-block lookup. A flat `HashMap` for now: the live
/// block count for MRP workloads is expected in the hundreds; if profiling
/// (P3) shows lookup cost, a direct-mapped table keyed by pc hash can drop
/// in behind the same interface.
pub struct BlockCache {
    map: HashMap<u64, BlockEntry>,
    /// Presence bitmap over guest pages: bit set iff at least one cached
    /// block's guest range intersects the page. The write barrier probes
    /// this first; untouched pages return without touching `map` at all.
    code_pages: Vec<u64>,
    /// Blocks intersecting each touched guest page, so a page's bitmap bit
    /// clears exactly when its last covering block goes away.
    page_shares: HashMap<u32, u32>,
}

/// A guest address is translatable in both ARM and Thumb states, so the key
/// combines the PC with its Thumb flag.
#[inline]
fn key(guest_pc: u32, thumb: bool) -> u64 {
    (u64::from(guest_pc) << 1) | u64::from(thumb)
}

impl Default for BlockCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            code_pages: vec![0; CODE_PAGE_WORDS],
            page_shares: HashMap::new(),
        }
    }

    /// Register a translated block, replacing any previous translation for
    /// the same guest entry point.
    pub fn insert(&mut self, entry: BlockEntry) -> Option<BlockEntry> {
        debug_assert!(entry.guest_end > entry.guest_pc, "empty block guest range");
        if let Some(old) = self.map.get(&key(entry.guest_pc, entry.thumb)) {
            self.retag_pages(old.guest_pc, old.guest_end, -1);
        }
        self.retag_pages(entry.guest_pc, entry.guest_end, 1);
        self.map.insert(key(entry.guest_pc, entry.thumb), entry)
    }

    /// Find the translated block for a guest entry point.
    pub fn lookup(&self, guest_pc: u32, thumb: bool) -> Option<BlockEntry> {
        self.map.get(&key(guest_pc, thumb)).copied()
    }

    /// Unlink a block (manual invalidation entry point).
    pub fn remove(&mut self, guest_pc: u32, thumb: bool) -> Option<BlockEntry> {
        let entry = self.map.remove(&key(guest_pc, thumb))?;
        self.retag_pages(entry.guest_pc, entry.guest_end, -1);
        Some(entry)
    }

    /// Drop every translated block (arena reset companion; also the
    /// structural-mapping-change flush).
    pub fn clear(&mut self) {
        self.map.clear();
        self.page_shares.clear();
        self.code_pages.iter_mut().for_each(|word| *word = 0);
    }

    /// Write-barrier entry point: drop every cached block whose guest code
    /// range intersects `[start, start + len)`. Probes the per-page bitmap
    /// first, so writes far from any translated code cost a couple of loads;
    /// a hit falls back to a linear scan over the (small) block map. Returns
    /// the number of blocks dropped.
    pub fn invalidate_written(&mut self, start: u32, len: u32) -> usize {
        if len == 0 || self.map.is_empty() {
            return 0;
        }
        let end = u64::from(start) + u64::from(len);
        let first_page = start >> PAGE_SHIFT;
        let last_page = u64::from(end.saturating_sub(1) >> PAGE_SHIFT) as u32;
        let mut touched = false;
        for page in first_page..=last_page {
            if self.code_pages[(page >> 6) as usize] & (1u64 << (page & 63)) != 0 {
                touched = true;
                break;
            }
        }
        if !touched {
            return 0;
        }
        let victims: Vec<u64> = self
            .map
            .iter()
            .filter(|(_, entry)| {
                u64::from(entry.guest_pc) < end && u64::from(start) < u64::from(entry.guest_end)
            })
            .map(|(key, _)| *key)
            .collect();
        let mut dropped = 0;
        for key in victims {
            if let Some(entry) = self.map.remove(&key) {
                self.retag_pages(entry.guest_pc, entry.guest_end, -1);
                dropped += 1;
            }
        }
        dropped
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Update the page presence index for the block range
    /// `[start, end)` by `delta` (±1), keeping the bitmap in sync with the
    /// per-page block counts.
    fn retag_pages(&mut self, start: u32, end: u32, delta: i32) {
        let last_page = end.saturating_sub(1) >> PAGE_SHIFT;
        for page in (start >> PAGE_SHIFT)..=last_page {
            let count = self.page_shares.entry(page).or_insert(0);
            if delta < 0 {
                *count = count
                    .checked_sub(1)
                    .expect("translated-page refcount underflow");
            } else {
                *count = count
                    .checked_add(delta as u32)
                    .expect("translated-page refcount overflow");
            }
            let present = *count != 0;
            if !present {
                self.page_shares.remove(&page);
            }
            let word = &mut self.code_pages[(page >> 6) as usize];
            let mask = 1u64 << (page & 63);
            if present {
                *word |= mask;
            } else {
                *word &= !mask;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pc: u32, thumb: bool, offset: u32) -> BlockEntry {
        // A representative 16-byte block (8 Thumb instructions) unless the
        // caller opts into custom ranges via `entry_range`.
        entry_range(pc, pc + 16, thumb, offset)
    }

    fn entry_range(start: u32, end: u32, thumb: bool, offset: u32) -> BlockEntry {
        BlockEntry {
            code: CodePtr(offset),
            guest_pc: start,
            guest_end: end,
            instructions: (end - start) / 2,
            thumb,
        }
    }

    #[test]
    fn insert_lookup_remove_round_trip() {
        let mut cache = BlockCache::new();
        assert!(cache.lookup(0x1000, true).is_none());

        cache.insert(entry(0x1000, true, 0));
        cache.insert(entry(0x2000, false, 16));
        assert_eq!(cache.len(), 2);

        assert_eq!(cache.lookup(0x1000, true), Some(entry(0x1000, true, 0)));
        assert_eq!(cache.lookup(0x2000, false), Some(entry(0x2000, false, 16)));
        // Same address in the other instruction state is a distinct block.
        assert_eq!(cache.lookup(0x1000, false), None);
        assert_eq!(cache.lookup(0x2000, true), None);

        assert_eq!(cache.remove(0x1000, true), Some(entry(0x1000, true, 0)));
        assert_eq!(cache.lookup(0x1000, true), None);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_replaces_previous_translation() {
        let mut cache = BlockCache::new();
        cache.insert(entry(0x3000, true, 0));
        let old = cache.insert(entry(0x3000, true, 48));
        assert_eq!(old, Some(entry(0x3000, true, 0)));
        assert_eq!(cache.lookup(0x3000, true).unwrap().code, CodePtr(48));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn clear_drops_everything() {
        let mut cache = BlockCache::new();
        cache.insert(entry(0x1000, true, 0));
        cache.insert(entry(0x2000, false, 16));
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.lookup(0x1000, true), None);
        // The page index must go quiet again: writes into formerly hot
        // pages take the fast zero path.
        assert_eq!(cache.invalidate_written(0x1000, 8), 0);
    }

    #[test]
    fn invalidate_written_drops_only_overlapping_blocks() {
        let mut cache = BlockCache::new();
        cache.insert(entry_range(0x1000, 0x1010, true, 0)); // A
        cache.insert(entry_range(0x1010, 0x1020, true, 16)); // B, adjacent
        cache.insert(entry_range(0x2000, 0x2020, false, 32)); // C, far away

        // Write inside A drops only A; B starts exactly at A's exclusive end.
        assert_eq!(cache.invalidate_written(0x1008, 4), 1);
        assert!(cache.lookup(0x1000, true).is_none());
        assert!(cache.lookup(0x1010, true).is_some());
        assert!(cache.lookup(0x2000, false).is_some());
        assert_eq!(cache.len(), 2);

        // A write starting at B's exclusive end leaves B in place.
        assert_eq!(cache.invalidate_written(0x1020, 4), 0);
        assert!(cache.lookup(0x1010, true).is_some());

        // A wide write sweeping both remaining blocks drops them together.
        assert_eq!(cache.invalidate_written(0x1000, 0x1100), 2);
        assert!(cache.is_empty());
    }

    #[test]
    fn invalidate_written_fast_path_keeps_unrelated_blocks() {
        let mut cache = BlockCache::new();
        cache.insert(entry_range(0x1000, 0x1010, true, 0));

        // Same page, no overlap: bitmap hit, linear scan, zero drops.
        assert_eq!(cache.invalidate_written(0x1800, 4), 0);
        // Different page entirely: bitmap miss, zero drops.
        assert_eq!(cache.invalidate_written(0x9000, 4), 0);
        assert_eq!(cache.len(), 1);

        // Zero-length writes never invalidate anything.
        assert_eq!(cache.invalidate_written(0x1000, 0), 0);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn invalidate_written_handles_cross_page_blocks() {
        let mut cache = BlockCache::new();
        // Block straddling the page boundary at 0x2000.
        cache.insert(entry_range(0x1FFE, 0x2006, true, 0));
        // Write into the second page of the span but past the block.
        assert_eq!(cache.invalidate_written(0x2FF0, 4), 0);
        assert_eq!(cache.len(), 1);
        // Write inside the span drops it and clears both page bits.
        assert_eq!(cache.invalidate_written(0x2000, 2), 1);
        assert!(cache.is_empty());
        assert_eq!(cache.invalidate_written(0x1FFE, 8), 0);
        assert_eq!(cache.invalidate_written(0x2000, 8), 0);
    }

    #[test]
    fn invalidate_written_probes_bits_past_the_first_bitmap_word_half() {
        // Page indices >= 32 exercise `1u64 << (page & 63)` bits beyond the
        // low i32 half; a debug build used to panic on shift overflow there.
        let mut cache = BlockCache::new();
        // Guest page 0x20 (address 0x20000) is bit 32 of bitmap word 0.
        cache.insert(entry_range(0x2_0000, 0x2_0010, true, 0));
        assert_eq!(cache.invalidate_written(0x2_1000, 4), 0);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.invalidate_written(0x2_0004, 4), 1);
        assert!(cache.is_empty());
        // Guest page 0x41 lands in bit 1 of word 1.
        cache.insert(entry_range(0x4_1000, 0x4_1010, true, 16));
        assert_eq!(cache.invalidate_written(0x4_1008, 2), 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn page_bits_follow_shared_and_last_blocks() {
        let mut cache = BlockCache::new();
        cache.insert(entry_range(0x1000, 0x1010, true, 0));
        cache.insert(entry_range(0x1010, 0x1020, true, 16));
        // Two blocks share the page; dropping one keeps the bit alive.
        assert_eq!(cache.remove(0x1000, true).is_some(), true);
        assert_eq!(cache.invalidate_written(0x1018, 4), 1);
        assert!(cache.is_empty());
        // With the page empty the bitmap must be quiet again.
        assert_eq!(cache.invalidate_written(0x1000, 0x1000), 0);
    }

    #[test]
    fn insert_replacement_tracks_the_new_range() {
        let mut cache = BlockCache::new();
        cache.insert(entry_range(0x1000, 0x1010, true, 0));
        // Retranslation grows the block; the old range's page bookkeeping
        // must not leak.
        cache.insert(entry_range(0x1000, 0x1030, true, 64));
        assert_eq!(cache.invalidate_written(0x1020, 4), 1);
        assert!(cache.is_empty());
        assert_eq!(cache.invalidate_written(0x1000, 0x1000), 0);
    }
}
