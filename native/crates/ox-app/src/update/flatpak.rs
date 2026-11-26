// SPDX-License-Identifier: AGPL-3.0-only
//! Flatpak checks its configured origin and verifies and deploys the update.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use super::{UpdateState, Updates};

const PORTAL: &str = "org.freedesktop.portal.Flatpak";
const PORTAL_PATH: &str = "/org/freedesktop/portal/Flatpak";
const MONITOR: &str = "org.freedesktop.portal.Flatpak.UpdateMonitor";

pub(crate) fn is_flatpak() -> bool {
    ox_core::integration::Sandbox::detect().is_flatpak()
}

pub(super) async fn monitor(updates: &Updates) -> Result<gio::DBusProxy, glib::Error> {
    let bus = gio::bus_get_future(gio::BusType::Session).await?;
    let sender = bus.unique_name().expect("a session bus client has a unique name");
    let sender = sender.trim_start_matches(':').replace('.', "_");
    let path = format!("{PORTAL_PATH}/update_monitor/{sender}/ox_updates");
    // Listen before creating the monitor so that an immediate signal is kept.
    let proxy = gio::DBusProxy::new_future(
        &bus,
        gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES | gio::DBusProxyFlags::DO_NOT_AUTO_START_AT_CONSTRUCTION,
        None,
        Some(PORTAL),
        &path,
        MONITOR,
    )
    .await?;
    proxy.connect_local(
        "g-signal",
        false,
        glib::clone!(
            #[weak]
            updates,
            #[upgrade_or]
            None,
            move |args| {
                let name = args[2].get::<String>().ok()?;
                let payload = args[3].get::<glib::Variant>().ok()?;
                let details = glib::VariantDict::new(Some(&payload.try_child_value(0)?));
                match name.as_str() {
                    "UpdateAvailable" => show_available(&updates, &details),
                    "Progress" => show_progress(&updates, &details),
                    _ => {}
                }
                None
            }
        ),
    );
    let options = glib::VariantDict::new(None);
    options.insert("handle_token", "ox_updates");
    bus.call_future(
        Some(PORTAL),
        PORTAL_PATH,
        PORTAL,
        "CreateUpdateMonitor",
        Some(&glib::Variant::tuple_from_iter([options.end()])),
        None,
        gio::DBusCallFlags::NONE,
        10_000,
    )
    .await?;
    Ok(proxy)
}

fn show_available(updates: &Updates, details: &glib::VariantDict) {
    if updates.state() == UpdateState::FlatpakInstalling {
        return;
    }
    let text = |key| {
        details
            .lookup::<String>(key)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
    };
    let (Some(running), Some(local), Some(remote)) = (
        text("running-commit"),
        text("local-commit"),
        text("remote-commit"),
    ) else {
        return;
    };
    let state = if running != local {
        UpdateState::FlatpakReady
    } else if remote != local {
        UpdateState::FlatpakAvailable
    } else {
        UpdateState::FlatpakWatching
    };
    let available = state == UpdateState::FlatpakAvailable;
    updates.set_state(state);
    if available {
        updates.maybe_install_flatpak();
    }
}

fn show_progress(updates: &Updates, details: &glib::VariantDict) {
    if updates.state() != UpdateState::FlatpakInstalling {
        return;
    }
    match details.lookup::<u32>("status").ok().flatten() {
        Some(1) => updates.set_state(UpdateState::FlatpakWatching),
        Some(2) => updates.set_state(UpdateState::FlatpakReady),
        Some(3) => updates.set_state(UpdateState::FlatpakFailed),
        _ => {}
    }
}

pub(super) async fn install(proxy: &gio::DBusProxy) -> Result<glib::Variant, glib::Error> {
    let options = glib::VariantDict::new(None);
    let parameters = glib::Variant::tuple_from_iter(["".to_variant(), options.end()]);
    proxy
        .call_future("Update", Some(&parameters), gio::DBusCallFlags::NONE, 10_000)
        .await
}
