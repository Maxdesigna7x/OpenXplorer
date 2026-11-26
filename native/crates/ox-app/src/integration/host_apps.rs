// SPDX-License-Identifier: AGPL-3.0-only
//! Host application listing, launching and default association for Open
//! with when `OpenXplorer` runs inside a Flatpak sandbox.
//!
//! Inside Flatpak, GIO's `GAppInfo` only sees the sandbox's own
//! `XDG_DATA_DIRS` and cannot start host binaries or update the host's
//! `mimeapps.list`. Because the Flatpak manifest grants
//! `--talk-name=org.freedesktop.Flatpak`, this module queries and starts
//! the host's GIO application database through `flatpak-spawn --host`,
//! giving Open with the same installed, recommended, related and default
//! applications as the native Linux desktop.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ox_core::integration::ApplicationInfo;
use serde::Deserialize;

use super::applications::{LaunchTarget, OpenWithError};

/// The Flatpak tool that runs a command on the host.
const FLATPAK_SPAWN: &str = "flatpak-spawn";

/// How long context-menu lookups for a content type stay cached in memory.
const MENU_CACHE_TTL: Duration = Duration::from_secs(5);

/// Host script that queries the host's GIO application database and prints
/// JSON for [`HostQueryResult`].
const HOST_LIST_SCRIPT: &str = r#"
import json, sys, gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio

ct = sys.argv[1]
scope = sys.argv[2]

def info(app):
    icon = app.get_icon()
    return {
        "id": app.get_id() or "",
        "name": app.get_display_name() or "",
        "should_show": bool(app.should_show()),
        "supports_files": bool(app.supports_files()),
        "supports_uris": bool(app.supports_uris()),
        "icon": icon.to_string() if icon else None,
    }

default_app = Gio.AppInfo.get_default_for_type(ct, False)
default_id = default_app.get_id() if default_app else None

if scope == "menu":
    recommended = Gio.AppInfo.get_recommended_for_type(ct)
    recommended_ids = [app.get_id() for app in recommended if app.get_id()]
    candidates = list(recommended)
elif scope == "all":
    all_for_type = Gio.AppInfo.get_all_for_type(ct)
    recommended_ids = [app.get_id() for app in all_for_type if app.get_id()]
    candidates = list(Gio.AppInfo.get_all())
else:
    all_for_type = Gio.AppInfo.get_all_for_type(ct)
    recommended_ids = [app.get_id() for app in all_for_type if app.get_id()]
    candidates = list(all_for_type)
    if default_app is not None and default_id not in recommended_ids:
        candidates.insert(0, default_app)

print(json.dumps({
    "default_id": default_id,
    "recommended_ids": recommended_ids,
    "apps": [info(app) for app in candidates],
}, ensure_ascii=False))
"#;

/// Host script that launches a desktop ID on a path or URI via GIO.
/// Exit code 2 means the desktop ID is not installed; 3 means launch failed.
const HOST_LAUNCH_SCRIPT: &str = r#"
import sys, gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio

app_id, kind, target = sys.argv[1], sys.argv[2], sys.argv[3]
app = Gio.DesktopAppInfo.new(app_id)
if app is None:
    sys.exit(2)
item = Gio.File.new_for_path(target) if kind == "path" else Gio.File.new_for_uri(target)
if not app.launch([item], None):
    sys.exit(3)
"#;

/// Host script that makes a desktop ID the default for a content type.
const HOST_SET_DEFAULT_SCRIPT: &str = r#"
import sys, gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio

app_id, ct = sys.argv[1], sys.argv[2]
app = Gio.DesktopAppInfo.new(app_id)
if app is None or not app.set_as_default_for_type(ct):
    sys.exit(1)
"#;

/// Host script that checks whether a desktop ID is installed on the host.
const HOST_EXISTS_SCRIPT: &str = r#"
import sys, gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio

sys.exit(0 if Gio.DesktopAppInfo.new(sys.argv[1]) is not None else 1)
"#;

/// Which slice of the host's application catalog to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostQueryScope {
    /// Default plus every handler registered for the type (recommended and related).
    Recommended,
    /// Every installed application on the host.
    AllInstalled,
    /// Only recommended handlers for the context menu.
    Menu,
}

impl HostQueryScope {
    const fn as_arg(self) -> &'static str {
        match self {
            Self::Recommended => "recommended",
            Self::AllInstalled => "all",
            Self::Menu => "menu",
        }
    }
}

/// One application reported by the host's GIO database.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HostApplication {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) should_show: bool,
    pub(crate) supports_files: bool,
    pub(crate) supports_uris: bool,
    pub(crate) icon: Option<String>,
}

impl ApplicationInfo for HostApplication {
    fn id(&self) -> Option<String> {
        (!self.id.is_empty()).then(|| self.id.clone())
    }

    fn display_name(&self) -> String {
        self.name.clone()
    }

    fn should_show(&self) -> bool {
        self.should_show
    }

    fn supports_files(&self) -> bool {
        self.supports_files
    }

    fn supports_uris(&self) -> bool {
        self.supports_uris
    }

    fn icon(&self) -> Option<String> {
        self.icon.clone()
    }
}

/// The parsed result of querying the host's application database for one
/// content type.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HostQueryResult {
    pub(crate) default_id: Option<String>,
    pub(crate) recommended_ids: Vec<String>,
    pub(crate) apps: Vec<HostApplication>,
}

/// Parses the JSON produced by [`HOST_LIST_SCRIPT`].
pub(crate) fn parse_host_query(bytes: &[u8]) -> Option<HostQueryResult> {
    serde_json::from_slice(bytes).ok()
}

