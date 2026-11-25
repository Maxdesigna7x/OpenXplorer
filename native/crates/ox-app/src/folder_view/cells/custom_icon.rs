// SPDX-License-Identifier: AGPL-3.0-only
//! Custom icons (PROP-016): an item whose `GVfs` metadata names a picture
//! or theme icon shows it in place of its art in the details and icon views.
//! The lookup runs when a cell is bound, at low priority, like a thumbnail lookup,
//! and a picture that arrives after the cell shows another item is
//! dropped. Pictures are decoded once at icon size and kept, so scrolling
//! back to an item does not decode its picture again. An item without a
//! custom icon shows its preview in the same place, when it has one
//! ([`crate::thumbnails`]). A folder without one shows the picture chosen
//! for it ([`super::folder_picture`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use super::folder_picture::{FOLDER_PICTURE, SMALLEST_PICTURE_ICON};
use super::FileCell;
use crate::folder_view::item::FileItem;
use crate::picture_guard::{self, Limits};
use crate::thumbnails::{self, PreviewPolicy};

/// The metadata key Files stores a custom icon under.
pub(crate) const CUSTOM_ICON: &str = "metadata::custom-icon";
/// Files also stores a theme icon name instead of an image URI.
const CUSTOM_ICON_NAME: &str = "metadata::custom-icon-name";
/// Pictures larger than this are not decoded for an icon.
const MAX_ICON_BYTES: u64 = 16 * 1024 * 1024;
/// The size a picture is decoded at: the largest icon the views draw.
const ICON_PIXELS: i32 = 256;
/// How many decoded pictures are kept.
const MAX_CACHED_ICONS: usize = 256;
/// How many custom pictures are decoded at once, including workers whose
/// rows scrolled away before their blocking decoder could finish.
const DECODING_AT_ONCE: usize = 4;

thread_local! {
    /// Decoded pictures by their URI.
    static DECODED: RefCell<HashMap<String, gdk::Texture>> = RefCell::new(HashMap::new());
    static DECODING: Rc<thumbnails::Slots> = Rc::new(thumbnails::Slots::new(DECODING_AT_ONCE));
}

/// The decoded picture at `icon_uri`, decoding it off the main thread the
/// first time.
pub(crate) async fn icon_texture(icon_uri: String) -> Option<gdk::Texture> {
    if let Some(texture) = DECODED.with_borrow(|decoded| decoded.get(&icon_uri).cloned()) {
        return Some(texture);
    }
    let uri = icon_uri.clone();
    let texture = DECODING
        .with(Rc::clone)
        .blocking(move |_| decode(&uri))
        .await
        .flatten()?;
    DECODED.with_borrow_mut(|decoded| {
        if decoded.len() >= MAX_CACHED_ICONS {
            decoded.clear();
        }
        decoded.insert(icon_uri, texture.clone());
    });
    Some(texture)
}

impl FileCell {
    /// Shows the art again, then looks up `item`'s custom icon and, if it
    /// has none, a folder's picture (PROP-037): on the folder on a tile of
    /// Medium icons or larger, in the icon's place on smaller ones and in
    /// rows; or, where `previews` allow one, its preview (VIEW-057).
    /// A lookup still running for the item shown before is dropped.
    pub(crate) fn look_up_picture(&self, item: &FileItem, previews: PreviewPolicy) {
        self.cancel_picture_lookup();
        let imp = self.imp();
        imp.folder_picture.show_texture(None);
        let entry = item.entry();
        let uri = entry.uri.clone();
        let size = imp.icon_size.get();
        let scale = self.scale_factor().max(1);
        let pixels = size * scale;
        let preview = previews.request(entry, pixels);
        let cached = preview.as_ref().and_then(thumbnails::cached_preview);
        if let Some((texture, _)) = &cached {
            imp.custom_icon.set_paintable(Some(texture));
            imp.custom_icon.set_visible(true);
            imp.image.set_opacity(0.0);
        } else {
            imp.custom_icon.set_visible(false);
            imp.image.set_opacity(1.0);
        }
        if !uri.starts_with("file:") && preview.is_none() {
            return;
        }
        let enough_cached = cached.is_some_and(|(_, enough)| enough);
        let cached_own = OWN_PICTURES.with_borrow(|cache| cache.get(&uri).cloned());
        if enough_cached && cached_own.as_ref().is_some_and(OwnPictures::is_empty) {
            return;
        }
        let is_folder = entry.is_dir;
        let on_the_folder = imp.is_tile.get() && imp.icon_size.get() >= SMALLEST_PICTURE_ICON;
        let cell = self.downgrade();
        let lookup = glib::spawn_future_local(async move {
            let own = match cached_own {
                Some(own) => own,
                None => own_pictures(&uri).await,
            };
            let mut paintable = match own.icon {
                Some(icon) => icon_texture(icon)
                    .await
                    .map(|texture| texture.upcast::<gdk::Paintable>()),
                None => None,
            };
            if let (None, Some(cell), Some(name)) = (&paintable, cell.upgrade(), own.icon_name) {
                let theme = gtk::IconTheme::for_display(&cell.display());
                if theme.has_icon(&name) {
                    paintable = Some(
                        theme
                            .lookup_icon(
                                &name,
                                &[],
                                size,
                                scale,
                                cell.direction(),
                                gtk::IconLookupFlags::empty(),
                            )
                            .upcast(),
                    );
                }
            }
            // A folder's picture sits on the folder at Medium icons and
            // larger, and takes the small icon's place in Details, List
            // and Small icons, as Windows draws it there.
            if let Some(picture) = own.folder_picture.filter(|_| paintable.is_none() && is_folder) {
                if on_the_folder {
                    if let (Some(cell), Some(picture)) = (cell.upgrade(), icon_texture(picture).await) {
                        cell.imp().folder_picture.show_texture(Some(&picture));
                        cell.imp().picture_lookup.take();
                    }
                    return;
                }
                paintable = icon_texture(picture).await.map(|texture| texture.upcast());
            }
            if paintable.is_none() && !enough_cached {
                if let Some(request) = preview {
                    paintable = thumbnails::preview(request).await.map(|texture| texture.upcast());
                }
            }
            if let Some(cell) = cell.upgrade() {
                let imp = cell.imp();
                if let Some(paintable) = paintable {
                    imp.custom_icon.set_paintable(Some(&paintable));
                    imp.custom_icon.set_visible(true);
                    imp.image.set_opacity(0.0);
                }
                imp.picture_lookup.take();
            }
        });
        imp.picture_lookup.replace(Some(lookup));
    }

