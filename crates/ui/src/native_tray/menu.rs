// Adapted from domenkozar/gpui-tray (Apache-2.0).

use std::collections::HashMap;

use crate::native_tray::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MenuSnapshot {
    pub(crate) generation: u64,
    pub(crate) items: Vec<NativeMenuItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeMenuItem {
    Separator,
    Item {
        id: MenuItemId,
        label: String,
        checked: bool,
        enabled: bool,
    },
    Submenu {
        label: String,
        enabled: bool,
        items: Vec<Self>,
    },
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub(crate) struct MenuItemId(pub(crate) u32);

pub(crate) struct ActionTable {
    pub(crate) generation: u64,
    actions: HashMap<MenuItemId, Box<dyn gpui_kit::Action>>,
}

impl ActionTable {
    pub(crate) fn empty(generation: u64) -> Self {
        Self {
            generation,
            actions: HashMap::new(),
        }
    }

    pub(crate) fn action(&self, id: MenuItemId) -> Option<Box<dyn gpui_kit::Action>> {
        self.actions.get(&id).map(|action| action.boxed_clone())
    }
}

pub(crate) fn compile_menu(
    generation: u64,
    items: Vec<gpui_kit::MenuItem>,
) -> Result<(MenuSnapshot, ActionTable)> {
    let mut next_id = 1_u32;
    let mut actions = HashMap::new();
    let items = compile_items(items, &mut next_id, &mut actions)?;

    Ok((
        MenuSnapshot { generation, items },
        ActionTable {
            generation,
            actions,
        },
    ))
}

fn compile_items(
    items: Vec<gpui_kit::MenuItem>,
    next_id: &mut u32,
    actions: &mut HashMap<MenuItemId, Box<dyn gpui_kit::Action>>,
) -> Result<Vec<NativeMenuItem>> {
    items
        .into_iter()
        .map(|item| {
            let checked = item.is_checked();
            let enabled = !item.is_disabled();
            match item {
                gpui_kit::MenuItem::Separator => Ok(NativeMenuItem::Separator),
                gpui_kit::MenuItem::Submenu(menu) => Ok(NativeMenuItem::Submenu {
                    label: menu.name.to_string(),
                    enabled: !menu.disabled,
                    items: compile_items(menu.items, next_id, actions)?,
                }),
                gpui_kit::MenuItem::Action { name, action, .. } => {
                    let id = MenuItemId(*next_id);
                    *next_id = next_id.checked_add(1).ok_or_else(|| {
                        Error::native_message("tray menu contains more than u32::MAX actions")
                    })?;
                    actions.insert(id, action);
                    Ok(NativeMenuItem::Item {
                        id,
                        label: name.to_string(),
                        checked,
                        enabled,
                    })
                }
                gpui_kit::MenuItem::SystemMenu(_) => Err(Error::UnsupportedMenuItem("SystemMenu")),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro.
    use super::{MenuItemId, NativeMenuItem, compile_menu};

    gpui_kit::actions!(native_tray_menu_test, [First, Second]);

    #[test]
    fn compiles_nested_menu_and_actions() {
        let items = vec![
            gpui_kit::MenuItem::action("First", First).checked(true),
            gpui_kit::MenuItem::separator(),
            gpui_kit::MenuItem::submenu(
                gpui_kit::Menu::new("Nested")
                    .items([gpui_kit::MenuItem::action("Second", Second).disabled(true)])
                    .disabled(true),
            ),
        ];

        let (snapshot, table) = compile_menu(7, items).unwrap();
        assert_eq!(snapshot.generation, 7);
        assert_eq!(table.generation, 7);
        assert!(table.action(MenuItemId(1)).is_some());
        assert!(table.action(MenuItemId(2)).is_some());
        assert!(table.action(MenuItemId(3)).is_none());
        let NativeMenuItem::Item {
            checked, enabled, ..
        } = &snapshot.items[0]
        else {
            panic!("first compiled item is not an action");
        };
        assert!(*checked);
        assert!(*enabled);
        let NativeMenuItem::Submenu { enabled, .. } = &snapshot.items[2] else {
            panic!("third compiled item is not a submenu");
        };
        assert!(!enabled, "disabled submenu must compile as disabled");
    }
}
