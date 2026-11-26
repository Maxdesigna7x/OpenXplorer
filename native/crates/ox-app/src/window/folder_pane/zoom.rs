// SPDX-License-Identifier: AGPL-3.0-only
//! Animate the visible files between their old and new layout rectangles.
//! GTK lays out the destination once. The frame clock then moves its tiles
//! individually with retained render nodes.

use std::cell::{Cell, OnceCell};
use std::collections::HashMap;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::folder_view::item::FileItem;

use super::{FolderPane, FolderView, PanePage};

#[derive(Clone, Debug)]
struct Tile {
    item: FileItem,
    position: u32,
    rect: graphene::Rect,
    image_size: graphene::Size,
    image: gsk::RenderNode,
}

#[derive(Debug)]
struct MovingTile {
    tile: Tile,
    from: graphene::Rect,
}

#[derive(Debug)]
struct Reflow {
    backdrop: gsk::RenderNode,
    viewport: graphene::Rect,
    tiles: Vec<MovingTile>,
}

#[derive(Debug)]
pub(in crate::window) struct Zoom {
    before: Vec<Tile>,
    before_frame: Option<gsk::RenderNode>,
    reflow: OnceCell<Reflow>,
    started: Cell<Option<i64>>,
    duration_us: i64,
    first_frame_us: i64,
    progress: Cell<f64>,
    tick: Option<gtk::TickCallbackId>,
}

struct VisibleCell {
    item: FileItem,
    position: u32,
    rect: graphene::Rect,
    cell: gtk::Widget,
}

