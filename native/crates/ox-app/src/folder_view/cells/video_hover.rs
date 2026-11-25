// SPDX-License-Identifier: AGPL-3.0-only
//! Inline muted video preview on tile hover: after a short hover delay, plays
//! a few seconds from the start, 25%, 50%, and 75% of the video inside the
//! tile's picture widget, then restores the original static thumbnail.
//!
//! Only one `gtk::MediaFile` is ever active across the application, and it is
//! torn down immediately on pointer leave, scroll, zoom, or cell unbind so
//! browsing and memory usage stay unaffected.
//!
//! Re-entrancy invariant: `ACTIVE` is NEVER held borrowed across any GTK or
//! GStreamer call (`set_paintable`, `set_visible`, `play`, `pause`, `seek`,
//! `clear`), and `stream.clear()` is deferred to an idle callback after all
//! signal handlers are disconnected.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use crate::folder_view::item::FileItem;
use crate::thumbnails::PreviewPolicy;

use super::FileCell;

/// Minimum logical icon size that shows an inline video preview on hover
/// (Medium icons, Large, Extra large, and Tiles; excludes 16 px Details/List).
const SMALLEST_VIDEO_HOVER_ICON: i32 = 48;

/// How long the pointer must rest over a video tile before loading playback,
/// so sweeping the cursor across a folder triggers no decoders.
const HOVER_DELAY: Duration = Duration::from_millis(380);

/// How long each preview segment (0%, 25%, 50%, 75%) plays.
const SEGMENT_DURATION: Duration = Duration::from_millis(2_500);

/// Shortest video duration (10 s) that steps through 0%, 25%, 50% and 75%;
/// shorter clips play straight through once and then restore the thumbnail.
const MIN_MULTI_SEGMENT_US: i64 = 10_000_000;

/// Maximum time a short or unseekable clip may play before restoring the
/// original thumbnail.
const MAX_SINGLE_PLAY_DURATION: Duration = Duration::from_secs(10);

struct ActivePreview {
    token: u64,
    cell: glib::WeakRef<FileCell>,
    stream: gtk::MediaFile,
    handlers: Vec<glib::SignalHandlerId>,
    swapped: bool,
    saved_paintable: Option<gdk::Paintable>,
    saved_visible: bool,
    saved_image_opacity: f64,
    timer: Option<glib::SourceId>,
}

thread_local! {
    static NEXT_TOKEN: Cell<u64> = const { Cell::new(1) };
    static ACTIVE: RefCell<Option<ActivePreview>> = const { RefCell::new(None) };
}

/// Stops any active hover video preview and restores its tile's original
/// static thumbnail.
pub(crate) fn stop_active_video_preview() {
    let active = ACTIVE.with_borrow_mut(Option::take);
    if let Some(active) = active {
        teardown(active);
    }
}

fn stop_if_token(token: u64) {
    let active = ACTIVE.with_borrow_mut(|slot| {
        if slot.as_ref().is_some_and(|active| active.token == token) {
            slot.take()
        } else {
            None
        }
    });
    if let Some(active) = active {
        teardown(active);
    }
}

fn stop_if_cell(cell: &FileCell) {
    let active = ACTIVE.with_borrow_mut(|slot| {
        if slot
            .as_ref()
            .and_then(|active| active.cell.upgrade())
            .is_some_and(|owner| owner == *cell)
        {
            slot.take()
        } else {
            None
        }
    });
    if let Some(active) = active {
        teardown(active);
    }
}

fn teardown(mut active: ActivePreview) {
    if let Some(timer) = active.timer.take() {
        timer.remove();
    }
    for handler in active.handlers.drain(..) {
        active.stream.disconnect(handler);
    }
    active.stream.pause();
    if active.swapped {
        if let Some(cell) = active.cell.upgrade() {
            let imp = cell.imp();
            let still_ours = imp.custom_icon.paintable().is_some_and(|paintable| {
                paintable.as_ptr() == active.stream.upcast_ref::<gdk::Paintable>().as_ptr()
            });
            if still_ours {
                imp.custom_icon.set_paintable(active.saved_paintable.as_ref());
                imp.custom_icon.set_visible(active.saved_visible);
                imp.image.set_opacity(active.saved_image_opacity);
            }
        }
    }
    // Defer GStreamer pipeline teardown until the current GTK/GStreamer
    // callback stack has unwound so rapid pointer leave/enter never races
    // inside `GtkGstMediaFile`.
    let stream = active.stream;
    glib::idle_add_local_once(move || {
        stream.clear();
    });
}