    /// Drops the lookup of a custom icon or preview still running, as for
    /// a row GTK unbinds when it scrolls away.
    pub(crate) fn cancel_picture_lookup(&self) {
        if let Some(lookup) = self.imp().picture_lookup.take() {
            lookup.abort();
        }
    }

    /// Whether the cell shows its folder's picture on the folder, for
    /// tests.
    #[cfg(test)]
    pub(crate) fn shows_folder_picture(&self) -> bool {
        self.imp().folder_picture.shows_picture()
    }

    /// Whether the cell shows a custom icon or a preview, for tests.
    #[cfg(test)]
    pub(crate) fn shows_custom_icon(&self) -> bool {
        self.imp().custom_icon.is_visible()
    }
}

/// How many local items' custom-icon metadata entries are cached in memory.
const MAX_OWN_PICTURES: usize = 2048;

thread_local! {
    static OWN_PICTURES: RefCell<HashMap<String, OwnPictures>> = RefCell::new(HashMap::new());
}

/// Drops cached custom-icon / folder-picture metadata for `uri` so a change
/// in Properties is read again.
pub(super) fn forget_own_pictures(uri: &str) {
    OWN_PICTURES.with_borrow_mut(|cache| {
        cache.remove(uri);
    });
}

/// The custom icon and folder picture assigned to a local item.
#[derive(Debug, Clone, Default)]
struct OwnPictures {
    icon: Option<String>,
    icon_name: Option<String>,
    folder_picture: Option<String>,
}

impl OwnPictures {
    fn is_empty(&self) -> bool {
        self.icon.is_none() && self.icon_name.is_none() && self.folder_picture.is_none()
    }
}

/// The custom icon and folder picture of the local item at `uri`, read
/// in one query and cached in memory.
async fn own_pictures(uri: &str) -> OwnPictures {
    if !uri.starts_with("file:") {
        return OwnPictures::default();
    }
    let file = gio::File::for_uri(uri);
    let Ok(info) = file
        .query_info_future(
            &format!("{CUSTOM_ICON},{CUSTOM_ICON_NAME},{FOLDER_PICTURE}"),
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::LOW,
        )
        .await
    else {
        return OwnPictures::default();
    };
    let named = |key: &str| {
        info.attribute_string(key)
            .filter(|picture| !picture.is_empty())
            .map(|picture| picture.to_string())
    };
    let own = OwnPictures {
        icon: named(CUSTOM_ICON),
        icon_name: named(CUSTOM_ICON_NAME),
        folder_picture: named(FOLDER_PICTURE),
    };
    OWN_PICTURES.with_borrow_mut(|cache| {
        if cache.len() >= MAX_OWN_PICTURES && !cache.contains_key(uri) {
            cache.clear();
        }
        cache.insert(uri.to_owned(), own.clone());
    });
    own
}

/// The picture at `uri` at icon size, if it is a local image of a
/// sensible size.
fn decode(uri: &str) -> Option<gdk::Texture> {
    let path = gio::File::for_uri(uri).path()?;
    let size = std::fs::metadata(&path).ok()?.len();
    if size > MAX_ICON_BYTES {
        return None;
    }
    let file = gio::File::for_path(&path);
    let decoded =
        picture_guard::decode_to_fit(&file, ICON_PIXELS, ICON_PIXELS, Limits::DISPLAY, None).ok()?;
    Some(gdk::Texture::for_pixbuf(&decoded.pixbuf))
}
