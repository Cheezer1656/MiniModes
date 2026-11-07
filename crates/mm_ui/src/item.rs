use chunkedge::item::ItemComponent;
use chunkedge::prelude::*;
use chunkedge::protocol::IntoTextComponent;

/// A display item with a name and lore lines, as used for menu buttons.
pub fn icon<'a>(
    kind: ItemKind,
    name: impl IntoText<'a>,
    lore: impl IntoIterator<Item = Text>,
) -> ItemStack {
    let name = name.into_cow_text().into_owned().not_italic();
    let lore = lore
        .into_iter()
        .map(|line| line.not_italic().into_text_component())
        .collect();
    ItemStack::new(kind, 1).with_components(vec![
        ItemComponent::ItemName(name.into_text_component()),
        ItemComponent::Lore(lore),
    ])
}
