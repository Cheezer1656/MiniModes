use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chunkedge::prelude::*;

use crate::{Bounds, MapSpawn};

/// Section coordinates: block coordinates divided by 16 (floored).
pub type SectionPos = [i32; 3];

const SECTION_VOLUME: usize = 16 * 16 * 16;

#[derive(Clone)]
pub(crate) enum Section {
    /// Every block is the same state.
    Uniform(BlockState),
    /// Indexed `y * 256 + z * 16 + x`.
    Dense(Box<[BlockState; SECTION_VOLUME]>),
}

impl Section {
    fn get(&self, i: usize) -> BlockState {
        match self {
            Self::Uniform(s) => *s,
            Self::Dense(blocks) => blocks[i],
        }
    }

    /// Returns the previous state.
    fn set(&mut self, i: usize, state: BlockState) -> BlockState {
        match self {
            Self::Uniform(s) if *s == state => state,
            Self::Uniform(s) => {
                let old = *s;
                let mut blocks = Box::new([old; SECTION_VOLUME]);
                blocks[i] = state;
                *self = Self::Dense(blocks);
                old
            }
            Self::Dense(blocks) => std::mem::replace(&mut blocks[i], state),
        }
    }

    pub(crate) fn blocks(&self) -> impl Iterator<Item = BlockState> + '_ {
        (0..SECTION_VOLUME).map(|i| self.get(i))
    }

    pub(crate) fn from_blocks(blocks: Vec<BlockState>) -> Self {
        debug_assert_eq!(blocks.len(), SECTION_VOLUME);
        if blocks.iter().all(|b| *b == blocks[0]) {
            Self::Uniform(blocks[0])
        } else {
            Self::Dense(blocks.into_boxed_slice().try_into().expect("section length"))
        }
    }

    fn is_air(&self) -> bool {
        match self {
            Self::Uniform(s) => s.is_air(),
            Self::Dense(blocks) => blocks.iter().all(|b| b.is_air()),
        }
    }
}

fn split(p: [i32; 3]) -> (SectionPos, usize) {
    let sp = p.map(|c| c.div_euclid(16));
    let [x, y, z] = p.map(|c| c.rem_euclid(16) as usize);
    (sp, y * 256 + z * 16 + x)
}

/// An instance's blocks, kept beside its `ChunkLayer`. Missing sections are air.
///
/// Sections are copy-on-write, so `clone()` is cheap: autosave snapshots the
/// mirror on the main thread and encodes the copy elsewhere.
#[derive(Clone)]
pub struct BlockMirror {
    pub bounds: Bounds,
    pub spawn: MapSpawn,
    pub(crate) sections: HashMap<SectionPos, Arc<Section>>,
    dirty: HashSet<SectionPos>,
}

impl BlockMirror {
    pub fn new(bounds: Bounds, spawn: MapSpawn) -> Self {
        Self {
            bounds,
            spawn,
            sections: HashMap::new(),
            dirty: HashSet::new(),
        }
    }

    /// A new map: a grass floor one block below `floor_y + 1`, dirt and stone
    /// beneath, spawn in the middle.
    pub fn flat(bounds: Bounds, floor_y: i32) -> Self {
        let spawn = MapSpawn {
            pos: [0.5, f64::from(floor_y) + 1.0, 0.5],
            yaw: 0.0,
            pitch: 0.0,
        };
        let mut mirror = Self::new(bounds, spawn);
        let fill = |m: &mut Self, y: i32, state| {
            for z in bounds.min[2]..bounds.max[2] {
                for x in bounds.min[0]..bounds.max[0] {
                    m.set([x, y, z], state);
                }
            }
        };
        fill(&mut mirror, bounds.min[1], BlockState::BEDROCK);
        for y in bounds.min[1] + 1..floor_y - 2 {
            fill(&mut mirror, y, BlockState::STONE);
        }
        for y in (floor_y - 2).max(bounds.min[1] + 1)..floor_y {
            fill(&mut mirror, y, BlockState::DIRT);
        }
        fill(&mut mirror, floor_y, BlockState::GRASS_BLOCK);
        mirror.dirty.clear();
        mirror
    }

    pub(crate) fn from_sections(bounds: Bounds, spawn: MapSpawn, sections: HashMap<SectionPos, Section>) -> Self {
        Self {
            bounds,
            spawn,
            sections: sections.into_iter().map(|(p, s)| (p, Arc::new(s))).collect(),
            dirty: HashSet::new(),
        }
    }

    pub fn get(&self, p: [i32; 3]) -> BlockState {
        let (sp, i) = split(p);
        self.sections.get(&sp).map_or(BlockState::AIR, |s| s.get(i))
    }

    /// Sets a block inside the bounds, returning the previous state. Positions
    /// outside the bounds are ignored and return `None`.
    pub fn set(&mut self, p: [i32; 3], state: BlockState) -> Option<BlockState> {
        if !self.bounds.contains(p) {
            return None;
        }
        let (sp, i) = split(p);
        let section = self.sections.entry(sp).or_insert_with(|| Arc::new(Section::Uniform(BlockState::AIR)));
        if section.get(i) == state {
            return Some(state);
        }
        let old = Arc::make_mut(section).set(i, state);
        if old != state {
            self.dirty.insert(sp);
        }
        Some(old)
    }

    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Sections changed since the last call.
    pub fn take_dirty(&mut self) -> HashSet<SectionPos> {
        std::mem::take(&mut self.dirty)
    }

    /// Marks sections dirty again (e.g. after a failed save).
    pub fn mark_dirty(&mut self, sections: impl IntoIterator<Item = SectionPos>) {
        self.dirty.extend(sections);
    }

