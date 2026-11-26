// SPDX-License-Identifier: AGPL-3.0-only
//! Inline muted video preview on tile hover: after a short hover delay, plays
//! a few seconds from the start, 25%, 50%, and 75% of the video inside the
//! tile, then restores the static thumbnail.
//!
//! Only one `gtk::MediaFile` is ever active across the application, and it is
//! torn down immediately on pointer leave, scroll, zoom, or cell unbind so
//! browsing and memory usage stay unaffected.
//!
//! Re-entrancy invariant: `ACTIVE` is NEVER held borrowed across any GTK or
//! `GStreamer` call (such as `play`, `pause`, `seek`,
//! `clear`), and `stream.clear()` is deferred to an idle callback after all
//! signal handlers are disconnected.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gstreamer as gst;
use gstreamer_pbutils as pbutils;
use gstreamer_pbutils::prelude::DiscovererStreamInfoExt;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};

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
    aspect: Option<f64>,
    frame: Option<gtk::AspectFrame>,
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
    if let (Some(frame), Some(cell)) = (active.frame, active.cell.upgrade()) {
        let imp = cell.imp();
        imp.icon_frame.remove_overlay(&frame);
        imp.custom_icon.set_opacity(1.0);
        imp.image
            .set_opacity(if imp.custom_icon.is_visible() { 0.0 } else { 1.0 });
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

/// GTK sizes its video paintable using the stored frame dimensions, which
/// can differ from the intended display shape when the pixels are non-square.
fn display_aspect(path: &Path) -> Option<f64> {
    gst::init().ok()?;
    let uri = gio::File::for_path(path).uri();
    let info = pbutils::Discoverer::new(gst::ClockTime::from_seconds(2))
        .ok()?
        .discover_uri(&uri)
        .ok()?;
    let video = info.video_streams().into_iter().find(|video| !video.is_image())?;
    let par = video.par();
    if video.width() == 0 || video.height() == 0 || par.numer() <= 0 {
        return None;
    }
    let aspect = f64::from(video.width()) * f64::from(par.numer())
        / (f64::from(video.height()) * f64::from(par.denom()));
    let orientation_of = |tags: gst::TagList| {
        tags.get::<gst::tags::ImageOrientation>()
            .map(|tag| tag.get().to_owned())
    };
    let orientation = video
        .tags()
        .and_then(orientation_of)
        .or_else(|| info.tags().and_then(orientation_of));
    let aspect = match orientation.as_deref() {
        Some("rotate-90" | "rotate-270" | "flip-rotate-90" | "flip-rotate-270") => 1.0 / aspect,
        _ => aspect,
    };
    Some(aspect)
}

fn preview_frame(stream: &gtk::MediaFile, aspect: f64, edge: i32) -> gtk::AspectFrame {
    let picture = gtk::Picture::builder()
        .paintable(stream)
        .content_fit(gtk::ContentFit::Fill)
        .can_target(false)
        .build();
    // GtkAspectFrame fits and centers the video inside the existing icon box.
    // Keep the shorter side at least one pixel wide.
    #[allow(clippy::cast_possible_truncation)]
    let ratio = aspect.clamp(1.0 / f64::from(edge), f64::from(edge)) as f32;
    gtk::AspectFrame::builder()
        .obey_child(false)
        .ratio(ratio)
        .width_request(edge)
        .height_request(edge)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .can_target(false)
        .child(&picture)
        .build()
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
        self.connect_unmap(FileCell::stop_video_hover);
    }

    /// Records whether `item` is eligible for inline hover video preview.
    pub(super) fn set_hover_video(&self, item: &FileItem, previews: PreviewPolicy) {
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
        let frame_id = stream.connect_invalidate_contents(move |stream| {
            begin_segments(token, stream);
        });

        ACTIVE.with_borrow_mut(|slot| {
            *slot = Some(ActivePreview {
                token,
                cell: self.downgrade(),
                stream: stream.clone(),
                handlers: vec![error_id, ended_id, prepared_id, frame_id],
                aspect: None,
                frame: None,
                timer: None,
            });
        });

        glib::spawn_future_local(async move {
            let Ok(Some(aspect)) = gio::spawn_blocking(move || display_aspect(&path)).await else {
                stop_if_token(token);
                return;
            };
            let stream = ACTIVE.with_borrow_mut(|slot| {
                let active = slot.as_mut().filter(|active| active.token == token)?;
                active.aspect = Some(aspect);
                Some(active.stream.clone())
            });
            if let Some(stream) = stream {
                // Keep the thumbnail until the stream has a frame to show.
                stream.play();
                begin_segments(token, &stream);
            }
        });
    }
}

