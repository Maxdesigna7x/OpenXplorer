// SPDX-License-Identifier: AGPL-3.0-only
//! Window actions for the folders shown inside the drive rows.

use gtk::prelude::*;
use ox_core::settings::FolderTreeOptions;

use super::actions::{text_action, toggle_action};
use super::file_ops::FileCommand;

use super::preferences::Preference;
use super::window_action::WindowAction;
use super::BrowserWindow;

pub(super) fn tree_key_bindings() -> impl Iterator<Item = (String, &'static str)> {
    [
        ("tree-open", "KP_Add"),
        ("tree-close", "KP_Subtract"),
        ("tree-open-all", "KP_Multiply"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
}

impl BrowserWindow {
    pub(super) fn install_folder_tree(&self) {
        let options = self.context().settings_data().preferences.folder_tree;
        self.sidebar().set_branch_options(options);
        self.add_action_entries([
            toggle_action(WindowAction::FolderTree, options.shown, |window, shown| {
                let mut options = window.sidebar().branch_options();
                options.shown = shown;
                window.change_folder_tree(options);
            }),
            text_action(WindowAction::FolderTreeOption, |window, name| {
                let mut options = window.sidebar().branch_options();
                let option = match name {
                    "show-hidden" => &mut options.show_hidden,
                    "limit-to-home" => &mut options.limit_to_home,
                    "auto-scroll" => &mut options.auto_scroll,
                    _ => return,
                };
                *option = !*option;
                window.change_folder_tree(options);
            }),
        ]);
    }

    fn change_folder_tree(&self, options: FolderTreeOptions) {
        self.sidebar().set_branch_options(options);
        self.save_preference(Preference::FolderTree(options));
    }

    pub(super) fn reveal_in_navigation_pane(&self) {
        let sidebar = self.sidebar();
        if sidebar.is_mapped() {
            sidebar.reveal_location();
        }
    }

    pub(super) fn can_paste_into(&self, uri: &str) -> bool {
        use gtk::subclass::prelude::*;
        let facts = self.facts_for_command(FileCommand::PasteInto);
        facts.clipboard.is_pasteable()
            && !facts.is_busy
            && self.imp().locations.borrow().is_writable_location(uri)
    }
}
