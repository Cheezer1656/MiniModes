//! Block states as stable text (`oak_stairs[facing=north,half=bottom]`), so
//! saved maps don't depend on one Minecraft version's numeric state ids.

use chunkedge::prelude::*;

pub fn state_name(state: BlockState) -> String {
    let kind = state.to_kind();
    let mut s = kind.to_str().to_owned();
    let props = kind.props();
    if !props.is_empty() {
        s.push('[');
        for (i, &p) in props.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(p.to_str());
            s.push('=');
            s.push_str(state.get(p).map_or("", |v| v.to_str()));
        }
        s.push(']');
    }
    s
}

/// Parses `name` or `name[prop=value,...]`, with an optional `minecraft:`
/// prefix. Unlisted properties keep the block's defaults.
pub fn parse_state(text: &str) -> Option<BlockState> {
    let text = text.trim();
    let (name, props) = match text.split_once('[') {
        Some((name, rest)) => (name, Some(rest.strip_suffix(']')?)),
        None => (text, None),
    };
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    let kind = BlockKind::from_str(name)?;
    let mut state = kind.to_state();
    for pair in props.into_iter().flat_map(|p| p.split(',')).filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=')?;
        let k = PropName::from_str(k.trim())?;
        if !kind.props().contains(&k) {
            return None;
        }
        state = state.set(k, PropValue::from_str(v.trim())?);
    }
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for state in [
            BlockState::AIR,
            BlockState::STONE,
            BlockState::OAK_STAIRS.set(PropName::Facing, PropValue::East),
            BlockState::OAK_LOG.set(PropName::Axis, PropValue::X),
        ] {
            let name = state_name(state);
            assert_eq!(parse_state(&name), Some(state), "{name}");
        }
        assert_eq!(state_name(BlockState::STONE), "stone");
        assert_eq!(parse_state("minecraft:stone"), Some(BlockState::STONE));
        assert_eq!(
            parse_state("oak_log[axis=z]"),
            Some(BlockState::OAK_LOG.set(PropName::Axis, PropValue::Z))
        );
        assert_eq!(parse_state("stone[axis=z]"), None);
        assert_eq!(parse_state("not_a_block"), None);
        assert_eq!(parse_state("oak_log[axis=sideways]"), None);
    }
}
