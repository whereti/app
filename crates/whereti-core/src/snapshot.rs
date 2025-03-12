//! On-disk snapshot of the name index (PLAN.md §4.5).
//!
//! The file is the in-memory columns written one after the other, each with an
//! xxh3 checksum. Saving is atomic: write a temporary file, flush it to disk,
//! then rename it over the old one. Any problem on load (wrong version, bad
//! checksum, truncated file) is reported as an error, and the caller rescans:
//! a rescan takes seconds and is safer than repairing a file.
//!
//! Only compact indexes are saved (call `NameIndex::compact` first), so the
//! file never contains live-update overlay data.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use bytemuck::{Pod, Zeroable};
use xxhash_rust::xxh3::xxh3_64;

use crate::index::{NameIndex, VolumeInfo, NO_PARENT};

const MAGIC: &[u8; 8] = b"WHERETI\0";
const VERSION: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
enum Tag {
    Volumes = 1,
    Names,
    NameOff,
    Parent,
    Flags,
    Ext,
    Size,
    Mtime,
    Volume,
    NameRank,
    AltNames,
    AltOff,
    AltEntry,
    Exts,
    DirOrder,
    RecMap,
    Links,
    Exclusions,
}

const ORDER: [Tag; 18] = [
    Tag::Volumes,
    Tag::Names,
    Tag::NameOff,
    Tag::Parent,
    Tag::Flags,
    Tag::Ext,
    Tag::Size,
    Tag::Mtime,
    Tag::Volume,
    Tag::NameRank,
    Tag::AltNames,
    Tag::AltOff,
    Tag::AltEntry,
    Tag::Exts,
    Tag::DirOrder,
    Tag::RecMap,
    Tag::Links,
    Tag::Exclusions,
];

pub fn save(index: &NameIndex, path: &Path) -> io::Result<u64> {
    if !index.is_compact() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "index has live updates; compact() it before saving"));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");

    // Record maps are stored back to back; each volume knows its slice.
    let mut volumes = index.volumes.clone();
    let mut rec_map: Vec<u32> = Vec::new();
    for (v, map) in volumes.iter_mut().zip(&index.rec_maps) {
        v.rec_start = rec_map.len() as u64;
        v.rec_len = map.len() as u64;
        rec_map.extend_from_slice(map);
    }
    let mut links: Vec<u32> = Vec::new();
    let mut keys: Vec<&u32> = index.links.keys().collect();
    keys.sort_unstable();
    for k in keys {
        for &other in &index.links[k] {
            links.extend([*k, other]);
        }
    }
    let exts = index.exts.join("\0").into_bytes();
    let exclusions = index.exclusions.join("\0").into_bytes();

    {
        let file = File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&(ORDER.len() as u32).to_le_bytes())?;
        for tag in ORDER {
            let (elem, bytes): (u32, &[u8]) = match tag {
                Tag::Volumes => (size_of::<VolumeInfo>() as u32, bytemuck::cast_slice(&volumes)),
                Tag::Names => (1, &index.names),
                Tag::NameOff => (4, bytemuck::cast_slice(&index.name_off)),
                Tag::Parent => (4, bytemuck::cast_slice(&index.parent)),
                Tag::Flags => (2, bytemuck::cast_slice(&index.flags)),
                Tag::Ext => (2, bytemuck::cast_slice(&index.ext)),
                Tag::Size => (8, bytemuck::cast_slice(&index.size)),
                Tag::Mtime => (4, bytemuck::cast_slice(&index.mtime)),
                Tag::Volume => (1, &index.volume),
                Tag::NameRank => (4, bytemuck::cast_slice(&index.name_rank)),
                Tag::AltNames => (1, &index.alt_names),
                Tag::AltOff => (4, bytemuck::cast_slice(&index.alt_off)),
                Tag::AltEntry => (4, bytemuck::cast_slice(&index.alt_entry)),
                Tag::Exts => (1, &exts),
                Tag::DirOrder => (4, bytemuck::cast_slice(&index.dir_order)),
                Tag::RecMap => (4, bytemuck::cast_slice(&rec_map)),
                Tag::Links => (4, bytemuck::cast_slice(&links)),
                Tag::Exclusions => (1, &exclusions),
            };
            w.write_all(&(tag as u32).to_le_bytes())?;
            w.write_all(&elem.to_le_bytes())?;
            w.write_all(&(bytes.len() as u64 / elem as u64).to_le_bytes())?;
            w.write_all(&xxh3_64(bytes).to_le_bytes())?;
            w.write_all(bytes)?;
        }
        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?; // MoveFileExW(MOVEFILE_REPLACE_EXISTING)
    Ok(fs::metadata(path)?.len())
}

