// SPDX-License-Identifier: AGPL-3.0-only
//! Showing and hiding the navigation pane (SIDE-024).
//!
//! Dolphin closes its Places panel with F9 and Explorer turns off View >
//! Show > Navigation pane. The navigation row's button, View and F9
//! run the same saved action. The narrow layout temporarily hides the
//! pane, and Places lists its locations whenever it is hidden.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::icons::{self, Icon};

use super::menu_popover::{name_menu_button, MenuPopover};
use super::{BrowserWindow, WindowAction};

/// The glyph of the Places button, as big as Back's and Up's.
const PLACES_GLYPH: i32 = 16;

impl BrowserWindow {
    /// Adds the Places button to the navigation row and shows the pane as
    /// the preferences say.
    pub(super) fn install_sidebar_toggle(&self) {
        let imp = self.imp();
        let toggle = gtk::ToggleButton::builder()
            .child(&icons::image(Icon::PanelLeft, PLACES_GLYPH))
            .tooltip_text(ox_core::i18n::gettext("Toggle navigation pane (F9)"))
            .valign(gtk::Align::Center)
            .css_classes(["nav-sidebar-toggle"])
            .build();
        toggle.update_property(&[gtk::accessible::Property::Label(&ox_core::i18n::gettext(
            "Navigation pane",
        ))]);
        WindowAction::Sidebar.assign_to(&toggle);
        imp.navigation_row
            .insert_child_after(&toggle, Some(&*imp.navigation_buttons));
        let button = gtk::MenuButton::builder()
            .child(&icons::image(Icon::Folder, PLACES_GLYPH))
            .tooltip_text(ox_core::i18n::gettext("Places"))
            .valign(gtk::Align::Center)
            .css_classes(["nav-places"])
            .visible(false)
            .build();
        name_menu_button(&button, &ox_core::i18n::gettext("Places"));
        let popover = MenuPopover::new(Vec::new());
        button.set_popover(Some(&popover));
        // Built as it opens, so it lists the places shown now.
        button.set_create_popup_func(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            popover,
            move |_| popover.set_entries(window.sidebar().places_menu())
        ));
        imp.navigation_row.insert_child_after(&button, Some(&toggle));
        imp.sidebar_toggle
            .set(toggle)
            .expect("the sidebar switch is added once");
        imp.places_button
            .set(button)
            .expect("the Places button is added once");
        let preferences = self.context().settings_data().preferences;
        self.show_sidebar(!preferences.hide_sidebar);
        self.sidebar().set_icon_size(preferences.sidebar_icon_size);
    }

    /// Shows or hides the navigation pane, its resizer with it, and the
    /// Places button in its stead.
    pub(super) fn show_sidebar(&self, shown: bool) {
        let room = self.is_picking() || !self.window_width().is_compact();
        let shown = room && shown;
        let imp = self.imp();
        let focus = GtkWindowExt::focus(self);
        let sidebar_focused = focus.as_ref().is_some_and(|widget| {
            widget.is_ancestor(self.sidebar()) || widget == self.sidebar_resizer().upcast_ref::<gtk::Widget>()
        });
        if !shown && sidebar_focused {
            self.folder_pane().focus_view();
        }
        self.sidebar().set_visible(shown);
        self.sidebar_resizer().set_visible(shown);
        if let Some(button) = imp.places_button.get() {
            button.set_visible(!shown);
            if let Some(toggle) = imp.sidebar_toggle.get() {
                if !room && focus.as_ref() == Some(toggle.upcast_ref()) {
                    button.grab_focus();
                }
                toggle.set_visible(room);
            }
        }
    }

    /// Reapplies the user's choice at the current width.
    pub(super) fn fit_sidebar(&self) {
        let shown = self
            .window_action_state(WindowAction::Sidebar)
            .and_then(|state| state.get::<bool>())
            .unwrap_or(true);
        self.show_sidebar(shown);
    }

    /// The Places button shown while the navigation pane is hidden.
    #[cfg(test)]
    pub(super) fn places_button(&self) -> &gtk::MenuButton {
        self.imp()
            .places_button
            .get()
            .expect("install_sidebar_toggle adds the button")
    }
}
