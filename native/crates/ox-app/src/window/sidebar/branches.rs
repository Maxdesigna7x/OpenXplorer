// SPDX-License-Identifier: AGPL-3.0-only
//! Folders expanded directly beneath mounted drives in the navigation pane.

use std::collections::{HashMap, HashSet};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};
use ox_core::settings::FolderTreeOptions;

use crate::icons::{Art, Icon};
use crate::volumes::VolumeKind;
use crate::window::place_menus::PlaceMenu;

use super::entries::{RowLevel, RowTarget, Section, SidebarEntry};
use super::{row, BrowserWindow, HiddenRow, Sidebar};

#[derive(Debug, Default)]
pub(super) struct Branches {
    options: FolderTreeOptions,
    expanded: HashSet<String>,
    folders: HashMap<String, Vec<SidebarEntry>>,
    loading: HashMap<String, u64>,
    next_request: u64,
}

impl Sidebar {
    pub(in crate::window) fn branch_options(&self) -> FolderTreeOptions {
        self.imp().branches.borrow().options
    }

    pub(in crate::window) fn set_branch_options(&self, options: FolderTreeOptions) {
        let old = self.imp().branches.borrow().options;
        self.imp().branches.borrow_mut().options = options;
        if old.show_hidden != options.show_hidden {
            self.refresh_branches();
        } else {
            self.rebuild_branches();
        }
        if !old.shown && options.shown {
            self.follow_selected_drive();
        }
    }

    fn drive_key(&self, entry: &SidebarEntry) -> Option<String> {
        if entry.section != Section::ThisPc {
            return None;
        }
        let RowTarget::Location(uri) = &entry.target else {
            return None;
        };
        if matches!(entry.level, RowLevel::Descendant(_)) {
            return Some(uri.clone());
        }
        if !matches!(
            entry.menu,
            Some(PlaceMenu::Drive {
                kind: VolumeKind::Drive,
                ..
            })
        ) {
            return None;
        }
        let uri = self.root().and_downcast::<BrowserWindow>().map_or_else(
            || uri.clone(),
            |window| window.flatpak_root_destination(uri.clone()),
        );
        Some(gio::File::for_uri(&uri).uri().to_string())
    }

    pub(super) fn build_sidebar_row(&self, entry: &SidebarEntry) -> gtk::ListBoxRow {
        let branch = self
            .branch_options()
            .shown
            .then(|| self.drive_key(entry))
            .flatten();
        let expanded = branch
            .as_ref()
            .map(|uri| self.imp().branches.borrow().expanded.contains(uri));
        let row = row::sidebar_row(entry, self.row_look(), self.is_collapsed(entry.section), expanded);
        if let Some((uri, expanded)) = branch.zip(expanded) {
            if let Some(chevron) = row::chevron(&row) {
                chevron.connect_clicked(glib::clone!(
                    #[weak(rename_to = sidebar)]
                    self,
                    move |_| {
                        sidebar.set_branch(&uri, !expanded);
                    }
                ));
            }
        }
        row
    }

    pub(super) fn rebuild_branches(&self) {
        let focused = self.focused_entry().map(|entry| entry.target);
        let mut rows = Vec::new();
        {
            let branches = self.imp().branches.borrow();
            for (entry, hidden) in self.imp().base_rows.borrow().iter() {
                rows.push((entry.clone(), *hidden));
                if branches.options.shown {
                    if let Some(uri) = self.drive_key(entry) {
                        branches.append(&mut rows, &uri, 2, *hidden);
                    }
                }
            }
        }
        let (entries, hidden) = rows.into_iter().unzip();
        self.set_entries(entries);
        self.mark_hidden(hidden);
        self.highlight_location();
        if let Some(target) = focused {
            let index = self
                .imp()
                .entries
                .borrow()
                .iter()
                .position(|entry| entry.target == target);
            if let Some(row) = index.and_then(|index| self.list().row_at_index(i32::try_from(index).ok()?)) {
                if row.is_visible() {
                    row.grab_focus();
                    self.highlight_location();
                }
            }
        }
    }

