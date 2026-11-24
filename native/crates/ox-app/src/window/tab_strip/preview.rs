// SPDX-License-Identifier: AGPL-3.0-only
//! In-memory previews of the folder pane, captured at tooltip size.

use std::collections::VecDeque;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{cairo, gdk, glib};

use super::{TabId, TabView};
use crate::window::BrowserWindow;

const WIDTH: i32 = 260;
const HEIGHT: i32 = 156;
const KEPT: usize = 16;

#[derive(Debug, Default)]
pub(super) struct Previews(VecDeque<(TabId, String, gdk::Texture)>);

impl Previews {
    pub(super) fn retain(&mut self, tabs: &[TabView]) {
        self.0
            .retain(|(id, uri, _)| tabs.iter().any(|tab| tab.id == *id && tab.uri == *uri));
    }

    fn insert(&mut self, id: TabId, uri: String, texture: gdk::Texture) {
        self.0.retain(|(kept, _, _)| *kept != id);
        self.0.push_front((id, uri, texture));
        self.0.truncate(KEPT);
    }

    fn get(&self, tab: &TabView) -> Option<gdk::Texture> {
        self.0
            .iter()
            .find(|(id, uri, _)| *id == tab.id && *uri == tab.uri)
            .map(|(_, _, texture)| texture.clone())
    }
}

impl BrowserWindow {
    pub(in crate::window) fn remember_tab_preview(&self) {
        let active = self
            .imp()
            .session
            .borrow()
            .active()
            .map(|tab| (tab.id, tab.uri().to_owned()));
        if let Some(((id, uri), texture)) = active.zip(picture(self.folder_pane())) {
            self.tab_strip()
                .imp()
                .previews
                .borrow_mut()
                .insert(id, uri, texture);
        }
    }

    fn tab_preview(&self, tab: &TabView) -> Option<gdk::Texture> {
        if self.imp().session.borrow().active_id() == Some(tab.id) {
            return picture(self.folder_pane());
        }
        self.tab_strip().imp().previews.borrow().get(tab)
    }
}

/// GTK's hover delay and dismissal also apply to the preview. Keyboard
/// tooltips keep the ordinary address, and a tab's close button keeps its own.
pub(super) fn attach(widget: &gtk::Box, tab: TabView) {
    widget.connect_query_tooltip(move |widget, _, _, keyboard, tooltip| {
        let window = widget.root().and_downcast::<BrowserWindow>();
        let texture = window
            .filter(|_| !keyboard)
            .and_then(|window| window.tab_preview(&tab));
        if !keyboard {
            let preview = gtk::Box::new(gtk::Orientation::Vertical, 5);
            let title = gtk::Label::new(Some(&tab.title));
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let address = gtk::Label::new(Some(&tab.tooltip));
            address.set_xalign(0.0);
            address.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            address.add_css_class("dim-label");
            preview.append(&title);
            preview.append(&address);
            if let Some(texture) = texture {
                let image = gtk::Picture::for_paintable(&texture);
                image.set_size_request(texture.width(), texture.height());
                preview.append(&image);
            } else {
                let icon = crate::icons::ArtImage::new(tab.icon, 48);
                icon.set_halign(gtk::Align::Center);
                icon.set_size_request(WIDTH, 92);
                preview.append(&icon);
            }
            tooltip.set_custom(Some(&preview));
        } else {
            tooltip.set_text(Some(&tab.tooltip));
        }
        true
    });
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "widget sizes are exact as f32 in this range and preview dimensions fit i32"
)]
fn picture(widget: &impl IsA<gtk::Widget>) -> Option<gdk::Texture> {
    if !widget.is_mapped() {
        return None;
    }
    let width = widget.width() as f32;
    let height = widget.height() as f32;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (WIDTH as f32 / width).min(HEIGHT as f32 / height).min(1.0);
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(scale, scale);
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node()?;
    // Tab changes can run while the window renderer is busy. Rasterize the
    // small preview independently, without asking the live GPU for a texture.
    let width = (width * scale).ceil() as i32;
    let height = (height * scale).ceil() as i32;
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).ok()?;
    {
        let context = cairo::Context::new(&surface).ok()?;
        node.draw(&context);
    }
    let stride = usize::try_from(surface.stride()).ok()?;
    let bytes = glib::Bytes::from_owned(surface.data().ok()?.to_vec());
    let format = if cfg!(target_endian = "little") {
        gdk::MemoryFormat::B8g8r8a8Premultiplied
    } else {
        gdk::MemoryFormat::A8r8g8b8Premultiplied
    };
    Some(gdk::MemoryTexture::new(width, height, format, &bytes, stride).upcast())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::harness::{wait_for_frames, Fixture, TestWindow};

    #[gtk::test]
    fn visited_tabs_keep_a_small_preview_after_switching_away() {
        let fixture = Fixture::standard();
        let test = TestWindow::open(&fixture.uri());
        wait_for_frames(&test.window, 2);
        let strip = test.window.tab_strip();
        let tab = strip.imp().shown.borrow()[0].0.clone();
        let active = test.window.tab_preview(&tab).expect("the active preview");
        assert!(active.width() <= WIDTH && active.height() <= HEIGHT);

        test.activate("new-tab", None);
        let saved = test.window.tab_preview(&tab).expect("the previous tab's preview");
        assert_eq!((saved.width(), saved.height()), (active.width(), active.height()));
        test.window.close_tab(tab.id);
        assert!(test.window.tab_preview(&tab).is_none());
    }
}
