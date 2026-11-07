use chunkedge::interact_item::InteractItemMessage;
use chunkedge::inventory::{ClickMode, ClickSlotMessage, HeldItem};
use chunkedge::prelude::*;

use crate::{UiAction, UiSource};

/// First hotbar slot in the player inventory window.
const HOTBAR_START: u16 = 36;

/// Locked hotbar items. Inserting this makes the player's inventory read-only
/// and replaces the hotbar contents.
#[derive(Component, Clone)]
pub struct Hotbar<A: Send + Sync + 'static> {
    pub slots: Vec<HotbarSlot<A>>,
}

#[derive(Clone)]
pub struct HotbarSlot<A> {
    /// Hotbar index, 0..=8.
    pub index: u8,
    pub item: ItemStack,
    pub action: A,
}

impl<A: Send + Sync + 'static> Hotbar<A> {
    fn action_at(&self, inventory_slot: u16) -> Option<&A> {
        self.slots
            .iter()
            .find(|s| HOTBAR_START + u16::from(s.index) == inventory_slot)
            .map(|s| &s.action)
    }
}

pub(crate) fn apply_hotbars<A: Send + Sync + 'static>(
    mut players: Query<(&Hotbar<A>, &mut Inventory), Changed<Hotbar<A>>>,
) {
    for (hotbar, mut inventory) in &mut players {
        inventory.readonly = true;
        for idx in 0..9 {
            inventory.set_slot(HOTBAR_START + idx, ItemStack::EMPTY);
        }
        for slot in &hotbar.slots {
            if slot.index < 9 {
                inventory.set_slot(HOTBAR_START + u16::from(slot.index), slot.item.clone());
            }
        }
    }
}

pub(crate) fn handle_hotbar_use<A: Clone + Send + Sync + 'static>(
    mut uses: MessageReader<InteractItemMessage>,
    mut clicks: MessageReader<ClickSlotMessage>,
    players: Query<(&Hotbar<A>, &HeldItem, Option<&OpenInventory>)>,
    mut actions: MessageWriter<UiAction<A>>,
) {
    for used in uses.read() {
        if used.hand != Hand::Main {
            continue;
        }
        let Ok((hotbar, held, _)) = players.get(used.client) else {
            continue;
        };
        if let Some(action) = hotbar.action_at(held.slot()) {
            actions.write(UiAction {
                client: used.client,
                action: action.clone(),
                source: UiSource::Hotbar { slot: held.slot() },
            });
        }
    }

    // Clicking a hotbar item from the player's own inventory screen (window 0).
    for click in clicks.read() {
        let Ok((hotbar, _, open)) = players.get(click.client) else {
            continue;
        };
        if open.is_some() || click.window_id.0 != 0 || click.mode != ClickMode::Click {
            continue;
        }
        let Ok(slot) = u16::try_from(click.slot_id) else {
            continue;
        };
        if let Some(action) = hotbar.action_at(slot) {
            actions.write(UiAction {
                client: click.client,
                action: action.clone(),
                source: UiSource::Hotbar { slot },
            });
        }
    }
}
