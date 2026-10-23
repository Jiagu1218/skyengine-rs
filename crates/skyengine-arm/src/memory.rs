use std::sync::{
    OnceLock,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};

use crate::{Error, Result};

/// Debug aid: PC of the guest instruction currently executing, kept fresh by
/// the CPU step loop. Read by the SKYENGINE_WATCH_ADDR watchpoint logger.
pub static CURRENT_PC: AtomicU32 = AtomicU32::new(0);

/// Address of the most recent unmapped guest access (0 when none). Hosts can
/// take it after a failed step to run compatibility fallbacks, such as
/// mapping a legacy memory window on demand before retrying the instruction.
static LAST_UNMAPPED_ACCESS: AtomicU32 = AtomicU32::new(0);

/// Take the address of the most recent unmapped guest access, clearing it.
pub fn take_last_unmapped_access() -> Option<u32> {
    match LAST_UNMAPPED_ACCESS.swap(0, Ordering::Relaxed) {
        0 => None,
        address => Some(address),
    }
}

/// Callback invoked after every successful guest memory write. The JIT
/// dispatcher registers one so translated blocks whose guest code a write
/// overwrote can be dropped before execution re-enters them.
pub type WriteBarrierFn = fn(start: u32, len: u32);

/// Installed write-barrier callback, stored as a raw `usize` (0 = none) so
/// the write paths only pay one relaxed load per write when no barrier is
/// registered. The engine is single-threaded; the atomic only exists so the
/// static can be mutated at all.
static WRITE_BARRIER: AtomicUsize = AtomicUsize::new(0);

/// Install (or clear, passing `None`) the global write-barrier callback.
pub fn set_write_barrier(barrier: Option<WriteBarrierFn>) {
    WRITE_BARRIER.store(
        barrier.map_or(0, |barrier| barrier as usize),
        Ordering::Relaxed,
    );
}

/// Notify the installed barrier that a write of `len` bytes at `address`
/// succeeded. No-op unless a callback is registered.
#[inline]
fn notify_write(address: u32, len: u32) {
    let barrier = WRITE_BARRIER.load(Ordering::Relaxed);
    if barrier != 0 {
        // SAFETY: nonzero values are only ever stored from a
        // `WriteBarrierFn` pointer in `set_write_barrier`.
        let barrier = unsafe { core::mem::transmute::<usize, WriteBarrierFn>(barrier) };
        barrier(address, len);
    }
}

static WATCH_TARGET: OnceLock<u32> = OnceLock::new();
static WATCH_LENGTH: OnceLock<usize> = OnceLock::new();

/// Address watched when SKYENGINE_WATCH_ADDR=0xHEX is set; u32::MAX when off.
fn watch_target() -> u32 {
    *WATCH_TARGET.get_or_init(|| {
        std::env::var_os("SKYENGINE_WATCH_ADDR")
            .map(|value| {
                let text = value.to_string_lossy();
                let text = text.trim_start_matches("0x").trim_start_matches("0X");
                u32::from_str_radix(text, 16).unwrap_or(u32::MAX)
            })
            .unwrap_or(u32::MAX)
    })
}

/// Watched byte count (SKYENGINE_WATCH_LEN, default 4).
fn watch_length() -> usize {
    *WATCH_LENGTH.get_or_init(|| {
        std::env::var_os("SKYENGINE_WATCH_LEN")
            .and_then(|value| value.to_string_lossy().trim().parse().ok())
            .unwrap_or(4)
    })
}