    /// Blocks inside `region` where `other` differs from this mirror, with
    /// `other`'s state: what makes this mirror match it there.
    pub fn differences(&self, other: &BlockMirror, region: Bounds) -> Vec<([i32; 3], BlockState)> {
        let Some(region) = region.intersect(&self.bounds) else {
            return Vec::new();
        };
        region
            .iter()
            .filter_map(|p| {
                let theirs = other.get(p);
                (self.get(p) != theirs).then_some((p, theirs))
            })
            .collect()
    }

    /// Non-air sections, for encoding.
    pub(crate) fn solid_sections(&self) -> impl Iterator<Item = (&SectionPos, &Section)> {
        self.sections.iter().filter(|(_, s)| !s.is_air()).map(|(p, s)| (p, &**s))
    }

    /// Builds one chunk column for a layer whose dimension starts at `min_y`
    /// and is `height` blocks tall. Columns outside the bounds come back empty.
    pub fn chunk(&self, column: [i32; 2], min_y: i32, height: u32) -> UnloadedChunk {
        let mut chunk = UnloadedChunk::with_height(height);
        for sy in min_y.div_euclid(16)..(min_y + height as i32).div_euclid(16) {
            let Some(section) = self.sections.get(&[column[0], sy, column[1]]) else {
                continue;
            };
            let chunk_sect = (sy - min_y.div_euclid(16)) as u32;
            match &**section {
                Section::Uniform(state) => chunk.fill_block_state_section(chunk_sect, *state),
                Section::Dense(blocks) => {
                    for (i, state) in blocks.iter().enumerate() {
                        if !state.is_air() {
                            let (x, z, y) = (i % 16, (i / 16) % 16, i / 256);
                            chunk.set_block_state(x as u32, chunk_sect * 16 + y as u32, z as u32, *state);
                        }
                    }
                }
            }
        }
        chunk
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const MIN_Y: i32 = -64;
    const HEIGHT: u32 = 384;

    /// A tiny deterministic PRNG so the test needs no dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: u64) -> i64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n) as i64
        }
    }

    #[test]
    fn mirror_matches_chunks_after_random_edits() {
        let bounds = Bounds::square(48);
        let mut mirror = BlockMirror::flat(bounds, 64);
        let mut chunks: HashMap<[i32; 2], UnloadedChunk> =
            bounds.chunk_columns().map(|c| (c, mirror.chunk(c, MIN_Y, HEIGHT))).collect();

        let palette = [BlockState::AIR, BlockState::STONE, BlockState::GLASS, BlockState::OAK_PLANKS];
        let mut rng = Lcg(7);
        for _ in 0..20_000 {
            let p = [rng.next(60) as i32 - 30, rng.next(80) as i32 + 30, rng.next(60) as i32 - 30];
            let state = palette[rng.next(4) as usize];
            if mirror.set(p, state).is_some() {
                let column = [p[0].div_euclid(16), p[2].div_euclid(16)];
                let chunk = chunks.get_mut(&column).unwrap();
                chunk.set_block_state(p[0].rem_euclid(16) as u32, (p[1] - MIN_Y) as u32, p[2].rem_euclid(16) as u32, state);
            } else {
                assert!(!bounds.contains(p));
            }
        }

        for (column, expected) in &chunks {
            let rebuilt = mirror.chunk(*column, MIN_Y, HEIGHT);
            for y in 0..HEIGHT {
                for z in 0..16 {
                    for x in 0..16 {
                        assert_eq!(
                            rebuilt.block_state(x, y, z),
                            expected.block_state(x, y, z),
                            "column {column:?} at {x},{y},{z}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tracks_dirty_sections_and_ignores_out_of_bounds() {
        let mut mirror = BlockMirror::flat(Bounds::square(32), 64);
        assert!(!mirror.is_dirty());
        assert_eq!(mirror.set([0, 64, 0], BlockState::GRASS_BLOCK), Some(BlockState::GRASS_BLOCK));
        assert!(!mirror.is_dirty(), "no-op writes aren't changes");
        assert_eq!(mirror.set([0, 65, 0], BlockState::STONE), Some(BlockState::AIR));
        assert_eq!(mirror.set([16, 300, 0], BlockState::STONE), None);
        assert_eq!(mirror.take_dirty(), HashSet::from([[0, 4, 0]]));
        assert_eq!(mirror.get([0, 65, 0]), BlockState::STONE);
        assert_eq!(mirror.get([0, 1, 0]), BlockState::STONE);
        assert_eq!(mirror.get([0, 0, 0]), BlockState::BEDROCK);
    }

    #[test]
    fn snapshots_are_independent() {
        let mut mirror = BlockMirror::flat(Bounds::square(32), 64);
        let snapshot = mirror.clone();
        mirror.set([1, 64, 1], BlockState::GLASS);
        assert_eq!(snapshot.get([1, 64, 1]), BlockState::GRASS_BLOCK);
        assert_eq!(mirror.get([1, 64, 1]), BlockState::GLASS);
    }

    #[test]
    fn differences_inside_a_region() {
        let bounds = Bounds::square(32);
        let live = BlockMirror::flat(bounds, 10);
        let mut published = live.clone();
        published.set([1, 11, 1], BlockState::STONE);
        published.set([5, 11, 5], BlockState::STONE);
        published.set([1, 10, 1], BlockState::AIR);
        let region = Bounds::from_corners([0, 0, 0], [2, 20, 2]);
        let mut diff = live.differences(&published, region);
        diff.sort_by_key(|(p, _)| *p);
        assert_eq!(diff, [([1, 10, 1], BlockState::AIR), ([1, 11, 1], BlockState::STONE)]);
        assert!(live.differences(&published, Bounds::from_corners([100, 0, 0], [101, 1, 1])).is_empty());
    }
}
