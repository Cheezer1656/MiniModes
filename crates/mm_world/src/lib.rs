//! Game maps: the `.mmw` file format, a content-addressed store for it, and
//! [`BlockMirror`], the in-memory copy of an instance's blocks that saves and
//! (later) scripts read from.

mod codec;
mod mirror;
mod state;
mod store;

use serde::{Deserialize, Serialize};

pub use codec::{MapFile, decode, encode};
pub use mirror::{BlockMirror, SectionPos};
pub use state::{parse_state, state_name};
pub use store::{MapHash, WorldStore};

/// The vertical build range of every map.
pub const MIN_Y: i32 = 0;
pub const MAX_Y: i32 = 256;

/// An axis-aligned box of blocks: `min` inclusive, `max` exclusive.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bounds {
    pub min: [i32; 3],
    pub max: [i32; 3],
}

impl Bounds {
    /// A `size`×`size` square centered on the origin, over the full build height.
    pub fn square(size: i32) -> Self {
        let half = size / 2;
        Self {
            min: [-half, MIN_Y, -half],
            max: [size - half, MAX_Y, size - half],
        }
    }

    /// The box spanned by two corners, both inclusive.
    pub fn from_corners(a: [i32; 3], b: [i32; 3]) -> Self {
        Self {
            min: [0, 1, 2].map(|i| a[i].min(b[i])),
            max: [0, 1, 2].map(|i| a[i].max(b[i]) + 1),
        }
    }

    pub fn contains(&self, p: [i32; 3]) -> bool {
        (0..3).all(|i| p[i] >= self.min[i] && p[i] < self.max[i])
    }

    /// The overlap of two boxes, if any.
    pub fn intersect(&self, other: &Bounds) -> Option<Bounds> {
        let b = Bounds {
            min: [0, 1, 2].map(|i| self.min[i].max(other.min[i])),
            max: [0, 1, 2].map(|i| self.max[i].min(other.max[i])),
        };
        (0..3).all(|i| b.min[i] < b.max[i]).then_some(b)
    }

    pub fn size(&self) -> [i32; 3] {
        [0, 1, 2].map(|i| self.max[i] - self.min[i])
    }

    pub fn volume(&self) -> u64 {
        self.size().iter().map(|&s| s.max(0) as u64).product()
    }

    /// Every block position, x fastest, then z, then y.
    pub fn iter(&self) -> impl Iterator<Item = [i32; 3]> + use<> {
        let b = *self;
        (b.min[1]..b.max[1]).flat_map(move |y| {
            (b.min[2]..b.max[2]).flat_map(move |z| (b.min[0]..b.max[0]).map(move |x| [x, y, z]))
        })
    }

    /// Chunk columns overlapping the box.
    pub fn chunk_columns(&self) -> impl Iterator<Item = [i32; 2]> + use<> {
        let (x0, x1) = (self.min[0].div_euclid(16), (self.max[0] - 1).div_euclid(16));
        let (z0, z1) = (self.min[2].div_euclid(16), (self.max[2] - 1).div_euclid(16));
        (z0..=z1).flat_map(move |z| (x0..=x1).map(move |x| [x, z]))
    }
}

/// Where players appear in a map.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct MapSpawn {
    pub pos: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_geometry() {
        let b = Bounds::square(128);
        assert_eq!(b.min, [-64, 0, -64]);
        assert_eq!(b.size(), [128, 256, 128]);
        assert!(b.contains([63, 255, -64]) && !b.contains([64, 0, 0]) && !b.contains([0, 256, 0]));
        assert_eq!(b.chunk_columns().count(), 64);

        let sel = Bounds::from_corners([5, 70, -3], [2, 64, 1]);
        assert_eq!((sel.min, sel.max), ([2, 64, -3], [6, 71, 2]));
        assert_eq!(sel.volume(), 4 * 7 * 5);
        assert_eq!(sel.iter().count() as u64, sel.volume());

        let clipped = Bounds::from_corners([60, 250, 0], [70, 300, 0]).intersect(&b).unwrap();
        assert_eq!((clipped.min, clipped.max), ([60, 250, 0], [64, 256, 1]));
        assert!(Bounds::from_corners([100, 0, 0], [110, 0, 0]).intersect(&b).is_none());
    }
}
