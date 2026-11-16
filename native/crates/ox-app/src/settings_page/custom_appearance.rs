// SPDX-License-Identifier: AGPL-3.0-only
use gtk::prelude::*;
use gtk::{gdk, glib};
use ox_core::i18n::gettext;
use ox_core::settings::{Appearance, AppearanceChange, AppearanceOptions, PreferencesUpdate};

use super::group::SettingsGroup;
use super::parts;
use super::row::{ControlName, SettingRow};
use super::search::RowText;
use super::section::SettingsSection;
use super::SettingsPage;
use crate::i18n::message_id;
use crate::window::ButtonStyle;

struct ColorRole {
    token: &'static str,
    title: &'static str,
    light: u32,
    dark: u32,
}

const COLORS: [ColorRole; 12] = [
    ColorRole {
        token: "bg",
        title: message_id("Content background"),
        light: 0xffffff,
        dark: 0x202020,
    },
    ColorRole {
        token: "sidebar",
        title: message_id("Sidebar background"),
        light: 0xfafafa,
        dark: 0x252525,
    },
    ColorRole {
        token: "title",
        title: message_id("Title bar"),
        light: 0xf3f3f3,
        dark: 0x202020,
    },
    ColorRole {
        token: "chrome",
        title: message_id("Toolbar and status bar"),
        light: 0xf9f9f9,
        dark: 0x282828,
    },
    ColorRole {
        token: "flyout",
        title: message_id("Menu background"),
        light: 0xf9f9f9,
        dark: 0x2c2c2c,
    },
    ColorRole {
        token: "field_bg",
        title: message_id("Address and search fields"),
        light: 0xffffff,
        dark: 0x202020,
    },
    ColorRole {
        token: "card",
        title: message_id("Drive cards"),
        light: 0xffffff,
        dark: 0x202020,
    },
    ColorRole {
        token: "text",
        title: message_id("Primary text"),
        light: 0x1b1b1b,
        dark: 0xf1f1f1,
    },
    ColorRole {
        token: "muted",
        title: message_id("Secondary text"),
        light: 0x616161,
        dark: 0xaaaaaa,
    },
    ColorRole {
        token: "border",
        title: message_id("Dividers and borders"),
        light: 0xe8e8e8,
        dark: 0x373737,
    },
    ColorRole {
        token: "item_hover",
        title: message_id("Item under the pointer"),
        light: 0xe5f3ff,
        dark: 0x2d2d2d,
    },
    ColorRole {
        token: "item_selected",
        title: message_id("Selected item"),
        light: 0xcce8ff,
        dark: 0x3d3d3d,
    },
];

pub(super) fn append_groups(page: &SettingsPage, section: &SettingsSection) {
    let layout = SettingsGroup::new(&gettext("Custom layout"));
    layout.add_row(&number_row(
        page,
        message_id("Icon grid gap"),
        message_id("Space between tiles, in pixels. −1 uses the default."),
        |options| options.icon_gap,
        AppearanceChange::Gap,
    ));
    layout.add_row(&number_row(
        page,
        message_id("Icon grid padding"),
        message_id("Space inside each tile, in pixels. −1 uses the default."),
        |options| options.icon_padding,
        AppearanceChange::Padding,
    ));
    layout.add_row(&number_row(
        page,
        message_id("Corner radius"),
        message_id("Rounds windows, selections and menus. 0 makes them square. −1 uses the default."),
        |options| options.corner_radius,
        AppearanceChange::Radius,
    ));

    let blue = SettingRow::new(RowText {
        title: message_id("Blue file selection"),
        description: message_id("Uses the blue preset in place of the selected-item color in both themes."),
        keywords: "highlight accent selected blue contrast",
    });
    let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    page.follow_preferences(glib::clone!(
        #[weak]
        switch,
        move |preferences| switch.set_active(preferences.appearance_options.blue_selection)
    ));
    switch.connect_active_notify(glib::clone!(
        #[weak]
        page,
        move |switch| {
            if page.is_user_change() {
                save(&page, AppearanceChange::BlueSelection(switch.is_active()));
            }
        }
    ));
    blue.add_control(&switch, ControlName::RowTitle);
    layout.add_row(&blue);
    section.append_group(&layout);

    for (appearance, title) in [
        (Appearance::Light, message_id("Light palette")),
        (Appearance::Dark, message_id("Dark palette")),
    ] {
        let group = SettingsGroup::new_folded(&gettext(title));
        for role in &COLORS {
            group.add_row(&color_row(page, appearance, role));
        }
        let reset = SettingRow::new(RowText {
            title: message_id("Reset palette"),
            description: message_id("Restore the original colors for this theme."),
            keywords: "reset default palette colors",
        });
        let button = parts::button(&gettext("Reset colors"), ButtonStyle::Bordered);
        button.connect_clicked(glib::clone!(
            #[weak]
            page,
            move |_| save(&page, AppearanceChange::ResetPalette(appearance))
        ));
        reset.add_control(&button, ControlName::OwnLabel);
        group.add_row(&reset);
        section.append_group(&group);
    }
}

