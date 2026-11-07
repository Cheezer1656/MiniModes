//! `.mmw`: `MMW1` magic, then a zstd-compressed postcard [`MapFile`]. Block
//! states are a file-wide palette of [`state_name`]s; each non-air 16³
//! section stores its own palette of indices into it plus bit-packed blocks.

use std::collections::HashMap;

use anyhow::{Context, bail, ensure};
use chunkedge::prelude::*;
use serde::{Deserialize, Serialize};

use crate::mirror::Section;
use crate::{BlockMirror, Bounds, MapSpawn, parse_state, state_name};

const MAGIC: &[u8; 4] = b"MMW1";
const VERSION: u16 = 1;
const ZSTD_LEVEL: i32 = 6;
/// Decoding limits, so a corrupt file can't exhaust memory. A 512² map with
/// every section distinct stays well under both.
const MAX_SECTIONS: usize = 1 << 16;
const MAX_DECOMPRESSED: usize = 512 << 20;

#[derive(Serialize, Deserialize)]
pub struct MapFile {
    pub version: u16,
    pub bounds: Bounds,
    pub spawn: MapSpawn,
    palette: Vec<String>,
    sections: Vec<SectionRecord>,
    /// Reserved for signs, chests and other block entities.
    block_entities: Vec<BlockEntityRecord>,
}

#[derive(Serialize, Deserialize)]
struct SectionRecord {
    pos: [i32; 3],
    /// Indices into [`MapFile::palette`].
    palette: Vec<u32>,
    /// Bits per block; 0 when the section has a single state.
    bits: u8,
    /// Blocks in `y, z, x` order, `64 / bits` per word, low bits first.
    data: Vec<u64>,
}

#[derive(Serialize, Deserialize)]
struct BlockEntityRecord {
    pos: [i32; 3],
    nbt: Vec<u8>,
}

pub fn encode(mirror: &BlockMirror) -> anyhow::Result<Vec<u8>> {
    let mut palette: Vec<String> = Vec::new();
    let mut palette_ids: HashMap<BlockState, u32> = HashMap::new();
    let mut sections: Vec<_> = mirror.solid_sections().collect();
    // Deterministic output, so identical maps hash identically.
    sections.sort_by_key(|(pos, _)| [pos[1], pos[2], pos[0]]);

    let records = sections
        .into_iter()
        .map(|(pos, section)| {
            let mut local: Vec<u32> = Vec::new();
            let mut local_ids: HashMap<BlockState, u64> = HashMap::new();
            let indices: Vec<u64> = section
                .blocks()
                .map(|state| {
                    *local_ids.entry(state).or_insert_with(|| {
                        let global = *palette_ids.entry(state).or_insert_with(|| {
                            palette.push(state_name(state));
                            palette.len() as u32 - 1
                        });
                        local.push(global);
                        local.len() as u64 - 1
                    })
                })
                .collect();
            let bits = bits_for(local.len());
            SectionRecord {
                pos: *pos,
                palette: local,
                bits,
                data: pack(&indices, bits),
            }
        })
        .collect();

    let file = MapFile {
        version: VERSION,
        bounds: mirror.bounds,
        spawn: mirror.spawn,
        palette,
        sections: records,
        block_entities: Vec::new(),
    };
    let body = postcard::to_stdvec(&file)?;
    let mut out = MAGIC.to_vec();
    out.extend(zstd::bulk::compress(&body, ZSTD_LEVEL)?);
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> anyhow::Result<BlockMirror> {
    let body = bytes.strip_prefix(MAGIC).context("not a .mmw file")?;
    let body = zstd::bulk::decompress(body, MAX_DECOMPRESSED).context("decompressing map")?;
    let file: MapFile = postcard::from_bytes(&body).context("decoding map")?;
    ensure!(file.version == VERSION, "unsupported .mmw version {}", file.version);
    ensure!(file.sections.len() <= MAX_SECTIONS, "too many sections");

    // Unknown names (a block removed in a newer version) load as air.
    let palette: Vec<BlockState> = file
        .palette
        .iter()
        .map(|name| parse_state(name).unwrap_or(BlockState::AIR))
        .collect();

    let mut sections = HashMap::with_capacity(file.sections.len());
    for record in file.sections {
        let local: Vec<BlockState> = record
            .palette
            .iter()
            .map(|&i| palette.get(i as usize).copied().context("palette index out of range"))
            .collect::<anyhow::Result<_>>()?;
        ensure!(!local.is_empty(), "empty section palette");
        ensure!(record.bits == bits_for(local.len()), "bad bits per block");
        let indices = unpack(&record.data, record.bits, 4096)?;
        let blocks = indices
            .into_iter()
            .map(|i| local.get(i as usize).copied().context("block index out of range"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if sections.insert(record.pos, Section::from_blocks(blocks)).is_some() {
            bail!("duplicate section {:?}", record.pos);
        }
    }
    Ok(BlockMirror::from_sections(file.bounds, file.spawn, sections))
}

fn bits_for(len: usize) -> u8 {
    if len <= 1 { 0 } else { (usize::BITS - (len - 1).leading_zeros()) as u8 }
}

fn pack(values: &[u64], bits: u8) -> Vec<u64> {
    if bits == 0 {
        return Vec::new();
    }
    let per_word = 64 / bits as usize;
    values
        .chunks(per_word)
        .map(|chunk| chunk.iter().enumerate().fold(0, |w, (i, v)| w | (v << (i * bits as usize))))
        .collect()
}

fn unpack(words: &[u64], bits: u8, count: usize) -> anyhow::Result<Vec<u64>> {
    if bits == 0 {
        return Ok(vec![0; count]);
    }
    ensure!(bits <= 16, "bits per block too large");
    let per_word = 64 / bits as usize;
    ensure!(words.len() == count.div_ceil(per_word), "wrong data length");
    let mask = (1u64 << bits) - 1;
    Ok((0..count)
        .map(|i| (words[i / per_word] >> ((i % per_word) * bits as usize)) & mask)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_round_trips() {
        for len in [1usize, 2, 3, 5, 17, 300] {
            let bits = bits_for(len);
            let values: Vec<u64> = (0..4096).map(|i| (i * 7 % len) as u64).collect();
            assert_eq!(unpack(&pack(&values, bits), bits, 4096).unwrap(), values, "len {len}");
        }
    }

    #[test]
    fn map_round_trips_and_is_small() {
        let mut mirror = BlockMirror::flat(Bounds::square(128), 64);
        for i in 0..40 {
            mirror.set([i, 65 + i % 7, -i], BlockState::OAK_STAIRS.set(PropName::Facing, PropValue::West));
            mirror.set([-i, 70, i], BlockState::GLASS);
        }
        mirror.spawn.yaw = 90.0;
        let bytes = encode(&mirror).unwrap();
        assert!(bytes.len() < 8 * 1024, "flat 128² map is {} bytes", bytes.len());

        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.bounds, mirror.bounds);
        assert_eq!(decoded.spawn, mirror.spawn);
        for p in mirror.bounds.intersect(&Bounds::from_corners([-64, 0, -64], [63, 80, 63])).unwrap().iter() {
            assert_eq!(decoded.get(p), mirror.get(p), "{p:?}");
        }
        // Deterministic: re-encoding the decoded map gives identical bytes.
        assert_eq!(encode(&decoded).unwrap(), bytes);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"nope").is_err());
        assert!(decode(b"MMW1garbage").is_err());
    }
}