/// Log guest writes touching [watch, watch+len) as `[watch] pc=.. write addr=.. len=.. value=..`.
fn watch_report(address: GuestAddr, len: usize, bytes: &[u8]) {
    let watch = watch_target();
    if watch == u32::MAX {
        return;
    }
    let length = watch_length();
    let start = u64::from(address.0);
    let end = start + len as u64;
    if end > u64::from(watch) && start < u64::from(watch) + length as u64 {
        let mut value_text = String::new();
        for byte in bytes.iter().take(4) {
            value_text.push_str(&format!("{byte:02x}"));
        }
        eprintln!(
            "[watch] pc=0x{:08x} write addr=0x{:08x} len={} value={}",
            CURRENT_PC.load(Ordering::Relaxed),
            address.0,
            len,
            value_text
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct GuestAddr(pub u32);

impl GuestAddr {
    pub fn checked_add(self, offset: u32) -> Result<Self> {
        self.0.checked_add(offset).map(Self).ok_or_else(|| {
            Error::ArmFault(format!("guest address overflow: {self:?} + {offset:#x}"))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions(u8);

impl Permissions {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);
    pub const READ_EXECUTE: Self = Self(Self::READ.0 | Self::EXECUTE.0);
    pub const READ_WRITE_EXECUTE: Self = Self(Self::READ.0 | Self::WRITE.0 | Self::EXECUTE.0);

    fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    fn union(self, additional: Self) -> Self {
        Self(self.0 | additional.0)
    }

    fn without(self, removed: Self) -> Self {
        Self(self.0 & !removed.0)
    }
}

/// Guest pages are 4 KiB; the flat page directory below maps every guest
/// page to either an owning region or the linear-scan fallback.
const PAGE_SHIFT: u32 = 12;
const PAGE_SIZE: u32 = 1 << PAGE_SHIFT;
const PAGE_MASK: u32 = PAGE_SIZE - 1;
const PAGE_COUNT: usize = 1 << (32 - PAGE_SHIFT);
/// Directory entry encoding: 0 falls back to the linear scan; otherwise the
/// low 16 bits hold `region index + 1` and bits 16..24 hold the permission
/// mask of the region that fully covers the page.
const PAGE_ENTRY_SLOW: u32 = 0;

#[derive(Debug)]
struct Region {
    base: u32,
    bytes: Vec<u8>,
    permissions: Permissions,
    name: String,
    mapping_id: u64,
    mapping_base: u32,
    mapping_len: usize,
}

impl Region {
    fn end(&self) -> u64 {
        u64::from(self.base) + self.bytes.len() as u64
    }
}

#[derive(Debug, Default)]
pub struct GuestMemory {
    regions: Vec<Region>,
    next_mapping_id: u64,
    /// Flat page directory, empty until the first successful mapping. Pages
    /// fully covered by exactly one region resolve to it in O(1); every other
    /// page (unmapped, partially covered, or shared by split regions) keeps
    /// using the linear region scan.
    pages: Vec<u32>,
    /// Bumped by every successful structural change (`map_bytes`, `unmap`,
    /// `change_permissions`). The JIT dispatcher snapshots it and flushes the
    /// whole block cache whenever it changes, since remapping can replace
    /// guest code wholesale. Ordinary reads and writes never touch it.
    structure_epoch: u32,
}

impl GuestMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of mapped regions (diagnostics).
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Current structural-mapping epoch. See the field docs; callers
    /// interested in wholesale cache flushes snapshot and compare it.
    pub fn structure_epoch(&self) -> u32 {
        self.structure_epoch
    }

    pub fn map(
        &mut self,
        base: GuestAddr,
        len: usize,
        permissions: Permissions,
        name: impl Into<String>,
    ) -> Result<()> {
        self.map_bytes(base, vec![0; len], permissions, name)
    }

    pub fn map_bytes(
        &mut self,
        base: GuestAddr,
        bytes: Vec<u8>,
        permissions: Permissions,
        name: impl Into<String>,
    ) -> Result<()> {
        if bytes.is_empty() {
            return Err(Error::ArmFault("cannot map an empty guest region".into()));
        }
        let end = u64::from(base.0)
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::ArmFault("guest mapping range overflow".into()))?;
        if end > u64::from(u32::MAX) + 1 {
            return Err(Error::ArmFault(format!(
                "guest mapping {:#x}..{end:#x} exceeds the 32-bit address space",
                base.0
            )));
        }
        if let Some(region) = self
            .regions
            .iter()
            .find(|region| u64::from(base.0) < region.end() && u64::from(region.base) < end)
        {
            return Err(Error::ArmFault(format!(
                "guest mapping {:#x}..{end:#x} overlaps {} at {:#x}..{:#x}",
                base.0,
                region.name,
                region.base,
                region.end()
            )));
        }
        let mapping_id = self.next_mapping_id;
        self.next_mapping_id = self
            .next_mapping_id
            .checked_add(1)
            .ok_or_else(|| Error::ArmFault("guest mapping identifier overflow".into()))?;
        self.regions.push(Region {
            base: base.0,
            bytes,
            permissions,
            name: name.into(),
            mapping_id,
            mapping_base: base.0,
            mapping_len: usize::try_from(end - u64::from(base.0))
                .expect("mapping length originated as usize"),
        });
        self.regions.sort_unstable_by_key(|region| region.base);
        self.rebuild_pages();
        self.structure_epoch = self.structure_epoch.wrapping_add(1);
        Ok(())
    }

    pub fn read_u8(&self, address: GuestAddr) -> Result<u8> {
        Ok(self.range(address, 1, Permissions::READ)?[0])
    }

    pub fn read_u16(&self, address: GuestAddr) -> Result<u16> {
        let bytes = self.read_array(address, Permissions::READ)?;
        Ok(u16::from_le_bytes(bytes))
    }

    pub fn read_u32(&self, address: GuestAddr) -> Result<u32> {
        let bytes = self.read_array(address, Permissions::READ)?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub fn fetch_u16(&self, address: GuestAddr) -> Result<u16> {
        let bytes = self.read_array(address, Permissions::EXECUTE)?;
        Ok(u16::from_le_bytes(bytes))
    }

    pub fn fetch_u32(&self, address: GuestAddr) -> Result<u32> {
        let bytes = self.read_array(address, Permissions::EXECUTE)?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub fn read(&self, address: GuestAddr, len: usize) -> Result<Vec<u8>> {
        match self.range(address, len, Permissions::READ) {
            Ok(bytes) => Ok(bytes.to_vec()),
            Err(_) => self.read_checked(address, len, Permissions::READ),
        }
    }

    pub fn write_u8(&mut self, address: GuestAddr, value: u8) -> Result<()> {
        watch_report(address, 1, &[value]);
        self.range_mut(address, 1, Permissions::WRITE)?[0] = value;
        notify_write(address.0, 1);
        Ok(())
    }

    pub fn write_u16(&mut self, address: GuestAddr, value: u16) -> Result<()> {
        self.write(address, &value.to_le_bytes())
    }

    pub fn write_u32(&mut self, address: GuestAddr, value: u32) -> Result<()> {
        self.write(address, &value.to_le_bytes())
    }

    pub fn write(&mut self, address: GuestAddr, bytes: &[u8]) -> Result<()> {
        watch_report(address, bytes.len(), bytes);
        if let Ok(target) = self.range_mut(address, bytes.len(), Permissions::WRITE) {
            target.copy_from_slice(bytes);
            notify_write(address.0, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
            return Ok(());
        }
        let segments = self.segments(address, bytes.len(), Permissions::WRITE)?;
        let mut source_offset = 0;
        for (index, offset, len) in segments {
            self.regions[index].bytes[offset..offset + len]
                .copy_from_slice(&bytes[source_offset..source_offset + len]);
            source_offset += len;
        }
        notify_write(address.0, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        Ok(())
    }

    pub fn check_range(
        &self,
        address: GuestAddr,
        len: usize,
        permissions: Permissions,
    ) -> Result<()> {
        self.segments(address, len, permissions).map(|_| ())
    }

    /// Check whether the full range is mapped, regardless of its permissions.
    pub fn is_mapped(&self, address: GuestAddr, len: usize) -> bool {
        self.segments(address, len, Permissions(0)).is_ok()
    }

    pub fn add_permissions(
        &mut self,
        address: GuestAddr,
        len: usize,
        permissions: Permissions,
    ) -> Result<()> {
        self.change_permissions(address, len, permissions, true)
    }

    pub fn remove_permissions(
        &mut self,
        address: GuestAddr,
        len: usize,
        permissions: Permissions,
    ) -> Result<()> {
        self.change_permissions(address, len, permissions, false)
    }

    fn change_permissions(
        &mut self,
        address: GuestAddr,
        len: usize,
        permissions: Permissions,
        add: bool,
    ) -> Result<()> {
        if len == 0 {
            return Err(Error::ArmFault(
                "cannot change permissions for an empty guest range".into(),
            ));
        }

        let segments = self.segments(address, len, Permissions(0))?;
        let mapping_id = self.regions[segments[0].0].mapping_id;
        if segments
            .iter()
            .any(|(index, _, _)| self.regions[*index].mapping_id != mapping_id)
        {
            return Err(Error::ArmFault(format!(
                "permission range at {:#010x} ({} bytes) crosses guest mappings",
                address.0, len
            )));
        }

        let requested_start = u64::from(address.0);
        let requested_end = requested_start + len as u64;
        let mut replacements = Vec::with_capacity(self.regions.len() + segments.len() * 2);
        for region in std::mem::take(&mut self.regions) {
            let region_start = u64::from(region.base);
            let region_end = region.end();
            if requested_start >= region_end || region_start >= requested_end {
                replacements.push(region);
                continue;
            }

            let overlap_start = (requested_start.max(region_start) - region_start) as usize;
            let overlap_end = (requested_end.min(region_end) - region_start) as usize;
            let Region {
                base,
                mut bytes,
                permissions: current_permissions,
                name,
                mapping_id,
                mapping_base,
                mapping_len,
            } = region;
            let after = bytes.split_off(overlap_end);
            let middle = bytes.split_off(overlap_start);
            if !bytes.is_empty() {
                replacements.push(Region {
                    base,
                    bytes,
                    permissions: current_permissions,
                    name: name.clone(),
                    mapping_id,
                    mapping_base,
                    mapping_len,
                });
            }
            replacements.push(Region {
                base: base + overlap_start as u32,
                bytes: middle,
                permissions: if add {
                    current_permissions.union(permissions)
                } else {
                    current_permissions.without(permissions)
                },
                name: name.clone(),
                mapping_id,
                mapping_base,
                mapping_len,
            });
            if !after.is_empty() {
                replacements.push(Region {
                    base: base + overlap_end as u32,
                    bytes: after,
                    permissions: current_permissions,
                    name,
                    mapping_id,
                    mapping_base,
                    mapping_len,
                });
            }
        }
        self.regions = replacements;
        self.rebuild_pages();
        self.structure_epoch = self.structure_epoch.wrapping_add(1);
        Ok(())
    }

    pub fn unmap(&mut self, base: GuestAddr, len: usize) -> Result<()> {
        let requested_end = u64::from(base.0)
            .checked_add(len as u64)
            .ok_or_else(|| Error::ArmFault("guest unmap range overflow".into()))?;
        let Some(first) = self.regions.iter().position(|region| region.base == base.0) else {
            return Err(Error::ArmFault(format!(
                "guest unmap does not match a region at {:#010x} ({} bytes)",
                base.0, len
            )));
        };
        let mapping_id = self.regions[first].mapping_id;
        if self.regions[first].mapping_base != base.0 || self.regions[first].mapping_len != len {
            return Err(Error::ArmFault(format!(
                "guest unmap does not match the original mapping at {:#010x} ({} bytes)",
                base.0, len
            )));
        }
        let mut cursor = u64::from(base.0);
        let mut after_last = first;
        while cursor < requested_end {
            let Some(region) = self.regions.get(after_last) else {
                break;
            };
            if region.mapping_id != mapping_id
                || u64::from(region.base) != cursor
                || region.end() > requested_end
            {
                break;
            }
            cursor = region.end();
            after_last += 1;
        }
        if cursor != requested_end || first == after_last {
            return Err(Error::ArmFault(format!(
                "guest unmap does not match contiguous regions at {:#010x} ({} bytes)",
                base.0, len
            )));
        }
        self.regions.drain(first..after_last);
        self.rebuild_pages();
        self.structure_epoch = self.structure_epoch.wrapping_add(1);
        Ok(())
    }

    fn read_array<const N: usize>(
        &self,
        address: GuestAddr,
        required: Permissions,
    ) -> Result<[u8; N]> {
        match self.range(address, N, required) {
            Ok(bytes) => Ok(bytes.try_into().expect("checked fixed-size range")),
            Err(_) => Ok(self
                .read_checked(address, N, required)?
                .try_into()
                .expect("checked fixed-size segmented range")),
        }
    }

    fn range(&self, address: GuestAddr, len: usize, required: Permissions) -> Result<&[u8]> {
        let (region, offset) = self.locate(address, len, required)?;
        Ok(&region.bytes[offset..offset + len])
    }

    fn range_mut(
        &mut self,
        address: GuestAddr,
        len: usize,
        required: Permissions,
    ) -> Result<&mut [u8]> {
        let index = self.locate_index(address, len, required)?;
        let region = &mut self.regions[index];
        let offset = (address.0 - region.base) as usize;
        Ok(&mut region.bytes[offset..offset + len])
    }

    fn locate(
        &self,
        address: GuestAddr,
        len: usize,
        required: Permissions,
    ) -> Result<(&Region, usize)> {
        let index = self.locate_index(address, len, required)?;
        let region = &self.regions[index];
        Ok((region, (address.0 - region.base) as usize))
    }

    fn read_checked(
        &self,
        address: GuestAddr,
        len: usize,
        required: Permissions,
    ) -> Result<Vec<u8>> {
        let segments = self.segments(address, len, required)?;
        let mut bytes = Vec::with_capacity(len);
        for (index, offset, len) in segments {
            bytes.extend_from_slice(&self.regions[index].bytes[offset..offset + len]);
        }
        Ok(bytes)
    }

    fn segments(
        &self,
        address: GuestAddr,
        len: usize,
        required: Permissions,
    ) -> Result<Vec<(usize, usize, usize)>> {
        let end = u64::from(address.0)
            .checked_add(len as u64)
            .ok_or_else(|| Error::ArmFault("guest access range overflow".into()))?;
        if end > u64::from(u32::MAX) + 1 {
            return Err(Error::ArmFault(
                "guest access exceeds the 32-bit address space".into(),
            ));
        }

        let mut cursor = u64::from(address.0);
        let mut segments = Vec::new();
        while cursor < end {
            let Some((index, region)) = self
                .regions
                .iter()
                .enumerate()
                .find(|(_, region)| cursor >= u64::from(region.base) && cursor < region.end())
            else {
                LAST_UNMAPPED_ACCESS.store(cursor as u32, Ordering::Relaxed);
                return Err(Error::ArmFault(format!(
                    "unmapped guest access at {cursor:#010x} ({} bytes)",
                    end - cursor
                )));
            };
            if !region.permissions.contains(required) {
                return Err(Error::ArmFault(format!(
                    "permission fault in {} at {cursor:#010x} (need {:?}, have {:?})",
                    region.name, required, region.permissions
                )));
            }
            let segment_end = end.min(region.end());
            segments.push((
                index,
                (cursor - u64::from(region.base)) as usize,
                (segment_end - cursor) as usize,
            ));
            cursor = segment_end;
        }
        Ok(segments)
    }

    /// Rebuild the flat page directory after any region-structure change.
    /// Pages fully covered by exactly one region become O(1) fast entries;
    /// the rest keep the linear scan. Region layouts only change through
    /// `map_bytes`, `change_permissions`, and `unmap`, all cold paths.
    fn rebuild_pages(&mut self) {
        let mut pages = vec![PAGE_ENTRY_SLOW; PAGE_COUNT];
        if self.regions.len() <= u16::MAX as usize {
            for (index, region) in self.regions.iter().enumerate() {
                let end = region.end();
                // Page p is fully covered iff region.base <= p<<12 and
                // (p+1)<<12 <= region.end.
                let first = (u64::from(region.base) + u64::from(PAGE_MASK)) >> PAGE_SHIFT;
                let last = end >> PAGE_SHIFT;
                if first >= last {
                    continue;
                }
                let entry = (u32::from(region.permissions.0) << 16) | (index as u32 + 1);
                for page in first as usize..last as usize {
                    pages[page] = entry;
                }
            }
        }
        self.pages = pages;
    }

    /// Resolve an access through the page directory when it stays inside one
    /// page fully covered by a region with sufficient permissions. Returning
    /// `None` defers to the linear scan, which decides success and error
    /// messages exactly as before.
    #[inline]
    fn locate_index_fast(
        &self,
        address: GuestAddr,
        len: usize,
        required: Permissions,
    ) -> Option<usize> {
        let offset = (address.0 & PAGE_MASK) as usize;
        if offset.checked_add(len)? > PAGE_SIZE as usize {
            return None;
        }
        let entry = *self.pages.get((address.0 >> PAGE_SHIFT) as usize)?;
        if entry == PAGE_ENTRY_SLOW {
            return None;
        }
        if (entry >> 16) as u8 & required.0 != required.0 {
            return None;
        }
        Some((entry & 0xFFFF) as usize - 1)
    }

    fn locate_index(&self, address: GuestAddr, len: usize, required: Permissions) -> Result<usize> {
        if let Some(index) = self.locate_index_fast(address, len, required) {
            return Ok(index);
        }
        let end = u64::from(address.0)
            .checked_add(len as u64)
            .ok_or_else(|| Error::ArmFault("guest access range overflow".into()))?;
        let Some((index, region)) = self
            .regions
            .iter()
            .enumerate()
            .find(|(_, region)| address.0 >= region.base && end <= region.end())
        else {
            LAST_UNMAPPED_ACCESS.store(address.0, Ordering::Relaxed);
            return Err(Error::ArmFault(format!(
                "unmapped guest access at {:#010x} ({} bytes)",
                address.0, len
            )));
        };
        if !region.permissions.contains(required) {
            return Err(Error::ArmFault(format!(
                "permission fault in {} at {:#010x} (need {:?}, have {:?})",
                region.name, address.0, required, region.permissions
            )));
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mappings_check_overlap_permissions_and_bounds() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 16, Permissions::READ_WRITE, "data")
            .unwrap();
        memory.write_u32(GuestAddr(0x1004), 0x7856_3412).unwrap();
        assert_eq!(memory.read_u32(GuestAddr(0x1004)).unwrap(), 0x7856_3412);
        assert!(memory.fetch_u32(GuestAddr(0x1004)).is_err());
        assert!(memory.read_u32(GuestAddr(0x100e)).is_err());
        assert!(
            memory
                .map(GuestAddr(0x1008), 8, Permissions::READ, "overlap")
                .is_err()
        );
    }

    #[test]
    fn adds_permissions_only_to_the_requested_range() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 16, Permissions::READ_WRITE, "heap")
            .unwrap();
        memory
            .write(GuestAddr(0x1000), &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66])
            .unwrap();

        assert!(memory.fetch_u16(GuestAddr(0x1002)).is_err());
        memory
            .add_permissions(GuestAddr(0x1002), 4, Permissions::EXECUTE)
            .unwrap();

        assert_eq!(memory.fetch_u16(GuestAddr(0x1002)).unwrap(), 0x4433);
        assert_eq!(memory.fetch_u16(GuestAddr(0x1004)).unwrap(), 0x6655);
        assert!(memory.fetch_u16(GuestAddr(0x1000)).is_err());
        assert!(memory.fetch_u32(GuestAddr(0x1004)).is_err());
        assert!(memory.fetch_u16(GuestAddr(0x1006)).is_err());
        assert_eq!(
            memory.read(GuestAddr(0x1000), 8).unwrap(),
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0, 0]
        );
        assert_eq!(memory.read_u32(GuestAddr(0x1002)).unwrap(), 0x6655_4433);
        memory.write_u32(GuestAddr(0x1000), 0x4030_2010).unwrap();
        assert_eq!(memory.read_u32(GuestAddr(0x1000)).unwrap(), 0x4030_2010);
        memory.write_u16(GuestAddr(0x1002), 0x8877).unwrap();
        assert_eq!(memory.read_u16(GuestAddr(0x1002)).unwrap(), 0x8877);
    }

    #[test]
    fn extends_permissions_across_regions_split_by_an_earlier_change() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 16, Permissions::READ_WRITE, "heap")
            .unwrap();
        memory
            .add_permissions(GuestAddr(0x1004), 4, Permissions::EXECUTE)
            .unwrap();

        memory
            .add_permissions(GuestAddr(0x1002), 10, Permissions::EXECUTE)
            .unwrap();

        assert!(memory.fetch_u16(GuestAddr(0x1000)).is_err());
        assert_eq!(memory.fetch_u16(GuestAddr(0x1002)).unwrap(), 0);
        assert_eq!(memory.fetch_u16(GuestAddr(0x100a)).unwrap(), 0);
        assert!(memory.fetch_u16(GuestAddr(0x100c)).is_err());
    }

    #[test]
    fn rejects_permission_changes_outside_one_mapped_region() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 8, Permissions::READ_WRITE, "same name")
            .unwrap();
        memory
            .map(GuestAddr(0x1008), 8, Permissions::READ_WRITE, "same name")
            .unwrap();

        assert!(
            memory
                .add_permissions(GuestAddr(0x1004), 8, Permissions::EXECUTE)
                .is_err()
        );
        assert!(
            memory
                .add_permissions(GuestAddr(0x0ffc), 8, Permissions::EXECUTE)
                .is_err()
        );
        assert!(
            memory
                .add_permissions(GuestAddr(0x1000), 0, Permissions::EXECUTE)
                .is_err()
        );
    }

    #[test]
    fn removes_permissions_only_from_the_requested_mapping_range() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 16, Permissions::READ_WRITE, "heap")
            .unwrap();
        memory
            .add_permissions(GuestAddr(0x1004), 4, Permissions::EXECUTE)
            .unwrap();
        assert_eq!(memory.fetch_u32(GuestAddr(0x1004)).unwrap(), 0);

        memory
            .remove_permissions(GuestAddr(0x1004), 4, Permissions::EXECUTE)
            .unwrap();

        assert!(memory.fetch_u16(GuestAddr(0x1004)).is_err());
        assert_eq!(memory.read_u32(GuestAddr(0x1004)).unwrap(), 0);
        memory.write_u32(GuestAddr(0x1004), 1).unwrap();
    }

    #[test]
    fn exact_mappings_can_be_unmapped() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 16, Permissions::READ_WRITE, "temporary")
            .unwrap();

        assert!(memory.unmap(GuestAddr(0x1000), 8).is_err());
        memory.unmap(GuestAddr(0x1000), 16).unwrap();
        assert!(memory.read_u8(GuestAddr(0x1000)).is_err());
    }

    #[test]
    fn mappings_split_by_permissions_can_be_unmapped_as_one_exact_range() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 16, Permissions::READ_WRITE, "temporary")
            .unwrap();
        memory
            .add_permissions(GuestAddr(0x1004), 4, Permissions::EXECUTE)
            .unwrap();

        assert!(memory.unmap(GuestAddr(0x1000), 8).is_err());
        assert!(memory.unmap(GuestAddr(0x1004), 4).is_err());
        memory.unmap(GuestAddr(0x1000), 16).unwrap();

        assert!(memory.read_u8(GuestAddr(0x1000)).is_err());
        assert!(memory.fetch_u16(GuestAddr(0x1004)).is_err());
    }

    #[test]
    fn unmap_rejects_gaps_and_partial_boundary_regions() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 8, Permissions::READ_WRITE, "first")
            .unwrap();
        memory
            .map(GuestAddr(0x1010), 8, Permissions::READ_WRITE, "second")
            .unwrap();

        assert!(memory.unmap(GuestAddr(0x1000), 24).is_err());
        assert!(memory.unmap(GuestAddr(0x1000), 4).is_err());
        assert!(memory.unmap(GuestAddr(0x1004), 4).is_err());
        assert_eq!(memory.read_u8(GuestAddr(0x1000)).unwrap(), 0);
        assert_eq!(memory.read_u8(GuestAddr(0x1010)).unwrap(), 0);
    }

    #[test]
    fn unmap_rejects_adjacent_independent_mappings() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 8, Permissions::READ_WRITE, "temporary")
            .unwrap();
        memory
            .map(GuestAddr(0x1008), 8, Permissions::READ_WRITE, "temporary")
            .unwrap();

        assert!(memory.unmap(GuestAddr(0x1000), 16).is_err());
        assert_eq!(memory.read_u8(GuestAddr(0x1000)).unwrap(), 0);
        assert_eq!(memory.read_u8(GuestAddr(0x1008)).unwrap(), 0);
    }

    #[test]
    fn page_directory_serves_fully_covered_pages() {
        let mut memory = GuestMemory::new();
        memory
            .map(
                GuestAddr(0x1000),
                PAGE_SIZE as usize * 3,
                Permissions::READ_WRITE,
                "bulk",
            )
            .unwrap();
        memory.write_u32(GuestAddr(0x2500), 0x1234_5678).unwrap();
        assert_eq!(memory.read_u32(GuestAddr(0x2500)).unwrap(), 0x1234_5678);

        // Pages 1..4 (0x1000..0x4000) are fully covered and resolve directly.
        assert_eq!(
            memory.locate_index_fast(GuestAddr(0x2500), 4, Permissions::READ),
            Some(0)
        );
        // Accesses crossing a page boundary fall back to the linear scan.
        assert_eq!(
            memory.locate_index_fast(GuestAddr(0x2ffe), 4, Permissions::READ),
            None
        );
        assert_eq!(memory.read_u32(GuestAddr(0x2ffe)).unwrap(), 0);
        // Accesses beyond the mapping and permission faults still report.
        assert!(memory.read_u32(GuestAddr(0x4000)).is_err());
        assert!(memory.fetch_u32(GuestAddr(0x2500)).is_err());
    }

    #[test]
    fn page_directory_tracks_permission_splits_and_unmaps() {
        let mut memory = GuestMemory::new();
        memory
            .map(
                GuestAddr(0x1000),
                PAGE_SIZE as usize * 4,
                Permissions::READ,
                "code",
            )
            .unwrap();
        // Split the region at a non-page boundary: pages containing the split
        // halves stay on the slow path, interior pages stay fast.
        memory
            .add_permissions(GuestAddr(0x1080), 0x100, Permissions::EXECUTE)
            .unwrap();

        assert!(
            memory
                .locate_index_fast(GuestAddr(0x10c0), 2, Permissions::EXECUTE)
                .is_none()
        );
        assert_eq!(memory.fetch_u16(GuestAddr(0x10c0)).unwrap(), 0);
        assert_eq!(
            memory.locate_index_fast(GuestAddr(0x2800), 1, Permissions::READ),
            Some(2)
        );
        assert_eq!(memory.read_u8(GuestAddr(0x2800)).unwrap(), 0);
        assert!(memory.fetch_u16(GuestAddr(0x2800)).is_err());
        assert!(memory.write_u8(GuestAddr(0x2800), 1).is_err());

        memory
            .unmap(GuestAddr(0x1000), PAGE_SIZE as usize * 4)
            .unwrap();
        assert!(memory.read_u8(GuestAddr(0x2800)).is_err());
        assert_eq!(
            memory.locate_index_fast(GuestAddr(0x2800), 1, Permissions::READ),
            None
        );
    }

    #[test]
    fn page_directory_leaves_partial_pages_on_the_slow_path() {
        let mut memory = GuestMemory::new();
        memory
            .map(GuestAddr(0x1000), 0x20, Permissions::READ_WRITE, "small")
            .unwrap();
        // The 32-byte region only partially covers its page.
        assert_eq!(
            memory.locate_index_fast(GuestAddr(0x1008), 4, Permissions::READ),
            None
        );
        memory.write_u32(GuestAddr(0x1008), 0xabcd_ef01).unwrap();
        assert_eq!(memory.read_u32(GuestAddr(0x1008)).unwrap(), 0xabcd_ef01);
    }

    /// Capture log for the write-barrier tests. The barrier ABI is a plain
    /// function pointer, so it cannot close over state.
    static BARRIER_LOG: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());

    /// Serializes the tests that install the process-global write barrier;
    /// without it, parallel test threads would cross-notify each other.
    static BARRIER_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Private address window of the barrier tests (no other test maps here).
    const BARRIER_TEST_WINDOW: std::ops::Range<u32> = 0x7E00_0000..0x7E20_0000;

    fn barrier_probe(start: u32, len: u32) {
        // Parallel tests write their own low mappings while the barrier is
        // installed; only record this window so the asserted log stays exact.
        if BARRIER_TEST_WINDOW.contains(&start) {
            BARRIER_LOG.lock().unwrap().push((start, len));
        }
    }

    #[test]
    fn write_barrier_observes_all_successful_writes() {
        let _guard = BARRIER_TEST_GUARD.lock().unwrap();
        let mut memory = GuestMemory::new();
        // Distinctive addresses no other test maps, so concurrent tests'
        // successful writes cannot leak into the asserted log even while
        // this test holds the installed barrier.
        const BASE: u32 = 0x7E00_0000;
        memory
            .map(
                GuestAddr(BASE),
                PAGE_SIZE as usize * 2,
                Permissions::READ_WRITE,
                "first",
            )
            .unwrap();
        memory
            .map(
                GuestAddr(BASE + PAGE_SIZE * 2),
                PAGE_SIZE as usize,
                Permissions::READ_WRITE,
                "second",
            )
            .unwrap();

        BARRIER_LOG.lock().unwrap().clear();
        set_write_barrier(Some(barrier_probe));
        memory.write_u8(GuestAddr(BASE), 0xAA).unwrap();
        memory.write_u16(GuestAddr(BASE + 2), 0x1234).unwrap();
        memory.write_u32(GuestAddr(BASE + 4), 0xdead_beef).unwrap();
        memory
            .write(GuestAddr(BASE + 0x100), &[1, 2, 3, 4])
            .unwrap();
        // Crossing the region boundary exercises the segmented slow path.
        memory
            .write(GuestAddr(BASE + PAGE_SIZE * 2 - 2), &[9; 4])
            .unwrap();
        // Reads must stay invisible to the barrier.
        let _ = memory.read_u32(GuestAddr(BASE + 4)).unwrap();
        set_write_barrier(None);

        let log = BARRIER_LOG.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                (BASE, 1),
                (BASE + 2, 2),
                (BASE + 4, 4),
                (BASE + 0x100, 4),
                (BASE + PAGE_SIZE * 2 - 2, 4),
            ]
        );
    }

    #[test]
    fn write_barrier_skips_failed_writes_and_stays_silent_when_cleared() {
        let _guard = BARRIER_TEST_GUARD.lock().unwrap();
        let mut memory = GuestMemory::new();
        const BASE: u32 = 0x7E10_0000;
        memory
            .map(GuestAddr(BASE), 0x100, Permissions::READ, "ro")
            .unwrap();

        BARRIER_LOG.lock().unwrap().clear();
        set_write_barrier(Some(barrier_probe));
        let denied = memory.write_u32(GuestAddr(BASE), 1);
        let unmapped = memory.write_u32(GuestAddr(BASE + 0x8000), 1);
        set_write_barrier(None);
        assert!(denied.is_err());
        assert!(unmapped.is_err());
        assert!(BARRIER_LOG.lock().unwrap().is_empty());

        // With the barrier cleared, writes succeed without any notification.
        memory
            .add_permissions(GuestAddr(BASE), 0x100, Permissions::WRITE)
            .unwrap();
        memory.write_u32(GuestAddr(BASE), 7).unwrap();
        assert!(BARRIER_LOG.lock().unwrap().is_empty());
    }

    #[test]
    fn structure_epoch_advances_on_mapping_changes_only() {
        let mut memory = GuestMemory::new();
        let initial = memory.structure_epoch();

        memory
            .map(
                GuestAddr(0x1000),
                PAGE_SIZE as usize,
                Permissions::READ_WRITE,
                "a",
            )
            .unwrap();
        let after_map = memory.structure_epoch();
        assert!(after_map > initial);

        // Ordinary data traffic leaves the epoch alone.
        memory.write_u32(GuestAddr(0x1000), 1).unwrap();
        let _ = memory.read_u32(GuestAddr(0x1000)).unwrap();
        assert_eq!(memory.structure_epoch(), after_map);

        memory
            .add_permissions(GuestAddr(0x1000), PAGE_SIZE as usize, Permissions::EXECUTE)
            .unwrap();
        let after_perm = memory.structure_epoch();
        assert!(after_perm > after_map);

        memory.unmap(GuestAddr(0x1000), PAGE_SIZE as usize).unwrap();
        assert!(memory.structure_epoch() > after_perm);

        // Failed structural operations must not advance the epoch.
        let before_failed = memory.structure_epoch();
        assert!(memory.unmap(GuestAddr(0x5000), PAGE_SIZE as usize).is_err());
        assert!(
            memory
                .map(GuestAddr(0), 0, Permissions::READ_WRITE, "empty")
                .is_err()
        );
        assert_eq!(memory.structure_epoch(), before_failed);
    }
}
