use chunkedge::inventory::{ClickMode, ClickSlotMessage, ClientInventoryState};
use chunkedge::prelude::*;

use crate::{UiAction, UiSource};

/// A read-only chest menu: items plus the action each clickable slot fires.
pub struct Menu<A> {
    title: Text,
    kind: InventoryKind,
    slots: Vec<ItemStack>,
    actions: Vec<Option<A>>,
}

impl<A> Menu<A> {
    /// `rows` is clamped to 1..=6.
    pub fn new<'a>(title: impl IntoText<'a>, rows: u8) -> Self {
        let kind = match rows.clamp(1, 6) {
            1 => InventoryKind::Generic9x1,
            2 => InventoryKind::Generic9x2,
            3 => InventoryKind::Generic9x3,
            4 => InventoryKind::Generic9x4,
            5 => InventoryKind::Generic9x5,
            _ => InventoryKind::Generic9x6,
        };
        let size = kind.slot_count();
        Self {
            title: title.into_cow_text().into_owned(),
            kind,
            slots: vec![ItemStack::EMPTY; size],
            actions: (0..size).map(|_| None).collect(),
        }
    }

    pub fn slot_count(&self) -> u16 {
        self.slots.len() as u16
    }

    /// Places a decorative item. Out-of-range slots are ignored.
    pub fn item(&mut self, slot: u16, item: ItemStack) -> &mut Self {
        if let Some(s) = self.slots.get_mut(usize::from(slot)) {
            *s = item;
        }
        self
    }

    /// Places a clickable item. Out-of-range slots are ignored.
    pub fn button(&mut self, slot: u16, item: ItemStack, action: A) -> &mut Self {
        if let Some(a) = self.actions.get_mut(usize::from(slot)) {
            *a = Some(action);
            self.slots[usize::from(slot)] = item;
        }
        self
    }

    /// Fills every still-empty slot, e.g. with glass panes.
    pub fn fill_empty(&mut self, item: &ItemStack) -> &mut Self {
        self.fill_empty_where(item, |_| true)
    }

    /// Fills still-empty slots for which `pred(slot)` holds.
    pub fn fill_empty_where(&mut self, item: &ItemStack, pred: impl Fn(u16) -> bool) -> &mut Self {
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_empty() && pred(idx as u16) {
                *slot = item.clone();
            }
        }
        self
    }
}

/// Present on a player while one of our menus is open.
#[derive(Component)]
pub struct ActiveMenu<A: Send + Sync + 'static> {
    inventory: Entity,
    actions: Box<[Option<A>]>,
}

pub trait MenuCommandsExt {
    /// Opens `menu` for `player`, replacing whatever window they have open.
    fn open_menu<A: Clone + Send + Sync + 'static>(&mut self, player: Entity, menu: Menu<A>);
    fn close_menu(&mut self, player: Entity);
}

impl MenuCommandsExt for Commands<'_, '_> {
    fn open_menu<A: Clone + Send + Sync + 'static>(&mut self, player: Entity, menu: Menu<A>) {
        self.queue(move |world: &mut World| open_menu_now(world, player, menu));
    }

    fn close_menu(&mut self, player: Entity) {
        if let Ok(mut entity) = self.get_entity(player) {
            entity.remove::<OpenInventory>();
        }
    }
}

fn open_menu_now<A: Clone + Send + Sync + 'static>(world: &mut World, player: Entity, menu: Menu<A>) {
    let Ok(mut player_ref) = world.get_entity_mut(player) else {
        return;
    };
    if !player_ref.contains::<Client>() {
        return;
    }
    let previous = player_ref.take::<ActiveMenu<A>>();

    let mut inventory = Inventory::with_title(menu.kind, menu.title);
    inventory.readonly = true;
    for (idx, item) in menu.slots.into_iter().enumerate() {
        if !item.is_empty() {
            inventory.set_slot(idx as u16, item);
        }
    }
    let inventory = world.spawn(inventory).id();

    // Removing first makes ChunkEdge close the old window and open the new
    // one this tick (its close system runs before its open system).
    world
        .entity_mut(player)
        .remove::<OpenInventory>()
        .insert((
            OpenInventory::new(inventory),
            ActiveMenu {
                inventory,
                actions: menu.actions.into_boxed_slice(),
            },
        ));

    if let Some(previous) = previous {
        world.despawn(previous.inventory);
    }
}

/// Drops menus whose window was closed by the client or replaced by another window.
pub(crate) fn cleanup_closed_menus<A: Send + Sync + 'static>(
    mut commands: Commands,
    menus: Query<(Entity, &ActiveMenu<A>, Option<&OpenInventory>)>,
) {
    for (player, menu, open) in &menus {
        if open.is_none_or(|open| open.entity != menu.inventory) {
            commands.entity(menu.inventory).despawn();
            commands.entity(player).remove::<ActiveMenu<A>>();
        }
    }
}

pub(crate) fn handle_menu_clicks<A: Clone + Send + Sync + 'static>(
    mut clicks: MessageReader<ClickSlotMessage>,
    menus: Query<(&ActiveMenu<A>, &OpenInventory, &ClientInventoryState)>,
    mut actions: MessageWriter<UiAction<A>>,
) {
    for click in clicks.read() {
        let Ok((menu, open, state)) = menus.get(click.client) else {
            continue;
        };
        if open.entity != menu.inventory || click.window_id != state.window_id() {
            continue;
        }
        let shift = match click.mode {
            ClickMode::Click => false,
            ClickMode::ShiftClick => true,
            _ => continue,
        };
        let Ok(slot) = u16::try_from(click.slot_id) else {
            continue;
        };
        if let Some(Some(action)) = menu.actions.get(usize::from(slot)) {
            actions.write(UiAction {
                client: click.client,
                action: action.clone(),
                source: UiSource::Menu {
                    slot,
                    button: click.button,
                    shift,
                },
            });
        }
    }
}
