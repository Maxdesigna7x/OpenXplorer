// SPDX-License-Identifier: AGPL-3.0-only
//! Retained GTK render nodes for folder zoom. The new layout is installed once;
//! the frame clock only changes how the two views are drawn.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};

use super::{FolderPane, FolderView, PanePage};

#[derive(Debug)]
pub(in crate::window) struct Zoom {
    before: gsk::RenderNode,
    bounds: graphene::Rect,
    scale: f32,
    started: Cell<Option<i64>>,
    duration_us: i64,
    progress: Cell<f32>,
    retargets: u8,
    tick: Option<gtk::TickCallbackId>,
}

impl FolderPane {
    pub(in crate::window) fn capture_zoom(
        &self,
        from: FolderView,
        to: FolderView,
        duration_ms: u32,
    ) -> Option<Zoom> {
        if self.page() != Some(PanePage::Listing)
            || !self.is_mapped()
            || !self.settings().is_gtk_enable_animations()
        {
            return None;
        }
        let bounds = self.bounds_for_zoom()?;
        let snapshot = gtk::Snapshot::new();
        let retargets = self
            .imp()
            .zoom
            .borrow()
            .as_ref()
            .map_or(0, |zoom| zoom.retargets + 1);
        // Repeated wheel events can capture a frame that already blends
        // two views. Bound the depth of that retained render tree.
        let (source, retargets) = if retargets >= 3 {
            (self.first_child()?, 0)
        } else {
            (self.clone().upcast(), retargets)
        };
        let paintable = gtk::WidgetPaintable::new(Some(&source));
        paintable.snapshot(&snapshot, f64::from(bounds.width()), f64::from(bounds.height()));
        let before = snapshot.to_node()?;
        Some(Zoom {
            before,
            bounds,
            scale: icon_scale(from, to),
            started: Cell::new(None),
            duration_us: i64::from(duration_ms) * 1_000,
            progress: Cell::new(0.0),
            retargets,
            tick: None,
        })
    }

    pub(in crate::window) fn animate_zoom(&self, mut zoom: Zoom) {
        self.stop_zoom_animation();
        zoom.tick = Some(self.add_tick_callback(|pane, clock| {
            let keep_going = pane.imp().zoom.borrow().as_ref().is_some_and(|zoom| {
                pane.settings().is_gtk_enable_animations()
                    && pane.bounds_for_zoom().as_ref() == Some(&zoom.bounds)
                    && zoom.advance(clock.frame_time())
            });
            if !keep_going {
                // Returning Break removes the current callback.
                pane.imp().zoom.take();
            }
            pane.queue_draw();
            if keep_going {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        }));
        self.imp().zoom.replace(Some(zoom));
        self.queue_draw();
    }

    pub(super) fn stop_zoom_animation(&self) {
        if let Some(tick) = self.imp().zoom.take().and_then(|zoom| zoom.tick) {
            tick.remove();
            self.queue_draw();
        }
    }

    #[expect(clippy::cast_precision_loss, reason = "window dimensions are small")]
    fn bounds_for_zoom(&self) -> Option<graphene::Rect> {
        let (width, height) = (self.width(), self.height());
        (width > 0 && height > 0).then(|| graphene::Rect::new(0.0, 0.0, width as f32, height as f32))
    }
}

impl Zoom {
    #[expect(
        clippy::cast_precision_loss,
        reason = "elapsed is at most 500 milliseconds here"
    )]
    fn advance(&self, time: i64) -> bool {
        let start = self.started.get().unwrap_or_else(|| {
            self.started.set(Some(time));
            time
        });
        let elapsed = time.saturating_sub(start);
        if elapsed >= self.duration_us {
            return false;
        }
        let fraction = elapsed as f32 / self.duration_us as f32;
        self.progress.set(1.0 - (1.0 - fraction).powi(3));
        true
    }

    pub(super) fn snapshot(&self, pane: &FolderPane, child: &gtk::Widget, snapshot: &gtk::Snapshot) {
        let progress = self.progress.get();
        snapshot.push_clip(&self.bounds);
        snapshot.push_cross_fade(f64::from(progress));
        self.with_scale(snapshot, 1.0 + (self.scale - 1.0) * progress, || {
            snapshot.append_node(&self.before);
        });
        snapshot.pop();
        self.with_scale(
            snapshot,
            1.0 + (self.scale.recip() - 1.0) * (1.0 - progress),
            || {
                pane.snapshot_child(child, snapshot);
            },
        );
        snapshot.pop();
        snapshot.pop();
    }

    fn with_scale(&self, snapshot: &gtk::Snapshot, scale: f32, draw: impl FnOnce()) {
        snapshot.save();
        snapshot.translate(&graphene::Point::new(
            self.bounds.width() * (1.0 - scale) * 0.5,
            self.bounds.height() * (1.0 - scale) * 0.5,
        ));
        snapshot.scale(scale, scale);
        draw();
        snapshot.restore();
    }
}

#[expect(clippy::cast_precision_loss, reason = "icon sizes are a few hundred pixels")]
fn icon_scale(from: FolderView, to: FolderView) -> f32 {
    match (from, to) {
        (FolderView::Icons(from), FolderView::Icons(to)) => {
            (to.pixels() as f32 / from.pixels() as f32).clamp(0.8, 1.25)
        }
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::harness::{wait_for_frames, wait_until, Fixture, TestWindow};

    #[gtk::test]
    fn zoom_animates_the_real_listing_and_retargets() {
        let fixture = Fixture::standard();
        let test = TestWindow::open(&fixture.uri());
        let pane = test.window.folder_pane();
        let settings = pane.settings();
        let previous = settings.is_gtk_enable_animations();
        settings.set_gtk_enable_animations(true);
        wait_for_frames(pane, 2);

        test.window.zoom_view(7);
        assert!(pane.imp().zoom.borrow().is_some());
        wait_for_frames(pane, 2);
        test.window.zoom_view(1);
        assert!(pane.imp().zoom.borrow().is_some());
        wait_until("the animation to finish", || pane.imp().zoom.borrow().is_none());

        settings.set_gtk_enable_animations(false);
        test.window.zoom_view(-1);
        assert!(pane.imp().zoom.borrow().is_none());
        settings.set_gtk_enable_animations(previous);
    }
}
