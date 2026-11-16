// SPDX-License-Identifier: AGPL-3.0-only
//! GTK4's X11 drag destination search scans the ordinary window tree,
//! which excludes GNOME Shell's Composite overlay. Send hover messages to
//! that overlay when its input shape is under the pointer. GTK keeps the
//! real selection and handles drops, including after the dock raises an app.

use std::cell::RefCell;
use std::time::Duration;

use gtk::glib::prelude::*;
use gtk::{gdk, glib};
use x11rb::connection::Connection as _;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::shape::{ConnectionExt as _, SK};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, Rectangle, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;

thread_local! {
    static TIMER: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
}

pub(super) fn start(display: &gdk::Display) {
    stop();
    if display.type_().name() != "GdkX11Display" {
        return;
    }
    let Some(mut hover) = Hover::connect() else { return };
    let timer = glib::timeout_add_local(Duration::from_millis(80), move || {
        hover.update();
        glib::ControlFlow::Continue
    });
    TIMER.with_borrow_mut(|active| *active = Some(timer));
}

pub(super) fn stop() {
    if let Some(timer) = TIMER.with_borrow_mut(Option::take) {
        timer.remove();
    }
}

struct Hover {
    connection: RustConnection,
    root: Window,
    overlay: Window,
    proxy: Window,
    source: Window,
    enter: Atom,
    position: Atom,
    leave: Atom,
    uri_list: Atom,
    copy: Atom,
    last_position: Option<(i16, i16)>,
}

impl Hover {
    fn connect() -> Option<Self> {
        let (connection, screen) = RustConnection::connect(None).ok()?;
        let root = connection.setup().roots.get(screen)?.root;
        let atom = |name: &[u8]| Some(connection.intern_atom(false, name).ok()?.reply().ok()?.atom);
        let property_window = |window, property| {
            connection
                .get_property(false, window, property, AtomEnum::WINDOW, 0, 1)
                .ok()?
                .reply()
                .ok()?
                .value32()?
                .next()
        };
        let manager = property_window(root, atom(b"_NET_SUPPORTING_WM_CHECK")?)?;
        let name = connection
            .get_property(false, manager, atom(b"_NET_WM_NAME")?, AtomEnum::ANY, 0, 32)
            .ok()?
            .reply()
            .ok()?
            .value;
        if name != b"GNOME Shell" {
            return None;
        }

        let overlay = connection
            .composite_get_overlay_window(root)
            .ok()?
            .reply()
            .ok()?
            .overlay_win;
        let proxy_atom = atom(b"XdndProxy")?;
        let proxy = property_window(overlay, proxy_atom)?;
        if property_window(proxy, proxy_atom)? != proxy {
            return None;
        }

        // Mutter replies with XdndStatus. Use a private window for those
        // replies so they cannot change the real drag's GDK destination.
        let source = connection.generate_id().ok()?;
        connection
            .create_window(
                0,
                source,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .ok()?
            .check()
            .ok()?;
        let (enter, position, leave, uri_list, copy) = (
            atom(b"XdndEnter")?,
            atom(b"XdndPosition")?,
            atom(b"XdndLeave")?,
            atom(b"text/uri-list")?,
            atom(b"XdndActionCopy")?,
        );
        Some(Self {
            connection,
            root,
            overlay,
            proxy,
            source,
            enter,
            position,
            leave,
            uri_list,
            copy,
            last_position: None,
        })
    }

    fn update(&mut self) {
        while matches!(self.connection.poll_for_event(), Ok(Some(_))) {}
        let inside = self.pointer_inside();
        if inside == self.last_position {
            return;
        }
        let Some((x, y)) = inside else {
            self.leave();
            return;
        };
        if self.last_position.replace((x, y)).is_none() {
            self.send(self.enter, [self.source, 5 << 24, self.uri_list, 0, 0]);
        }
        let coordinates = (u32::from(x.cast_unsigned()) << 16) | u32::from(y.cast_unsigned());
        self.send(
            self.position,
            [self.source, 0, coordinates, x11rb::CURRENT_TIME, self.copy],
        );
    }

    fn pointer_inside(&self) -> Option<(i16, i16)> {
        let pointer = self.connection.query_pointer(self.root).ok()?.reply().ok()?;
        if !pointer.same_screen {
            return None;
        }
        let shape = self
            .connection
            .shape_get_rectangles(self.overlay, SK::INPUT)
            .ok()?
            .reply()
            .ok()?;
        let (x, y) = (pointer.root_x, pointer.root_y);
        shape
            .rectangles
            .iter()
            .any(|rectangle| contains(rectangle, x, y))
            .then_some((x, y))
    }

    fn leave(&mut self) {
        if self.last_position.take().is_some() {
            self.send(self.leave, [self.source, 0, 0, 0, 0]);
        }
    }

    fn send(&self, kind: Atom, data: [u32; 5]) {
        let event = ClientMessageEvent::new(32, self.overlay, kind, data);
        let _ = self
            .connection
            .send_event(false, self.proxy, EventMask::NO_EVENT, event);
        let _ = self.connection.flush();
    }
}

impl Drop for Hover {
    fn drop(&mut self) {
        self.leave();
        let _ = self.connection.composite_release_overlay_window(self.root);
        let _ = self.connection.flush();
    }
}

fn contains(rectangle: &Rectangle, x: i16, y: i16) -> bool {
    let dx = i32::from(x) - i32::from(rectangle.x);
    let dy = i32::from(y) - i32::from(rectangle.y);
    (0..i32::from(rectangle.width)).contains(&dx) && (0..i32::from(rectangle.height)).contains(&dy)
}

#[cfg(test)]
mod tests {
    use x11rb::protocol::shape::SO;
    use x11rb::protocol::xproto::ClipOrdering;
    use x11rb::protocol::Event;
    use x11rb::wrapper::ConnectionExt as _;