fn begin_segments(token: u64, stream: &gtk::MediaFile) {
    if !stream.is_prepared() {
        return;
    }
    if !stream.has_video() || stream.error().is_some() {
        stop_if_token(token);
        return;
    }
    let target = ACTIVE.with_borrow(|slot| {
        let active = slot
            .as_ref()
            .filter(|active| active.token == token && active.frame.is_none())?;
        Some((active.cell.upgrade()?, active.aspect?))
    });
    let Some((cell, aspect)) = target else {
        return;
    };
    if !cell.is_mapped() || stream.intrinsic_width() == 0 {
        return;
    }

    let imp = cell.imp();
    let frame = preview_frame(stream, aspect, imp.icon_size.get());
    let still_active = ACTIVE.with_borrow_mut(|slot| {
        let Some(active) = slot.as_mut().filter(|active| active.token == token) else {
            return false;
        };
        active.frame = Some(frame.clone());
        true
    });
    if !still_active {
        return;
    }

    imp.icon_frame.add_overlay(&frame);
    frame.insert_after(&imp.icon_frame, Some(&imp.custom_icon));
    imp.custom_icon.set_opacity(0.0);
    imp.image.set_opacity(0.0);

    // Re-check token in case mutating widget visibility triggered a leave/unmap.
    let still_active = ACTIVE.with_borrow(|slot| slot.as_ref().is_some_and(|a| a.token == token));
    if !still_active {
        return;
    }

    let segments = segment_starts_us(stream.duration(), stream.is_seekable());
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
    let mut pending = Some(timer);
    let replaced = ACTIVE.with_borrow_mut(|slot| {
        let active = slot.as_mut().filter(|active| active.token == token)?;
        std::mem::replace(&mut active.timer, pending.take())
    });
    if let Some(timer) = replaced.or(pending) {
        timer.remove();
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

    #[gtk::test]
    fn hover_preview_fits_the_display_aspect_of_anamorphic_video() {
        use crate::media_details::tests::encode;
        use crate::test_support::harness::wait_until;

        gst::init().expect("GStreamer");
        let directory = tempfile::tempdir().expect("a fixture directory");
        let path = directory.path().join("portrait.ogv");
        encode(
            &format!(
                "videotestsrc num-buffers=60 ! \
                 video/x-raw,width=720,height=480,pixel-aspect-ratio=240/631,framerate=15/1 ! \
                 theoraenc ! oggmux name=mux ! filesink location=\"{}\" \
                 audiotestsrc wave=silence num-buffers=173 ! vorbisenc name=encoder ! mux.",
                path.display()
            ),
            "encoder",
            &gst::TagList::new(),
        );
        let cell = FileCell::new(super::super::CellLayout::IconTile, 96);
        let window = gtk::Window::builder().child(&cell).build();
        window.present();
        wait_until("the video tile to map", || cell.is_mapped());
        cell.start_video_hover(path);

        let shown_frame = || ACTIVE.with_borrow(|slot| slot.as_ref()?.frame.clone());
        wait_until("the hover video to be allocated", || {
            shown_frame()
                .and_then(|frame| frame.child())
                .is_some_and(|picture| picture.width() > 0)
        });
        let frame = shown_frame().expect("the live preview");
        let picture = frame.child().expect("the video picture");
        let ratio = f64::from(picture.width()) / f64::from(picture.height());
        assert!((ratio - 360.0 / 631.0).abs() < 0.015, "preview ratio was {ratio}");
        assert!(cell.imp().image.opacity() < f64::EPSILON);
        cell.stop_video_hover();
        assert!(frame.parent().is_none());
        assert!((1.0 - cell.imp().image.opacity()).abs() < f64::EPSILON);
        window.close();
    }
}