/// Short-lived cache of host context-menu lookups by content type.
fn menu_cache() -> &'static Mutex<HashMap<String, (Instant, HostQueryResult)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, HostQueryResult)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drops cached context-menu entries after a default association changes.
fn clear_menu_cache() {
    let mut cache = menu_cache().lock().unwrap_or_else(PoisonError::into_inner);
    cache.clear();
}

/// Returns a display name for `app_id` if a recent host menu lookup saw it.
pub(crate) fn cached_application_name(app_id: &str) -> Option<String> {
    let cache = menu_cache().lock().unwrap_or_else(PoisonError::into_inner);
    cache.values().find_map(|(_, result)| {
        result
            .apps
            .iter()
            .find(|app| app.id == app_id && !app.name.is_empty())
            .map(|app| app.name.clone())
    })
}

/// Queries the host's GIO application database for `content_type`.
/// Returns `None` if `flatpak-spawn --host` or host Python/GI is unavailable.
pub(crate) fn list_host_applications(content_type: &str, scope: HostQueryScope) -> Option<HostQueryResult> {
    if scope == HostQueryScope::Menu {
        let cache = menu_cache().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((stored_at, result)) = cache.get(content_type) {
            if stored_at.elapsed() < MENU_CACHE_TTL {
                return Some(result.clone());
            }
        }
    }

    let output = Command::new(FLATPAK_SPAWN)
        .args([
            "--host",
            "/usr/bin/python3",
            "-c",
            HOST_LIST_SCRIPT,
            content_type,
            scope.as_arg(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let result = parse_host_query(&output.stdout)?;
    if scope == HostQueryScope::Menu {
        let mut cache = menu_cache().lock().unwrap_or_else(PoisonError::into_inner);
        cache.insert(content_type.to_owned(), (Instant::now(), result.clone()));
    }
    Some(result)
}

/// Whether `app_id` is installed as a `.desktop` launcher on the host.
pub(crate) fn host_application_exists(app_id: &str) -> bool {
    Command::new(FLATPAK_SPAWN)
        .args(["--host", "/usr/bin/python3", "-c", HOST_EXISTS_SCRIPT, app_id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Launches `app_id` on the host with `target`.
///
/// # Errors
///
/// [`OpenWithError::NotInstalled`] when the host has no desktop entry for
/// `app_id`, or [`OpenWithError::NotStarted`] when launching fails.
pub(crate) fn launch_host_application(app_id: &str, target: &LaunchTarget) -> Result<(), OpenWithError> {
    let (kind, value) = match target {
        LaunchTarget::Path(path) => ("path", path.as_os_str().to_os_string()),
        LaunchTarget::Uri(uri) => ("uri", std::ffi::OsString::from(uri)),
    };
    let status = Command::new(FLATPAK_SPAWN)
        .args([
            "--host",
            "/usr/bin/python3",
            "-c",
            HOST_LAUNCH_SCRIPT,
            app_id,
            kind,
        ])
        .arg(&value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => return Ok(()),
        Ok(status) if status.code() == Some(2) => return Err(OpenWithError::NotInstalled),
        _ => {}
    }
    let fallback = Command::new(FLATPAK_SPAWN)
        .args(["--host", "gtk-launch", app_id])
        .arg(&value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match fallback {
        Ok(status) if status.success() => Ok(()),
        _ => Err(OpenWithError::NotStarted),
    }
}

/// Sets `app_id` as the host's default handler for `content_type`.
pub(crate) fn set_host_default_for_type(app_id: &str, content_type: &str) -> bool {
    let gio_ok = Command::new(FLATPAK_SPAWN)
        .args([
            "--host",
            "/usr/bin/python3",
            "-c",
            HOST_SET_DEFAULT_SCRIPT,
            app_id,
            content_type,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let ok = gio_ok
        || Command::new(FLATPAK_SPAWN)
            .args(["--host", "xdg-mime", "default", app_id, content_type])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    if ok {
        clear_menu_cache();
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_query_json_maps_to_application_info() {
        let raw = br#"{
            "default_id": "wps-office-prometheus.desktop",
            "recommended_ids": ["libreoffice-writer.desktop", "org.gnome.Nautilus.desktop"],
            "apps": [
                {
                    "id": "wps-office-prometheus.desktop",
                    "name": "WPS 2019",
                    "should_show": true,
                    "supports_files": true,
                    "supports_uris": false,
                    "icon": "wps-office2019-kprometheus"
                },
                {
                    "id": "libreoffice-writer.desktop",
                    "name": "LibreOffice Writer",
                    "should_show": true,
                    "supports_files": true,
                    "supports_uris": true,
                    "icon": "libreoffice-writer"
                },
                {
                    "id": "",
                    "name": "Unnamed",
                    "should_show": false,
                    "supports_files": false,
                    "supports_uris": false,
                    "icon": null
                }
            ]
        }"#;

        let parsed = parse_host_query(raw).expect("valid host query JSON");
        assert_eq!(
            parsed.default_id.as_deref(),
            Some("wps-office-prometheus.desktop")
        );
        assert_eq!(parsed.recommended_ids.len(), 2);
        assert_eq!(parsed.apps.len(), 3);
        assert_eq!(
            parsed.apps[0].id().as_deref(),
            Some("wps-office-prometheus.desktop")
        );
        assert_eq!(parsed.apps[0].display_name(), "WPS 2019");
        assert!(parsed.apps[0].should_show());
        assert!(parsed.apps[0].supports_files());
        assert!(!parsed.apps[0].supports_uris());
        assert_eq!(
            parsed.apps[0].icon().as_deref(),
            Some("wps-office2019-kprometheus")
        );
        assert_eq!(parsed.apps[2].id(), None);
        assert_eq!(parsed.apps[2].icon(), None);
    }
}