    pub(super) fn follow_selected_drive(&self) {
        let location = self.imp().location.borrow().clone();
        if let Some(uri) = location {
            self.follow_drive_path(&uri);
        }
    }

    pub(super) fn follow_drive_path(&self, uri: &str) {
        if !self.branch_options().shown || self.is_collapsed(Section::ThisPc) {
            return;
        }
        let target = gio::File::for_uri(uri);
        let drive = self
            .imp()
            .base_rows
            .borrow()
            .iter()
            .filter_map(|(entry, _)| self.drive_key(entry))
            .map(|uri| gio::File::for_uri(&uri))
            .filter(|drive| target.equal(drive) || target.has_prefix(drive))
            .max_by_key(|drive| drive.uri().len());
        let Some(drive) = drive else { return };
        // Only the ancestors of the current folder, and the folder itself,
        // are opened. Sibling drives and folders wait for their chevron.
        let mut path = Vec::new();
        let mut folder = target;
        loop {
            path.push(folder.uri().to_string());
            if folder.equal(&drive) {
                break;
            }
            let Some(parent) = folder.parent() else { return };
            folder = parent;
        }
        path.reverse();
        {
            let mut branches = self.imp().branches.borrow_mut();
            for pair in path.windows(2) {
                let parent = &pair[0];
                let child = &pair[1];
                let stale = branches.folders.get(parent).is_some_and(|folders| {
                    !folders
                        .iter()
                        .any(|entry| matches!(&entry.target, RowTarget::Location(uri) if uri == child))
                });
                if stale {
                    branches.folders.remove(parent);
                }
            }
        }
        let mut changed = false;
        for uri in path {
            changed |= self.imp().branches.borrow_mut().expanded.insert(uri.clone());
            self.load_branch(uri);
        }
        if changed {
            self.rebuild_branches();
        }
    }

    pub(in crate::window) fn refresh_branches(&self) {
        let expanded = {
            let mut branches = self.imp().branches.borrow_mut();
            branches.folders.clear();
            branches.loading.clear();
            branches.expanded.clone()
        };
        if self.branch_options().shown {
            for uri in expanded {
                self.load_branch(uri);
            }
        }
        self.rebuild_branches();
    }

    pub(super) fn set_focused_branch(&self, expand: bool) -> bool {
        let Some(uri) = self.focused_entry().and_then(|entry| self.drive_key(&entry)) else {
            return false;
        };
        self.branch_options().shown && self.set_branch(&uri, expand)
    }

    pub(super) fn open_all_below_focused(&self) -> bool {
        let Some(root) = self
            .focused_entry()
            .and_then(|entry| self.drive_key(&entry))
            .filter(|_| self.branch_options().shown)
        else {
            return false;
        };
        let sidebar = self.downgrade();
        glib::spawn_future_local(async move {
            let mut pending = vec![(root, 0)];
            let mut count = 0;
            while let Some((uri, depth)) = pending.pop() {
                let Some(sidebar) = sidebar.upgrade() else { return };
                let options = sidebar.branch_options();
                if !options.shown || count == 1_000 {
                    break;
                }
                count += 1;
                let known = sidebar.imp().branches.borrow().folders.get(&uri).cloned();
                let children = match known {
                    Some(children) => Some(children),
                    None => read_folders(&uri, options.show_hidden).await,
                };
                let Some(children) = children else { continue };
                if depth < 32 {
                    pending.extend(
                        children
                            .iter()
                            .rev()
                            .filter_map(|entry| match &entry.target {
                                RowTarget::Location(uri) => Some((uri.clone(), depth + 1)),
                                _ => None,
                            })
                            .take(1_000 - count),
                    );
                }
                let mut branches = sidebar.imp().branches.borrow_mut();
                branches.expanded.insert(uri.clone());
                branches.folders.insert(uri, children);
            }
            if let Some(sidebar) = sidebar.upgrade() {
                sidebar.rebuild_branches();
            }
        });
        true
    }

    fn set_branch(&self, uri: &str, expand: bool) -> bool {
        let changed = {
            let mut branches = self.imp().branches.borrow_mut();
            if expand {
                branches.expanded.insert(uri.to_owned())
            } else {
                branches.expanded.remove(uri)
            }
        };
        if changed {
            if expand {
                self.load_branch(uri.to_owned());
            }
            self.rebuild_branches();
        }
        changed
    }

