// SPDX-License-Identifier: AGPL-3.0-only
//! The folder tree following the active tab: its options, its top
//! folder, and the walk that opens the folders down to the one shown.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};
use ox_core::settings::FolderTreeOptions;

use super::{model, FolderTree};

impl FolderTree {
    /// The tree's options.
    pub(super) fn options(&self) -> FolderTreeOptions {
        self.imp().options.get()
    }

    /// Applies `options`: shows or hides the tree, and builds it again at
    /// the next [`Self::follow`] where its folders or its top change.
    pub(super) fn set_options(&self, options: FolderTreeOptions) {
        let old = self.imp().options.replace(options);
        self.set_visible(options.shown && self.imp().shown.borrow().is_some());
        if old.show_hidden != options.show_hidden || old.limit_to_home != options.limit_to_home {
            self.imp().root.replace(None);
        }
    }

    /// Discards old directory lists before an explicit folder refresh.
    pub(in crate::window) fn invalidate(&self) {
        self.imp().root.replace(None);
    }

    /// Follows the active tab, using its mounted volume as the top when one applies.
    pub(super) fn follow(
        &self,
        uri: Option<&str>,
        home: &str,
        mount: Option<gio::File>,
        title_of: impl Fn(&str) -> String,
    ) {
        let imp = self.imp();
        let shown = uri.map(gio::File::for_uri);
        let previous = imp.shown.replace(shown.clone());
        let Some(selection) = imp.selection.get() else {
            return;
        };
        let options = imp.options.get();
        self.set_visible(options.shown && shown.is_some());
        let Some(shown) = shown.filter(|_| options.shown) else {
            self.imp().walk.set(imp.walk.get() + 1);
            selection.set_selected(gtk::INVALID_LIST_POSITION);
            return;
        };
        let root =
            mount.unwrap_or_else(|| tree_root(&shown, &gio::File::for_uri(home), options.limit_to_home));
        let same_root = imp.root.borrow().as_ref().is_some_and(|old| old.equal(&root));
        if same_root
            && previous.is_some_and(|old| old.equal(&shown))
            && selection.selected() != gtk::INVALID_LIST_POSITION
        {
            return;
        }
        if !same_root {
            let tree = model::tree_model(&root, &title_of(&root.uri()), options.show_hidden);
            selection.set_model(Some(&tree));
            imp.root.replace(Some(root));
        }
        let walk = imp.walk.get() + 1;
        imp.walk.set(walk);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            async move { tree.walk_to(&shown, walk).await }
        ));
    }

    /// Ctrl+Shift+E (CMD-039): opens the folders from the top down to the
    /// folder shown, and its own subfolders, selects it and puts keyboard
    /// focus on it, as Windows 11's navigation pane does. False while the
    /// tree is hidden or the tab shows no folder.
    pub(in crate::window) fn reveal_shown(&self) -> bool {
        let imp = self.imp();
        if !imp.options.get().shown || !self.is_mapped() {
            return false;
        }
        let Some(shown) = imp.shown.borrow().clone() else {
            return false;
        };
        let walk = imp.walk.get() + 1;
        imp.walk.set(walk);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = tree)]
            self,
            async move {
                tree.walk_to(&shown, walk).await;
                if tree.imp().walk.get() != walk {
                    return;
                }
                let Some((position, row)) = tree.selected_row() else {
                    return;
                };
                tree.focus_row(position);
                // Scrolling focuses the row only where the list has focus.
                if let Some(view) = tree.imp().view.get() {
                    view.grab_focus();
                }
                row.set_expanded(true);
                if let Some(listing) = model::load_children(&row) {
                    model::loaded(&listing).await;
                }
            }
        ));
        true
    }

    /// Opens the folders from the top down to `target` and selects it, or
    /// the closest folder on the way that is still there.
    async fn walk_to(&self, target: &gio::File, walk: u64) {
        let Some(mut row) = self.row(0) else {
            return;
        };
        loop {
            let Some(file) = model::row_file(&row) else {
                return;
            };
            if file.equal(target) {
                row.set_expanded(true);
                model::load_children(&row);
                break;
            }
            row.set_expanded(true);
            let Some(listing) = model::load_children(&row) else {
                break;
            };
            let was_loaded = !listing.is_loading();
            model::loaded(&listing).await;
            if self.imp().walk.get() != walk {
                return;
            }
            let mut child = self.child_towards(&row, target);
            if child.is_none() && was_loaded {
                // The user may have opened a directory just created in the listing.
                listing.set_file(None::<&gio::File>);
                listing.set_file(Some(&file));
                model::loaded(&listing).await;
                if self.imp().walk.get() != walk {
                    return;
                }
                child = self.child_towards(&row, target);
            }
            let Some(child) = child else {
                break;
            };
            row = child;
        }
        self.mark(row.position());
    }

    /// The subfolder row of `row` that is or holds `target`.
    fn child_towards(&self, row: &gtk::TreeListRow, target: &gio::File) -> Option<gtk::TreeListRow> {
        let depth = row.depth();
        let mut position = row.position() + 1;
        while let Some(child) = self.row(position) {
            if child.depth() <= depth {
                return None;
            }
            let holds = child.depth() == depth + 1
                && model::row_file(&child).is_some_and(|file| file.equal(target) || target.has_prefix(&file));
            if holds {
                return Some(child);
            }
            position += 1;
        }
        None
    }

    /// Selects the row at `position` without focusing it, and scrolls to
    /// it unless automatic scrolling is off.
    fn mark(&self, position: u32) {
        let imp = self.imp();
        if let Some(selection) = imp.selection.get() {
            selection.set_selected(position);
        }
        if let (Some(view), true) = (imp.view.get(), imp.options.get().auto_scroll) {
            view.scroll_to(position, gtk::ListScrollFlags::NONE, None);
        }
    }
}

/// The top of the tree for a folder `shown`: the home folder while it
/// holds `shown` and the tree is limited to it, else the top of `shown`'s
/// file system or server.
fn tree_root(shown: &gio::File, home: &gio::File, limit_to_home: bool) -> gio::File {
    if limit_to_home && (shown.equal(home) || shown.has_prefix(home)) {
        return home.clone();
    }
    let mut root = shown.clone();
    while let Some(parent) = root.parent() {
        root = parent;
    }
    root
}