/// Computes the seek offsets (in microseconds) for the preview segments.
/// Videos at least 10 seconds long that support seeking play 0%, 25%, 50%,
/// and 75%; shorter or unseekable videos return a single `[0]` segment.
fn segment_starts_us(duration_us: i64, seekable: bool) -> Vec<i64> {
    if seekable && duration_us >= MIN_MULTI_SEGMENT_US {
        let quarter = duration_us / 4;
        vec![0, quarter, quarter * 2, quarter * 3]
    } else {
        vec![0]
    }
}

impl FileCell {
    /// Installs pointer enter/leave hooks so resting the cursor over a video
    /// tile plays a muted preview and leaving restores its thumbnail.
    pub(super) fn install_video_hover(&self) {
        let motion = gtk::EventControllerMotion::new();
        motion.connect_enter(glib::clone!(
            #[weak(rename_to = cell)]
            self,
            move |_, _, _| cell.schedule_video_hover()
        ));
        motion.connect_leave(glib::clone!(
            #[weak(rename_to = cell)]
            self,
            move |_| cell.stop_video_hover()
        ));
        self.add_controller(motion);
        self.connect_unmap(|cell| cell.stop_video_hover());
    }

    /// Records whether `item` is eligible for inline hover video preview.
    pub(super) fn set_hover_video(&self, item: &FileItem, previews: PreviewPolicy) {
        self.stop_video_hover();
        let entry = item.entry();
        let eligible = self.imp().icon_size.get() >= SMALLEST_VIDEO_HOVER_ICON
            && previews.shown
            && previews.videos
            && !entry.is_dir
            && !entry.is_virtual
            && entry
                .content_type
                .as_deref()
                .is_some_and(|content_type| content_type.starts_with("video/"))
            && entry.uri.starts_with("file:");
        let path = eligible.then(|| gio::File::for_uri(&entry.uri).path()).flatten();
        self.imp().hover_video.replace(path);
    }

    /// Cancels any pending hover timer and stops playback if this cell owns it.
    pub(super) fn stop_video_hover(&self) {
        if let Some(delay) = self.imp().hover_delay.take() {
            delay.remove();
        }
        stop_if_cell(self);
    }

    fn schedule_video_hover(&self) {
        if let Some(delay) = self.imp().hover_delay.take() {
            delay.remove();
        }
        let Some(path) = self.imp().hover_video.borrow().clone() else {
            return;
        };
        if !self.is_mapped() {
            return;
        }
        let delay = glib::timeout_add_local_once(
            HOVER_DELAY,
            glib::clone!(
                #[weak(rename_to = cell)]
                self,
                move || {
                    cell.imp().hover_delay.take();
                    if cell.is_mapped() {
                        cell.start_video_hover(path);
                    }
                }
            ),
        );
        self.imp().hover_delay.replace(Some(delay));
    }

    fn start_video_hover(&self, path: PathBuf) {
        stop_active_video_preview();
        let token = NEXT_TOKEN.with(|next| {
            let value = next.get();
            next.set(value.wrapping_add(1).max(1));
            value
        });
        let stream = gtk::MediaFile::for_filename(&path);
        stream.set_muted(true);
        stream.set_volume(0.0);
        stream.set_loop(false);

        let error_id = stream.connect_error_notify(move |stream| {
            if stream.error().is_some() {
                stop_if_token(token);
            }
        });
        let ended_id = stream.connect_ended_notify(move |stream| {
            if stream.is_ended() {
                stop_if_token(token);
            }
        });
        let prepared_id = stream.connect_prepared_notify(move |stream| {
            if stream.is_prepared() {
                begin_segments(token, stream);
            }
        });

        ACTIVE.with_borrow_mut(|slot| {
            *slot = Some(ActivePreview {
                token,
                cell: self.downgrade(),
                stream: stream.clone(),
                handlers: vec![error_id, ended_id, prepared_id],
                swapped: false,
                saved_paintable: None,
                saved_visible: false,
                saved_image_opacity: 1.0,
                timer: None,
            });
        });

        if stream.is_prepared() {
            begin_segments(token, &stream);
        } else {
            // Trigger GStreamer preroll; keep the static thumbnail visible
            // until `is_prepared` fires so there is no black flash.
            stream.play();
        }
    }
}