impl FolderPane {
    pub(in crate::window) fn capture_zoom(&self, duration_ms: u32) -> Option<Zoom> {
        crate::folder_view::stop_active_video_preview();
        if self.page() != Some(PanePage::Listing)
            || !self.is_mapped()
            || !self.settings().is_gtk_enable_animations()
        {
            return None;
        }
        let bounds = self.bounds_for_zoom()?;
        let retargeting = self.imp().zoom.borrow().is_some();
        let before = self
            .imp()
            .zoom
            .borrow()
            .as_ref()
            .map(Zoom::drawn_tiles)
            .unwrap_or_else(|| self.visible_zoom_tiles(&self.zoom_viewport(&bounds)));
        if before.is_empty() {
            return None;
        }
        let before_frame = if retargeting {
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(self)).snapshot(
                &snapshot,
                f64::from(bounds.width()),
                f64::from(bounds.height()),
            );
            snapshot.to_node()
        } else {
            self.first_child().and_then(|child| {
                let snapshot = gtk::Snapshot::new();
                self.snapshot_child(&child, &snapshot);
                snapshot.to_node()
            })
        };
        Some(Zoom {
            before,
            before_frame,
            reflow: OnceCell::new(),
            started: Cell::new(None),
            duration_us: i64::from(duration_ms) * 1_000,
            first_frame_us: 0,
            progress: Cell::new(0.0),
            tick: None,
        })
    }

    pub(in crate::window) fn animate_zoom(&self, mut zoom: Zoom) {
        self.stop_zoom_animation();
        if zoom.first_frame_us == 0 {
            zoom.first_frame_us = self.first_frame_step_us(zoom.duration_us);
        }
        zoom.tick = Some(self.add_tick_callback(|pane, clock| {
            let keep_going = pane.imp().zoom.borrow().as_ref().is_some_and(|zoom| {
                pane.settings().is_gtk_enable_animations()
                    && pane.is_mapped()
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

    pub(in crate::window) fn animate_resize(&self, mut zoom: Zoom) {
        // During a live resize a new target can arrive every frame. Give
        // each target one frame of movement so that tiles keep following
        // the window edge while the pointer is still moving.
        zoom.first_frame_us = self.first_frame_step_us(zoom.duration_us);
        self.animate_zoom(zoom);
    }

    fn first_frame_step_us(&self, duration_us: i64) -> i64 {
        self.frame_clock().map_or(0, |clock| {
            clock.refresh_info(clock.frame_time()).0.clamp(0, duration_us / 2)
        })
    }

    pub(in crate::window) fn stop_zoom_animation(&self) {
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

    fn zoom_viewport(&self, bounds: &graphene::Rect) -> graphene::Rect {
        let view = self.view_widget();
        let mut viewport = view
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_then(|scroll| scroll.compute_bounds(self))
            .and_then(|rect| rect.intersection(bounds))
            .unwrap_or(*bounds);
        if self.view() == FolderView::Details {
            let header = std::iter::successors(view.first_child(), WidgetExt::next_sibling)
                .find(|widget| widget.css_name() == "header");
            if let Some(rect) = header.and_then(|header| header.compute_bounds(self)) {
                let top = (rect.y() + rect.height()).clamp(viewport.y(), viewport.y() + viewport.height());
                viewport = graphene::Rect::new(
                    viewport.x(),
                    top,
                    viewport.width(),
                    viewport.y() + viewport.height() - top,
                );
            }
        }
        viewport
    }

    fn visible_zoom_cells(&self, viewport: &graphene::Rect) -> Vec<VisibleCell> {
        let view = self.view_widget();
        let details = self.view() == FolderView::Details;
        let mut cells: Vec<_> = self
            .owners()
            .shown_items(&view)
            .into_iter()
            .filter_map(|(position, row)| {
                let cell = if details {
                    self.owners().file_cell_at(position, &view)?.upcast()
                } else {
                    row
                };
                let rect = cell.compute_bounds(self)?;
                let visible = rect.intersection(viewport)?;
                if !cell.is_mapped() || visible.width() <= 0.0 || visible.height() <= 0.0 {
                    return None;
                }
                let item = self.model().item(position)?;
                Some(VisibleCell {
                    item,
                    position,
                    rect,
                    cell,
                })
            })
            .collect();
        cells.sort_unstable_by_key(|cell| cell.position);
        cells
    }

    fn visible_zoom_tiles(&self, viewport: &graphene::Rect) -> Vec<Tile> {
        self.visible_zoom_cells(viewport)
            .into_iter()
            .filter_map(|visible| {
                let image = snapshot_cell(&visible.cell, &visible.rect)?;
                let image_size = graphene::Size::new(visible.rect.width(), visible.rect.height());
                Some(Tile {
                    item: visible.item,
                    position: visible.position,
                    rect: visible.rect,
                    image_size,
                    image,
                })
            })
            .collect()
    }
}

impl Zoom {
    fn advance(&self, time: i64) -> bool {
        let start = self.started.get().unwrap_or_else(|| {
            let start = time.saturating_sub(self.first_frame_us);
            self.started.set(Some(start));
            start
        });
        let elapsed = time.saturating_sub(start);
        if elapsed >= self.duration_us {
            return false;
        }
        self.progress.set(self.eased_progress(elapsed));
        true
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "elapsed is at most 500 milliseconds here"
    )]
    fn eased_progress(&self, elapsed: i64) -> f64 {
        let fraction = elapsed as f64 / self.duration_us as f64;
        1.0 - (1.0 - fraction).powi(3)
    }

    // A second wheel step begins at the rectangles on screen, even when
    // the previous animation has only drawn part of its journey.
    fn drawn_tiles(&self) -> Vec<Tile> {
        self.reflow.get().map_or_else(
            || self.before.clone(),
            |reflow| {
                reflow
                    .tiles
                    .iter()
                    .filter_map(|moving| {
                        let rect = moving.rect_at(self.progress.get());
                        rect.intersection(&reflow.viewport)?;
                        let mut tile = moving.tile.clone();
                        tile.rect = rect;
                        Some(tile)
                    })
                    .collect()
            },
        )
    }

    pub(super) fn snapshot(&self, pane: &FolderPane, child: &gtk::Widget, snapshot: &gtk::Snapshot) {
        if self.reflow.get().is_none() {
            if let Some(reflow) = self.prepare_reflow(pane, child) {
                let _ = self.reflow.set(reflow);
                let started = pane
                    .frame_clock()
                    .map(|clock| clock.frame_time().saturating_sub(self.first_frame_us));
                self.started.set(started);
                self.progress.set(self.eased_progress(self.first_frame_us));
            }
        }
        let Some(reflow) = self.reflow.get() else {
            if let (Some(before), Some(bounds)) = (&self.before_frame, pane.bounds_for_zoom()) {
                snapshot.push_clip(&bounds);
                snapshot.append_node(before);
                snapshot.pop();
            } else {
                pane.snapshot_child(child, snapshot);
            }
            return;
        };
        snapshot.append_node(&reflow.backdrop);
        snapshot.push_clip(&reflow.viewport);
        let progress = self.progress.get();
        for moving in &reflow.tiles {
            moving.draw(snapshot, progress, &reflow.viewport);
        }
        snapshot.pop();
    }

    fn prepare_reflow(&self, pane: &FolderPane, child: &gtk::Widget) -> Option<Reflow> {
        let bounds = pane.bounds_for_zoom()?;
        let viewport = pane.zoom_viewport(&bounds);
        let after = pane.visible_zoom_cells(&viewport);
        let first = after.first()?.position;
        let mut old: HashMap<_, _> = self
            .before
            .iter()
            .map(|tile| (tile.item.entry().uri.as_str(), tile))
            .collect();
        // A just-recycled list can still report the previous allocation.
        // Wait for the next paint until GTK reports the new geometry.
        if after.len() == old.len()
            && after.iter().all(|cell| {
                old.get(cell.item.entry().uri.as_str())
                    .is_some_and(|old| old.rect == cell.rect)
            })
        {
            return None;
        }
        // Draw the destination once so newly entering cells have current
        // paintables at their final allocation and we can build the backdrop.
        let base = gtk::Snapshot::new();
        pane.snapshot_child(child, &base);
        let base = base.to_node()?;
        let mask = gtk::Snapshot::new();
        for cell in &after {
            if let Some(rect) = cell.rect.intersection(&viewport) {
                mask.append_node(gsk::ColorNode::new(&gdk::RGBA::WHITE, &rect));
            }
        }
        let masked = gsk::MaskNode::new(base, mask.to_node()?, gsk::MaskMode::InvertedAlpha);
        let backdrop = gtk::Snapshot::new();
        #[allow(
            deprecated,
            reason = "GTK 4.14 looks up the theme's named background this way"
        )]
        if let Some(background) = pane.style_context().lookup_color("ox_bg") {
            backdrop.append_node(gsk::ColorNode::new(&background, &viewport));
        }
        backdrop.append_node(masked);
        let backdrop = cache_backdrop(pane, backdrop.to_node()?, &bounds);
        let old_first = self
            .before
            .iter()
            .map(|tile| tile.position)
            .min()
            .unwrap_or(first);
        let mut tiles: Vec<_> = after
            .into_iter()
            .filter_map(|visible| {
                if let Some(previous) = old.remove(visible.item.entry().uri.as_str()) {
                    // The destination can still be loading a thumbnail at
                    // its new size. Move the artwork the user already saw
                    // without snapshotting the destination cell.
                    let tile = Tile {
                        item: visible.item,
                        position: visible.position,
                        rect: visible.rect,
                        image_size: previous.image_size,
                        image: previous.image.clone(),
                    };
                    Some(MovingTile {
                        tile,
                        from: previous.rect,
                    })
                } else {
                    let image = snapshot_cell(&visible.cell, &visible.rect)?;
                    let tile = Tile {
                        item: visible.item,
                        position: visible.position,
                        rect: visible.rect,
                        image_size: graphene::Size::new(visible.rect.width(), visible.rect.height()),
                        image,
                    };
                    let from = tile.offscreen(&viewport, old_first);
                    Some(MovingTile { tile, from })
                }
            })
            .collect();
        for previous in &self.before {
            if old.remove(previous.item.entry().uri.as_str()).is_some() {
                let mut tile = previous.clone();
                tile.rect = previous.offscreen(&viewport, first);
                tiles.push(MovingTile {
                    tile,
                    from: previous.rect,
                });
            }
        }
        Some(Reflow {
            backdrop,
            viewport,
            tiles,
        })
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "display scale factors are small integers"
)]
fn cache_backdrop(pane: &FolderPane, backdrop: gsk::RenderNode, bounds: &graphene::Rect) -> gsk::RenderNode {
    let Some(renderer) = pane.native().and_then(|native| native.renderer()) else {
        return backdrop;
    };
    let scale = pane.scale_factor().max(1) as f32;
    let (node, render_bounds) = if scale > 1.0 {
        let transform = gsk::Transform::new().scale(scale, scale);
        (
            gsk::TransformNode::new(&backdrop, Some(&transform)).upcast(),
            graphene::Rect::new(
                bounds.x() * scale,
                bounds.y() * scale,
                bounds.width() * scale,
                bounds.height() * scale,
            ),
        )
    } else {
        (backdrop.clone(), *bounds)
    };
    let texture = renderer.render_texture(&node, Some(&render_bounds));
    gsk::TextureNode::new(&texture, bounds).upcast()
}

fn snapshot_cell(cell: &gtk::Widget, rect: &graphene::Rect) -> Option<gsk::RenderNode> {
    snapshot_allocated_cell(cell).or_else(|| {
        let snapshot = gtk::Snapshot::new();
        gtk::WidgetPaintable::new(Some(cell)).snapshot(
            &snapshot,
            f64::from(rect.width()),
            f64::from(rect.height()),
        );
        snapshot.to_node()
    })
}

fn snapshot_allocated_cell(cell: &gtk::Widget) -> Option<gsk::RenderNode> {
    let parent = cell.parent()?;
    let zero = graphene::Point::new(0.0, 0.0);
    let origin = cell.compute_point(&parent, &zero)?;
    let snapshot = gtk::Snapshot::new();
    parent.snapshot_child(cell, &snapshot);
    let node = snapshot.to_node()?;
    let offset = graphene::Point::new(-origin.x(), -origin.y());
    let transform = gsk::Transform::new().translate(&offset);
    Some(gsk::TransformNode::new(node, Some(&transform)).upcast())
}

impl Tile {
    fn offscreen(&self, viewport: &graphene::Rect, first: u32) -> graphene::Rect {
        let y = if self.position < first {
            viewport.y() - self.rect.height()
        } else {
            viewport.y() + viewport.height()
        };
        graphene::Rect::new(self.rect.x(), y, self.rect.width(), self.rect.height())
    }
}

impl MovingTile {
    fn rect_at(&self, progress: f64) -> graphene::Rect {
        self.from.interpolate(&self.tile.rect, progress)
    }

    fn draw(&self, snapshot: &gtk::Snapshot, progress: f64, viewport: &graphene::Rect) {
        let rect = self.rect_at(progress);
        if rect.intersection(viewport).is_none() {
            return;
        }
        snapshot.save();
        snapshot.translate(&graphene::Point::new(rect.x(), rect.y()));
        snapshot.scale(
            rect.width() / self.tile.image_size.width(),
            rect.height() / self.tile.image_size.height(),
        );
        snapshot.append_node(&self.tile.image);
        snapshot.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_core::settings::PreferencesUpdate;
    use std::rc::Rc;

    use crate::test_support::harness::{wait_for_frames, wait_until, Fixture, TestWindow};

    #[gtk::test]
    fn resizing_the_window_moves_files_between_grid_columns() {
        let fixture = Fixture::with_files(80);
        let test = TestWindow::open(&fixture.uri());
        let pane = test.window.folder_pane();
        let settings = pane.settings();
        let previous = settings.is_gtk_enable_animations();
        settings.set_gtk_enable_animations(true);
        test.window.show_view(FolderView::from_key("large").unwrap());
        wait_for_frames(pane, 3);

        let observed = Rc::new(Cell::new(false));
        let record = Rc::clone(&observed);
        let tick = pane.add_tick_callback(move |pane, _| {
            record.set(record.get() || pane.imp().zoom.borrow().is_some());
            glib::ControlFlow::Continue
        });
        test.window.set_default_size(780, 680);
        wait_until("the window to shrink", || (test.window.width() - 780).abs() <= 8);
        wait_for_frames(pane, 3);
        tick.remove();
        assert!(!observed.get(), "window resize animation starts off");

        test.window.set_default_size(1320, 810);
        wait_until("the window to widen", || (test.window.width() - 1320).abs() <= 8);
        wait_for_frames(pane, 3);
        test.context.update_preferences(
            PreferencesUpdate {
                animate_view_zoom: Some(false),
                animate_window_resize: Some(true),
                ..PreferencesUpdate::default()
            },
            |outcome| outcome.expect("the independent animation choices are saved"),
        );
        wait_until("resize animation to be enabled", || {
            test.context.resize_animation_ms().is_some()
        });
        assert!(test.context.view_animation_ms().is_none());
        wait_for_frames(pane, 3);

        let observed = Rc::new(Cell::new(false));
        let record = Rc::clone(&observed);
        let tick = pane.add_tick_callback(move |pane, _| {
            if let Some(zoom) = pane.imp().zoom.borrow().as_ref() {
                if let Some(reflow) = zoom.reflow.get() {
                    record.set(
                        record.get()
                            || zoom.progress.get() > 0.0
                                && reflow.tiles.iter().any(|moving| {
                                    zoom.before.iter().any(|old| {
                                        old.item == moving.tile.item
                                            && old.rect == moving.from
                                            && old.image.as_ptr() == moving.tile.image.as_ptr()
                                    }) && (moving.from.x() - moving.tile.rect.x()).abs() > 1.0
                                        && (moving.from.y() - moving.tile.rect.y()).abs() > 1.0
                                }),
                    );
                }
            }
            glib::ControlFlow::Continue
        });
        test.window.set_default_size(780, 680);
        wait_until("the resized grid to move its files", || observed.get());
        tick.remove();
        settings.set_gtk_enable_animations(previous);
    }

    #[gtk::test]
    fn zoom_moves_the_same_file_to_its_new_grid_slot_and_retargets() {
        let fixture = Fixture::with_files(80);
        let test = TestWindow::open(&fixture.uri());
        let pane = test.window.folder_pane();
        let settings = pane.settings();
        let previous = settings.is_gtk_enable_animations();
        settings.set_gtk_enable_animations(true);
        let small = FolderView::from_key("small").unwrap();
        let large = FolderView::from_key("large").unwrap();
        test.window.show_view(small);
        wait_for_frames(pane, 3);

        let animation = pane.capture_zoom(500).expect("visible small tiles");
        test.window.show_view(large);
        wait_for_frames(pane, 3);
        let reflow = animation
            .prepare_reflow(pane, &pane.first_child().expect("folder overlay"))
            .expect("the two real grid layouts");
        animation.progress.set(0.5);
        let (item, current, image) = {
            let moving = reflow
                .tiles
                .iter()
                .find(|moving| {
                    animation
                        .before
                        .iter()
                        .any(|old| old.item == moving.tile.item && old.rect == moving.from)
                        && moving.tile.rect.intersection(&reflow.viewport).is_some()
                        && (moving.from.x() - moving.tile.rect.x()).abs() > 1.0
                        && (moving.from.height() - moving.tile.rect.height()).abs() > 1.0
                })
                .unwrap_or_else(|| {
                    panic!(
                        "the new grid redistributes and grows a visible file: {:?}",
                        reflow
                            .tiles
                            .iter()
                            .take(8)
                            .map(|tile| (tile.from, tile.tile.rect))
                            .collect::<Vec<_>>()
                    )
                });
            let midpoint = moving.rect_at(0.5);
            assert_ne!(midpoint, moving.from);
            assert_ne!(midpoint, moving.tile.rect);
            let original = animation
                .before
                .iter()
                .find(|tile| tile.item == moving.tile.item)
                .unwrap();
            assert_eq!(moving.tile.image.as_ptr(), original.image.as_ptr());
            assert_eq!(moving.tile.image_size, original.image_size);
            (
                moving.tile.item.clone(),
                moving.rect_at(animation.progress.get()),
                original.image.clone(),
            )
        };
        animation.reflow.set(reflow).expect("first target");
        pane.animate_zoom(animation);
        let next = pane.capture_zoom(500).expect("the animation is visible");
        let matched = next.before.iter().find(|tile| tile.item == item).unwrap();
        assert_eq!(matched.rect, current);
        assert_eq!(matched.image.as_ptr(), image.as_ptr());
        pane.stop_zoom_animation();
        settings.set_gtk_enable_animations(false);
        assert!(pane.capture_zoom(500).is_none());
        settings.set_gtk_enable_animations(previous);
    }

    #[gtk::test]
    fn zoom_keeps_sidebar_rows_and_skips_places_changed() {
        let fixture = Fixture::with_files(40);
        let test = TestWindow::open(&fixture.uri());
        let pane = test.window.folder_pane();
        let settings = pane.settings();
        let previous = settings.is_gtk_enable_animations();
        settings.set_gtk_enable_animations(true);
        test.window.show_view(FolderView::from_key("small").unwrap());
        wait_for_frames(pane, 3);

        let sidebar_list = test.window.sidebar().list();
        let first_row_before = sidebar_list.row_at_index(0).expect("sidebar has rows");
        let places_changed = Rc::new(Cell::new(0u32));
        let count = Rc::clone(&places_changed);
        let handler = test.context.connect_places_changed(move || {
            count.set(count.get() + 1);
        });

        let small = FolderView::from_key("small").unwrap();
        let expected = small.zoomed(1);
        test.window.zoom_view(1);
        wait_until("zoomed view preference to be saved", || {
            let style = test.context.settings_data().preferences.view_for(&fixture.uri());
            FolderView::from_style(&style.mode, style.icon_size) == expected
        });
        wait_for_frames(pane, 3);
        test.context.disconnect(handler);

        assert_eq!(
            places_changed.get(),
            0,
            "zooming should not emit places-changed and redraw the sidebar"
        );
        let first_row_after = sidebar_list.row_at_index(0).expect("sidebar still has rows");
        assert_eq!(
            first_row_before, first_row_after,
            "sidebar row widgets must stay intact during zoom"
        );
        settings.set_gtk_enable_animations(previous);
    }
}