fn number_row(
    page: &SettingsPage,
    title: &'static str,
    description: &'static str,
    read: fn(&AppearanceOptions) -> Option<u8>,
    change: fn(Option<u8>) -> AppearanceChange,
) -> SettingRow {
    let row = SettingRow::new(RowText {
        title,
        description,
        keywords: "pixels margin spacing density radius padding",
    });
    let spin = gtk::SpinButton::with_range(-1.0, 32.0, 1.0);
    page.follow_preferences(glib::clone!(
        #[weak]
        spin,
        move |preferences| {
            let value = read(&preferences.appearance_options);
            spin.set_value(value.map_or(-1.0, f64::from));
        }
    ));
    spin.connect_value_changed(glib::clone!(
        #[weak]
        page,
        move |spin| {
            if page.is_user_change() {
                save(&page, change(u8::try_from(spin.value_as_int()).ok()));
            }
        }
    ));
    row.add_control(&spin, ControlName::RowTitle);
    row
}

fn color_row(page: &SettingsPage, appearance: Appearance, role: &'static ColorRole) -> SettingRow {
    let row = SettingRow::new(RowText {
        title: role.title,
        description: "",
        keywords: "color colour palette",
    });
    let button = parts::button("", ButtonStyle::Bordered);
    page.follow_preferences(glib::clone!(
        #[weak]
        button,
        move |preferences| {
            let rgb = current_rgb(&preferences.appearance_options, appearance, role);
            button.set_label(&format!("#{rgb:06X}"));
        }
    ));
    button.connect_clicked(glib::clone!(
        #[weak]
        page,
        move |_| edit_hsv(&page, appearance, role)
    ));
    row.add_control(&button, ControlName::RowTitle);
    row
}

fn current_rgb(options: &AppearanceOptions, appearance: Appearance, role: &ColorRole) -> u32 {
    let (palette, default) = match appearance {
        Appearance::Light => (&options.light, role.light),
        Appearance::Dark => (&options.dark, role.dark),
    };
    palette.get(role.token).copied().unwrap_or(default)
}

#[allow(
    deprecated,
    reason = "GTK's dialog response supplies Accept, Cancel and Escape"
)]
fn edit_hsv(page: &SettingsPage, appearance: Appearance, role: &'static ColorRole) {
    let Some(parent) = page.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let rgb = current_rgb(
        &page.context().settings_data().preferences.appearance_options,
        appearance,
        role,
    );
    let Ok(color) = gdk::RGBA::parse(format!("#{rgb:06x}")) else {
        return;
    };
    let (hue, saturation, value) = gtk::rgb_to_hsv(color.red(), color.green(), color.blue());
    let dialog = gtk::Dialog::builder()
        .title(gettext(role.title))
        .transient_for(&parent)
        .modal(true)
        .build();
    dialog.add_button(&gettext("Cancel"), gtk::ResponseType::Cancel);
    dialog.add_button(&gettext("Apply"), gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);
    let body = dialog.content_area();
    body.set_spacing(12);
    body.set_margin_top(18);
    body.set_margin_bottom(18);
    body.set_margin_start(20);
    body.set_margin_end(20);
    let field = |label: &str, maximum, number| {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 20);
        let caption = gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .hexpand(true)
            .build();
        let spin = gtk::SpinButton::with_range(0.0, maximum, 0.1);
        spin.set_value(number);
        spin.set_digits(1);
        spin.update_relation(&[gtk::accessible::Relation::LabelledBy(&[caption.upcast_ref()])]);
        line.append(&caption);
        line.append(&spin);
        body.append(&line);
        spin
    };
    let h = field(&gettext("Hue (°)"), 360.0, f64::from(hue) * 360.0);
    let s = field(&gettext("Saturation (%)"), 100.0, f64::from(saturation) * 100.0);
    let v = field(&gettext("Value (%)"), 100.0, f64::from(value) * 100.0);
    let current = gtk::Label::new(None);
    body.append(&current);
    let rgb_at = move |h: &gtk::SpinButton, s: &gtk::SpinButton, v: &gtk::SpinButton| {
        #[allow(clippy::cast_possible_truncation)]
        let channels = gtk::hsv_to_rgb(
            (h.value() / 360.0) as f32,
            (s.value() / 100.0) as f32,
            (v.value() / 100.0) as f32,
        );
        [channels.0, channels.1, channels.2]
            .into_iter()
            .fold(0, |packed, channel| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let byte = (channel.clamp(0.0, 1.0) * 255.0).round() as u32;
                (packed << 8) | byte
            })
    };
    current.set_label(&format!("#{rgb:06X}"));
    for input in [&h, &s, &v] {
        input.connect_value_changed(glib::clone!(
            #[weak]
            h,
            #[weak]
            s,
            #[weak]
            v,
            #[weak]
            current,
            move |_| current.set_label(&format!("#{:06X}", rgb_at(&h, &s, &v)))
        ));
    }
    dialog.connect_response(glib::clone!(
        #[weak]
        page,
        move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                h.update();
                s.update();
                v.update();
                save(
                    &page,
                    AppearanceChange::Color(appearance, role.token.to_owned(), rgb_at(&h, &s, &v)),
                );
            }
            dialog.close();
        }
    ));
    dialog.present();
}

fn save(page: &SettingsPage, change: AppearanceChange) {
    page.save_preferences(PreferencesUpdate {
        appearance_change: Some(change),
        ..PreferencesUpdate::default()
    });
}