fn begin_segments(token: u64, stream: &gtk::MediaFile) {
    if !stream.has_video() || stream.error().is_some() {
        stop_if_token(token);
        return;
    }
    let segments = segment_starts_us(stream.duration(), stream.is_seekable());
    let cell = ACTIVE.with_borrow_mut(|slot| {
        let active = slot.as_mut().filter(|active| active.token == token)?;
        if active.swapped {
            return None;
        }
        let cell = active.cell.upgrade().filter(WidgetExt::is_mapped)?;
        active.swapped = true;
        Some(cell)
    });
    let Some(cell) = cell else {
        return;
    };

    // Snapshot the current thumbnail state and store it before mutating any
    // GTK widget properties, keeping `ACTIVE` unborrowed during GTK calls.
    let imp = cell.imp();
    let saved_paintable = imp.custom_icon.paintable();
    let saved_visible = imp.custom_icon.is_visible();
    let saved_image_opacity = imp.image.opacity();
    let still_active = ACTIVE.with_borrow_mut(|slot| {
        let Some(active) = slot.as_mut().filter(|active| active.token == token) else {
            return false;
        };
        active.saved_paintable = saved_paintable;
        active.saved_visible = saved_visible;
        active.saved_image_opacity = saved_image_opacity;
        true
    });
    if !still_active {
        return;
    }

    imp.custom_icon.set_paintable(Some(stream));
    imp.custom_icon.set_visible(true);
    imp.image.set_opacity(0.0);

    // Re-check token in case mutating widget visibility triggered a leave/unmap.
    let still_active = ACTIVE.with_borrow(|slot| slot.as_ref().is_some_and(|a| a.token == token));
    if !still_active {
        return;
    }

    stream.play();
    schedule_next_segment(token, segments, 1);
}

fn schedule_next_segment(token: u64, segments: Vec<i64>, next_index: usize) {
    let wait = if segments.len() > 1 {
        SEGMENT_DURATION
    } else {
        MAX_SINGLE_PLAY_DURATION
    };
    let timer = glib::timeout_add_local_once(wait, move || {
        let next_seek = ACTIVE.with_borrow_mut(|slot| {
            let active = slot.as_mut().filter(|active| active.token == token)?;
            active.timer.take();
            if next_index < segments.len() {
                Some((active.stream.clone(), segments[next_index]))
            } else {
                None
            }
        });
        if let Some((stream, position_us)) = next_seek {
            if stream.is_prepared() && stream.is_seekable() && stream.error().is_none() {
                stream.seek(position_us);
                stream.play();
                schedule_next_segment(token, segments, next_index + 1);
            } else {
                stop_if_token(token);
            }
        } else {
            stop_if_token(token);
        }
    });
    let replaced = ACTIVE.with_borrow_mut(|slot| {
        if let Some(active) = slot.as_mut().filter(|active| active.token == token) {
            Some(active.timer.replace(timer))
        } else {
            None
        }
    });
    match replaced {
        Some(Some(previous)) => previous.remove(),
        Some(None) => {}
        None => {
            // The preview stopped before we could store the timer; it will
            // no-op when it fires because `token` no longer matches.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_seekable_videos_step_through_quarters_and_short_ones_play_once() {
        assert_eq!(
            segment_starts_us(40_000_000, true),
            vec![0, 10_000_000, 20_000_000, 30_000_000]
        );
        assert_eq!(segment_starts_us(8_000_000, true), vec![0]);
        assert_eq!(segment_starts_us(40_000_000, false), vec![0]);
    }
}