    use super::*;

    #[gtk::test]
    fn hover_reaches_the_composite_proxy_without_sending_a_drop() {
        let (shell, screen) = RustConnection::connect(None).expect("the private X server");
        let root = shell.setup().roots[screen].root;
        let atom = |name: &[u8]| shell.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let manager_property = atom(b"_NET_SUPPORTING_WM_CHECK");
        let proxy_atom = atom(b"XdndProxy");
        let overlay = shell
            .composite_get_overlay_window(root)
            .unwrap()
            .reply()
            .unwrap()
            .overlay_win;
        let proxy = shell.generate_id().unwrap();
        shell
            .create_window(
                0,
                proxy,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        shell
            .change_property32(
                x11rb::protocol::xproto::PropMode::REPLACE,
                root,
                manager_property,
                AtomEnum::WINDOW,
                &[proxy],
            )
            .unwrap();
        for window in [proxy, overlay] {
            shell
                .change_property32(
                    x11rb::protocol::xproto::PropMode::REPLACE,
                    window,
                    proxy_atom,
                    AtomEnum::WINDOW,
                    &[proxy],
                )
                .unwrap();
        }
        shell
            .change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                proxy,
                atom(b"_NET_WM_NAME"),
                atom(b"UTF8_STRING"),
                b"GNOME Shell",
            )
            .unwrap();
        shell
            .shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                overlay,
                0,
                0,
                &[Rectangle {
                    x: 100,
                    y: 100,
                    width: 50,
                    height: 50,
                }],
            )
            .unwrap()
            .check()
            .unwrap();

        let mut hover = Hover::connect().expect("the GNOME Xdnd proxy");
        let update_at = |hover: &mut Hover, x, y| {
            shell
                .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x, y)
                .unwrap()
                .check()
                .unwrap();
            hover.update();
        };
        update_at(&mut hover, 10, 10);
        update_at(&mut hover, 110, 120);
        update_at(&mut hover, 110, 120);
        update_at(&mut hover, 10, 10);
        drop(hover);

        // Closing the sender flushes the queued messages to this connection.
        shell.get_input_focus().unwrap().reply().unwrap();
        let mut messages = Vec::new();
        while let Some(event) = shell.poll_for_event().unwrap() {
            if let Event::ClientMessage(message) = event {
                messages.push(message);
            }
        }
        assert_eq!(
            messages.iter().map(|message| message.type_).collect::<Vec<_>>(),
            [atom(b"XdndEnter"), atom(b"XdndPosition"), atom(b"XdndLeave")]
        );
        assert_eq!(messages[1].data.as_data32()[2], (110 << 16) | 120);

        shell
            .delete_property(root, manager_property)
            .unwrap()
            .check()
            .unwrap();
        shell
            .delete_property(overlay, proxy_atom)
            .unwrap()
            .check()
            .unwrap();
    }
}
