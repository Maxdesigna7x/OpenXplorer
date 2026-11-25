// SPDX-License-Identifier: AGPL-3.0-only
use std::fmt::Write;

use ox_core::settings::{Appearance, AppearanceOptions};

pub(super) fn stylesheet(options: &AppearanceOptions, appearance: Appearance) -> String {
    let mut css = String::new();
    let palette = match appearance {
        Appearance::Light => &options.light,
        Appearance::Dark => &options.dark,
    };
    for (token, rgb) in palette {
        let _ = writeln!(css, "@define-color ox_{token} #{rgb:06x};");
    }
    if options.blue_selection {
        let blue = match appearance {
            Appearance::Light => "#cce8ff",
            Appearance::Dark => "#225287",
        };
        let _ = writeln!(css, "@define-color ox_item_selected {blue};");
    }
    if options.blue_selection || palette.contains_key("item_selected") {
        css.push_str("@define-color ox_item_selected_hover @ox_item_selected;\n");
    }

    const GRID: &str = "gridview.files:not(.compact):not(.tiles):not(.content) > child:not(.heading-row)";
    if let Some(gap) = options.icon_gap {
        let half = f32::from(gap) / 2.0;
        let _ = writeln!(css, "{GRID} {{ margin: {half}px; }}");
    }
    if let Some(padding) = options.icon_padding {
        // Keep the uniform tile min-height from theme/fonts.rs so 1-line and
        // 2-line rows have identical heights and GtkGridView does not
        // oscillate its scroll range while holding the scrollbar thumb.
        let _ = writeln!(css, "{GRID} {{ padding: {padding}px; }}");
        let inset = 1 - i32::from(padding);
        let _ = writeln!(
            css,
            "{GRID} checkbutton.item-check.on-icon {{ margin: {inset}px -4px -4px {inset}px; }}"
        );
    }
    if let Some(radius) = options.corner_radius {
        let floating = "window.ox.csd:not(.maximized):not(.fullscreen):not(.tiled):not(.tiled-top):not(.tiled-left):not(.tiled-right):not(.tiled-bottom)";
        let _ = writeln!(
            css,
            "
            {floating} {{ border-radius: {radius}px; }}
            {floating} .ox-titlebar {{ border-radius: {radius}px {radius}px 0 0; }}
            {floating} .statusbar {{ border-radius: 0 0 {radius}px {radius}px; }}
            {floating} .ox-titlebar button.caption.close {{ border-top-right-radius: {radius}px; }}
            gridview.files > child, gridview.files.compact > child,
            gridview.files.tiles > child > .item-cell:not(.group-slot),
            gridview.files.content > child > .item-cell:not(.group-slot),
            columnview.files > listview > row, .sidebar list.places > row,
            popover.ox-menu > contents, popover.menu.ox-menu > contents,
            popover.ox-menu list > row, popover.menu.ox-menu modelbutton
            {{ border-radius: {radius}px; }}
        "
        );
    }
    css
}
