// SPDX-License-Identifier: AGPL-3.0-only
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Optional overrides for OpenXplorer's appearance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppearanceOptions {
    /// Space between icon tiles, in pixels.
    pub icon_gap: Option<u8>,
    /// Padding inside each icon tile, in pixels.
    pub icon_padding: Option<u8>,
    /// Radius of floating windows, file selections and menus, in pixels.
    pub corner_radius: Option<u8>,
    /// Use the blue selection preset in both themes.
    pub blue_selection: bool,
    /// Light palette overrides, as 24-bit RGB values.
    pub light: BTreeMap<String, u32>,
    /// Dark palette overrides, as 24-bit RGB values.
    pub dark: BTreeMap<String, u32>,
}

impl AppearanceOptions {
    fn palette_mut(&mut self, appearance: super::Appearance) -> &mut BTreeMap<String, u32> {
        match appearance {
            super::Appearance::Light => &mut self.light,
            super::Appearance::Dark => &mut self.dark,
        }
    }

    pub(super) fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub(super) fn bounded(mut self) -> Self {
        self.icon_gap = self.icon_gap.filter(|value| *value <= 32);
        self.icon_padding = self.icon_padding.filter(|value| *value <= 32);
        self.corner_radius = self.corner_radius.filter(|value| *value <= 32);
        for palette in [&mut self.light, &mut self.dark] {
            palette.retain(|key, value| {
                *value <= 0xff_ffff
                    && matches!(
                        key.as_str(),
                        "bg" | "sidebar"
                            | "title"
                            | "chrome"
                            | "flyout"
                            | "field_bg"
                            | "card"
                            | "text"
                            | "muted"
                            | "border"
                            | "item_hover"
                            | "item_selected"
                    )
            });
        }
        self
    }
}

/// An individual appearance edit, applied to the latest saved preferences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppearanceChange {
    /// Adjust the grid's tile gap.
    Gap(Option<u8>),
    /// Adjust each tile's padding.
    Padding(Option<u8>),
    /// Adjust the radius of windows, selections and menus.
    Radius(Option<u8>),
    /// Enable or disable blue file selection.
    BlueSelection(bool),
    /// Change one color in the chosen theme.
    Color(super::Appearance, String, u32),
    /// Clear overrides in the chosen theme.
    ResetPalette(super::Appearance),
}

impl AppearanceChange {
    pub(super) fn apply(&self, options: &mut AppearanceOptions) {
        match self {
            Self::Gap(value) => options.icon_gap = *value,
            Self::Padding(value) => options.icon_padding = *value,
            Self::Radius(value) => options.corner_radius = *value,
            Self::BlueSelection(value) => options.blue_selection = *value,
            Self::Color(appearance, token, rgb) => {
                options.palette_mut(*appearance).insert(token.clone(), *rgb);
            }
            Self::ResetPalette(appearance) => options.palette_mut(*appearance).clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Appearance, Preferences, PreferencesUpdate};

    #[test]
    fn appearance_edits_round_trip_and_reset_one_palette_only() {
        let mut preferences = Preferences::default();
        for change in [
            AppearanceChange::Gap(Some(3)),
            AppearanceChange::Color(Appearance::Light, "bg".to_owned(), 0xffeedd),
            AppearanceChange::Color(Appearance::Dark, "bg".to_owned(), 0x101217),
            AppearanceChange::ResetPalette(Appearance::Light),
        ] {
            preferences.apply(&PreferencesUpdate {
                appearance_change: Some(change),
                ..PreferencesUpdate::default()
            });
        }
        let json = serde_json::to_value(&preferences).unwrap();
        let mut reread = Preferences::default();
        reread.apply(&PreferencesUpdate::from_json(&json).unwrap());
        assert_eq!(reread.appearance_options, preferences.appearance_options);
        assert_eq!(reread.appearance_options.icon_gap, Some(3));
        assert!(reread.appearance_options.light.is_empty());
        assert_eq!(reread.appearance_options.dark.get("bg"), Some(&0x101217));
    }
}
