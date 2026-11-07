//! Declarative rules a script sets and the main thread enforces instantly,
//! with no round trip through the VM.

use std::collections::BTreeMap;

use chunkedge::prelude::*;
use mm_world::Bounds;

use crate::io::RegionId;

/// Unset fields inherit: a region's from the game-wide rules, the game-wide
/// ones from the defaults (nothing may be broken or placed).
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Rules {
    pub can_break: Option<bool>,
    pub can_place: Option<bool>,
    /// When set, only these blocks may be broken.
    pub breakable: Option<Vec<BlockKind>>,
    /// When set, only these blocks may be placed.
    pub placeable: Option<Vec<BlockKind>>,
}

/// The rules in force for one instance.
#[derive(Clone, Default, Debug)]
pub struct RuleSet {
    pub game: Rules,
    /// Later regions win where they overlap.
    pub regions: BTreeMap<RegionId, (Bounds, Rules)>,
}

impl RuleSet {
    /// The innermost value of `field` that applies at `pos`.
    fn lookup<'a, T>(&'a self, pos: [i32; 3], field: impl Fn(&'a Rules) -> Option<T>) -> Option<T> {
        self.regions
            .values()
            .rev()
            .filter(|(bounds, _)| bounds.contains(pos))
            .find_map(|(_, rules)| field(rules))
            .or_else(|| field(&self.game))
    }

    pub fn may_break(&self, pos: [i32; 3], block: BlockKind) -> bool {
        self.lookup(pos, |r| r.can_break).unwrap_or(false)
            && self
                .lookup(pos, |r| r.breakable.as_ref())
                .is_none_or(|allowed| allowed.contains(&block))
    }

    pub fn may_place(&self, pos: [i32; 3], block: BlockKind) -> bool {
        self.lookup(pos, |r| r.can_place).unwrap_or(false)
            && self
                .lookup(pos, |r| r.placeable.as_ref())
                .is_none_or(|allowed| allowed.contains(&block))
    }

    pub fn set(&mut self, region: Option<(RegionId, Bounds)>, rules: Rules) {
        match region {
            Some((id, bounds)) => {
                self.regions.insert(id, (bounds, rules));
            }
            None => self.game = rules,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_override_the_game_rules() {
        let mut rules = RuleSet::default();
        let p = [0, 64, 0];
        assert!(!rules.may_break(p, BlockKind::Stone), "nothing breaks by default");

        rules.set(
            None,
            Rules {
                can_break: Some(true),
                ..Rules::default()
            },
        );
        assert!(rules.may_break(p, BlockKind::Stone));
        assert!(!rules.may_place(p, BlockKind::Stone));

        let mine = Bounds::from_corners([-5, 60, -5], [5, 70, 5]);
        rules.set(
            Some((1, mine)),
            Rules {
                breakable: Some(vec![BlockKind::CoalOre]),
                ..Rules::default()
            },
        );
        assert!(!rules.may_break(p, BlockKind::Stone), "the mine only allows coal");
        assert!(rules.may_break(p, BlockKind::CoalOre), "can_break is inherited");
        assert!(rules.may_break([20, 64, 0], BlockKind::Stone), "outside the mine");

        let vault = Bounds::from_corners([0, 64, 0], [1, 64, 1]);
        rules.set(
            Some((2, vault)),
            Rules {
                can_break: Some(false),
                ..Rules::default()
            },
        );
        assert!(!rules.may_break(p, BlockKind::CoalOre), "the later region wins");
        rules.regions.remove(&2);
        assert!(rules.may_break(p, BlockKind::CoalOre));
    }
}
