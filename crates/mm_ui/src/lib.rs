//! Server-side UI built from vanilla primitives: read-only chest menus,
//! locked hotbar items and clickable NPCs. Every interaction is reported as a
//! [`UiAction<A>`] carrying the app's own action type `A`.

mod hotbar;
mod item;
mod menu;
mod npc;

use std::marker::PhantomData;

use chunkedge::prelude::*;
pub use hotbar::{Hotbar, HotbarSlot};
pub use item::icon;
pub use menu::{ActiveMenu, Menu, MenuCommandsExt};
pub use npc::{NpcAction, NpcSpec, NpcTeam, spawn_npc};

/// An interaction a player made with a UI element.
#[derive(Message, Clone, Debug)]
pub struct UiAction<A: Clone + Send + Sync + 'static> {
    pub client: Entity,
    pub action: A,
    pub source: UiSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiSource {
    /// A menu slot was clicked. `button` 0 is left click, 1 is right click.
    Menu { slot: u16, button: i8, shift: bool },
    /// A hotbar item was used (right-clicked or clicked in the inventory screen).
    Hotbar { slot: u16 },
    /// An NPC was clicked.
    Npc(Entity),
}

/// Registers menu, hotbar and NPC handling for one action type.
pub struct UiPlugin<A>(PhantomData<A>);

impl<A> Default for UiPlugin<A> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// UI input handlers run in this set during `Update`; read [`UiAction`]s after it.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UiInputSet;

impl<A: Clone + Send + Sync + 'static> Plugin for UiPlugin<A> {
    fn build(&self, app: &mut App) {
        app.add_message::<UiAction<A>>().add_systems(
            Update,
            (
                menu::cleanup_closed_menus::<A>,
                menu::handle_menu_clicks::<A>,
                hotbar::apply_hotbars::<A>,
                hotbar::handle_hotbar_use::<A>,
                npc::handle_npc_clicks::<A>,
            )
                .chain()
                .in_set(UiInputSet),
        );
        npc::build(app);
    }
}