    fn load_branch(&self, uri: String) {
        let (request, hidden) = {
            let mut branches = self.imp().branches.borrow_mut();
            if branches.loading.contains_key(&uri) || branches.folders.contains_key(&uri) {
                return;
            }
            branches.next_request += 1;
            let request = branches.next_request;
            branches.loading.insert(uri.clone(), request);
            (request, branches.options.show_hidden)
        };
        let sidebar = self.downgrade();
        glib::spawn_future_local(async move {
            let folders = read_folders(&uri, hidden).await;
            let Some(sidebar) = sidebar.upgrade() else { return };
            {
                let mut branches = sidebar.imp().branches.borrow_mut();
                if branches.loading.get(&uri) != Some(&request) {
                    return;
                }
                branches.loading.remove(&uri);
                if let Some(folders) = folders {
                    branches.folders.insert(uri, folders);
                }
            }
            sidebar.rebuild_branches();
            if sidebar.branch_options().auto_scroll {
                sidebar.scroll_to_loaded_location();
            }
        });
    }

    fn scroll_to_loaded_location(&self) {
        let Some(row) = self
            .list()
            .selected_row()
            .filter(|row| row.has_css_class("branch"))
        else {
            return;
        };
        let location = self.imp().location.borrow();
        let entries = self.imp().entries.borrow();
        let exact = usize::try_from(row.index())
            .ok()
            .and_then(|index| entries.get(index))
            .is_some_and(
                |entry| matches!(&entry.target, RowTarget::Location(uri) if Some(uri) == location.as_ref()),
            );
        if exact {
            if let Some(viewport) = row
                .ancestor(gtk::Viewport::static_type())
                .and_downcast::<gtk::Viewport>()
            {
                viewport.scroll_to(&row, None);
            }
        }
    }
}

impl Branches {
    fn append(&self, rows: &mut Vec<(SidebarEntry, HiddenRow)>, uri: &str, depth: u16, hidden: HiddenRow) {
        if !self.expanded.contains(uri) {
            return;
        }
        if let Some(children) = self.folders.get(uri) {
            for entry in children {
                let mut entry = entry.clone();
                entry.level = RowLevel::Descendant(depth);
                let RowTarget::Location(uri) = entry.target.clone() else {
                    continue;
                };
                rows.push((entry, hidden));
                self.append(rows, &uri, depth + 1, hidden);
            }
        }
    }
}

async fn read_folders(uri: &str, show_hidden: bool) -> Option<Vec<SidebarEntry>> {
    let file = gio::File::for_uri(uri);
    let enumerator = file
        .enumerate_children_future(
            "standard::name,standard::display-name,standard::type,standard::is-hidden",
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            glib::Priority::DEFAULT,
        )
        .await
        .ok()?;
    let mut folders = Vec::new();
    loop {
        let infos = enumerator
            .next_files_future(128, glib::Priority::DEFAULT)
            .await
            .ok()?;
        if infos.is_empty() {
            break;
        }
        for info in infos {
            if info.file_type() != gio::FileType::Directory || (!show_hidden && info.is_hidden()) {
                continue;
            }
            let child = file.child(info.name());
            folders.push(SidebarEntry {
                section: Section::ThisPc,
                level: RowLevel::Descendant(2),
                label: info.display_name().to_string(),
                icon: Art::Glyph(Icon::Folder),
                target: RowTarget::Location(child.uri().to_string()),
                tooltip: child.parse_name().to_string(),
                pinned: false,
                menu: None,
                eject: None,
            });
        }
    }
    let mut keyed: Vec<_> = folders
        .into_iter()
        .map(|entry| (crate::folder_view::sorting::SortKey::new(&entry.label), entry))
        .collect();
    keyed.sort_by(|(left, a), (right, b)| left.natural_cmp(right).then_with(|| a.label.cmp(&b.label)));
    Some(keyed.into_iter().map(|(_, entry)| entry).collect())
}