pub fn load(path: &Path) -> io::Result<NameIndex> {
    let file = File::open(path)?;
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("not a whereti index file"));
    }
    let version = read_u32(&mut r)?;
    if version != VERSION {
        return Err(bad(&format!("index format v{version}, expected v{VERSION} (rebuild needed)")));
    }
    let count = read_u32(&mut r)?;
    if count as usize != ORDER.len() {
        return Err(bad("unexpected section count"));
    }
    let mut idx = NameIndex::default();
    let mut rec_map: Vec<u32> = Vec::new();
    let mut links: Vec<u32> = Vec::new();
    for expected in ORDER {
        let tag = read_u32(&mut r)?;
        if tag != expected as u32 {
            return Err(bad("sections out of order"));
        }
        let elem = read_u32(&mut r)? as usize;
        let n = read_u64(&mut r)? as usize;
        let sum = read_u64(&mut r)?;
        match expected {
            Tag::Volumes => idx.volumes = read_vec(&mut r, n, elem, sum)?,
            Tag::Names => idx.names = read_vec(&mut r, n, elem, sum)?,
            Tag::NameOff => idx.name_off = read_vec(&mut r, n, elem, sum)?,
            Tag::Parent => idx.parent = read_vec(&mut r, n, elem, sum)?,
            Tag::Flags => idx.flags = read_vec(&mut r, n, elem, sum)?,
            Tag::Ext => idx.ext = read_vec(&mut r, n, elem, sum)?,
            Tag::Size => idx.size = read_vec(&mut r, n, elem, sum)?,
            Tag::Mtime => idx.mtime = read_vec(&mut r, n, elem, sum)?,
            Tag::Volume => idx.volume = read_vec(&mut r, n, elem, sum)?,
            Tag::NameRank => idx.name_rank = read_vec(&mut r, n, elem, sum)?,
            Tag::AltNames => idx.alt_names = read_vec(&mut r, n, elem, sum)?,
            Tag::AltOff => idx.alt_off = read_vec(&mut r, n, elem, sum)?,
            Tag::AltEntry => idx.alt_entry = read_vec(&mut r, n, elem, sum)?,
            Tag::Exts => idx.exts = read_strings(&mut r, n, elem, sum)?,
            Tag::DirOrder => idx.dir_order = read_vec(&mut r, n, elem, sum)?,
            Tag::RecMap => rec_map = read_vec(&mut r, n, elem, sum)?,
            Tag::Links => links = read_vec(&mut r, n, elem, sum)?,
            Tag::Exclusions => {
                idx.exclusions = read_strings(&mut r, n, elem, sum)?.into_iter().filter(|s| !s.is_empty()).collect();
            }
        }
    }
    for v in &idx.volumes {
        let (s, l) = (v.rec_start as usize, v.rec_len as usize);
        let slice = rec_map.get(s..s.checked_add(l).ok_or_else(|| bad("record map is invalid"))?);
        idx.rec_maps.push(slice.ok_or_else(|| bad("record map is invalid"))?.to_vec());
    }
    if !links.len().is_multiple_of(2) {
        return Err(bad("hard link table is invalid"));
    }
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for pair in links.chunks_exact(2) {
        map.entry(pair[0]).or_default().push(pair[1]);
    }
    idx.links = map;
    validate(&idx)?;
    // the noise list may have grown since the file was written
    crate::build::mark_noise(&mut idx);
    Ok(idx)
}

/// Cheap structural checks, so a damaged file can never cause out-of-bounds reads.
fn validate(idx: &NameIndex) -> io::Result<()> {
    let n = idx.parent.len();
    let same = [idx.flags.len(), idx.ext.len(), idx.size.len(), idx.mtime.len(), idx.volume.len(), idx.name_rank.len()];
    if same.iter().any(|&l| l != n) || idx.name_off.len() != n + 1 {
        return Err(bad("column lengths differ"));
    }
    if idx.name_off.windows(2).any(|w| w[0] >= w[1]) || *idx.name_off.last().unwrap_or(&0) as usize != idx.names.len() {
        return Err(bad("name offsets are invalid"));
    }
    if idx.alt_off.len() != idx.alt_entry.len() + 1
        || idx.alt_off.windows(2).any(|w| w[0] >= w[1])
        || *idx.alt_off.last().unwrap_or(&0) as usize != idx.alt_names.len()
    {
        return Err(bad("alt offsets are invalid"));
    }
    let in_range = |v: u32| v == NO_PARENT || (v as usize) < n;
    if !idx.parent.iter().all(|&p| in_range(p))
        || !idx.alt_entry.iter().all(|&e| (e as usize) < n)
        || !idx.dir_order.iter().all(|&e| (e as usize) < n)
        || !idx.rec_maps.iter().flatten().all(|&e| in_range(e))
        || !idx.links.iter().all(|(k, v)| (*k as usize) < n && v.iter().all(|&e| (e as usize) < n))
        || !idx.ext.iter().all(|&e| (e as usize) < idx.exts.len().max(1))
        || !idx.volume.iter().all(|&v| (v as usize) < idx.volumes.len())
    {
        return Err(bad("references out of range"));
    }
    let mut end = 0u32;
    for v in &idx.volumes {
        if v.first != end || v.end < v.first || v.end as usize > n {
            return Err(bad("volume table is invalid"));
        }
        end = v.end;
    }
    if end as usize != n {
        return Err(bad("volume table does not cover the index"));
    }
    Ok(())
}

fn read_strings(r: &mut impl Read, n: usize, elem: usize, sum: u64) -> io::Result<Vec<String>> {
    let raw: Vec<u8> = read_vec(r, n, elem, sum)?;
    let s = String::from_utf8(raw).map_err(|_| bad("text section is not UTF-8"))?;
    Ok(s.split('\0').map(str::to_string).collect())
}

fn read_vec<T: Pod + Zeroable>(r: &mut impl Read, n: usize, elem: usize, sum: u64) -> io::Result<Vec<T>> {
    if elem != size_of::<T>() {
        return Err(bad("unexpected element size"));
    }
    if n > (1 << 34) / elem.max(1) {
        return Err(bad("section too large"));
    }
    let mut v = vec![T::zeroed(); n];
    r.read_exact(bytemuck::cast_slice_mut(&mut v))?;
    if xxh3_64(bytemuck::cast_slice(&v)) != sum {
        return Err(bad("checksum mismatch (file damaged)"));
    }
    Ok(v)
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
