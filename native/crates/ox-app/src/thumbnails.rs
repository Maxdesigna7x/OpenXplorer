// SPDX-License-Identifier: AGPL-3.0-only
//! Previews of files in the folder views (VIEW-057), from the desktop's
//! shared thumbnail cache, as Dolphin and Files show them.
//!
//! A view asks for a preview when it binds a row or tile and drops the
//! request when GTK unbinds it, so rows scrolled past cost nothing more.
//! A request reads the cache through GIO (`thumbnail::path`,
//! `thumbnail::is-valid`, `thumbnail::failed`) at low priority. A missing
//! thumbnail is made by the session's thumbnailer service
//! ([`service`]) when it has one and supports the type, else, for a
//! picture gdk-pixbuf may read ([`pixbuf_may_read`]), by the app itself
//! ([`writer`]), which costs less than a program, else by a thumbnailer
//! the desktop installs for the type ([`thumbnailers`], a program the app
//! starts in a sandbox). The desktop's thumbnailer is asked for a picture
//! only when the app cannot read it ([`writer::Failure::Unreadable`]): one
//! the cache refuses would be refused for the program too, and a dropped
//! request needs no picture at all. Only a few are decoded or made at a
//! time ([`slots`]); a type that cannot be previewed, or a failed attempt,
//! keeps its icon. Every picture the app decodes itself, and every file it
//! reads back from the cache, goes through [`picture_guard`], so a huge,
//! damaged or animated one keeps its icon instead of holding the app up.
//! [`PreviewPolicy`] says which items of a folder get previews, from the
//! Settings the user chose (VIEW-058).

mod network_fs;
mod service;
mod slots;
mod thumbnailers;
mod writer;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gtk::gdk_pixbuf::{Pixbuf, PixbufFormat};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use ox_core::entry::{cached_thumbnail, CachedThumbnail, Entry, ThumbnailFlavor, THUMBNAIL_ATTRIBUTES};
use ox_core::settings::{ViewOptions, PREVIEW_SIZE_LIMIT};

pub(crate) use network_fs::is_on_network_filesystem;
pub(crate) use slots::Slots;
pub(crate) use thumbnailers::pdf_pages;

use crate::picture_guard::{self, Limits};
use thumbnailers::Catalogue;
use writer::Source;

/// Decoding can use spare cores, with room left for the UI and thumbnail
/// generators. The upper bound also limits simultaneous decoded images.
fn decoding_slots() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cpus.saturating_sub(2).clamp(1, 6)
}
/// How many thumbnails are made at a time, leaving cores free for the UI.
fn making_slots() -> usize {
    let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    cpus.saturating_sub(2).clamp(2, 4)
}
/// How many decoded previews are kept (one highest-resolution texture per item version).
const MAX_KEPT: usize = 1024;
/// How many parent directories are remembered for local-vs-network filesystem checks.
const MAX_NETWORK_DIRS: usize = 256;

/// Which items of the folder shown get previews.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate option of the Settings page"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PreviewPolicy {
    /// Previews are shown at all; off, every item keeps its icon.
    pub shown: bool,
    /// Items on a server or a device get previews too, wherever they are
    /// listed; off, they keep their icons even in a folder on this
    /// computer (search results, Recent, Quick access).
    pub remote: bool,
    /// Files larger than this keep their icon.
    pub size_limit: Option<u64>,
    /// Pictures get previews.
    pub pictures: bool,
    /// Videos get previews.
    pub videos: bool,
    /// Every other type gets previews.
    pub documents: bool,
}

impl PreviewPolicy {
    /// The policy for a folder that is on this computer, or not
    /// (`is_remote`), under the user's `options`.
    pub(crate) fn for_folder(options: &ViewOptions, is_remote: bool) -> Self {
        Self {
            shown: options.show_previews && (options.remote_previews || !is_remote),
            remote: options.remote_previews,
            size_limit: options.skip_large_previews.then_some(PREVIEW_SIZE_LIMIT),
            pictures: options.preview_pictures,
            videos: options.preview_videos,
            documents: options.preview_documents,
        }
    }

    /// Whether items of `content_type` get previews.
    fn allows_type(self, content_type: &str) -> bool {
        match content_type.split_once('/').map(|(kind, _)| kind) {
            Some("image") => self.pictures,
            Some("video") => self.videos,
            _ => self.documents,
        }
    }

    /// The preview request for `entry` at `pixels` device pixels, when
    /// [`Self::allows`] it; with network previews off it asks for a file
    /// on this computer's own disks only. Video thumbnailers seek to a single
    /// keyframe rather than decoding the whole file, so videos are not blocked
    /// by the 50 MB image size limit.
    pub(crate) fn request(self, entry: &Entry, pixels: i32) -> Option<PreviewRequest> {
        let request = PreviewRequest::for_entry(entry, pixels)?;
        let allowed = if request.content_type.starts_with("video/") {
            Self {
                size_limit: None,
                ..self
            }
            .allows(entry)
        } else {
            self.allows(entry)
        };
        allowed.then_some(PreviewRequest {
            local_only: !self.remote,
            existing_only: false,
            ..request
        })
    }

    /// Whether `entry` may show a preview: a file of a known type within
    /// the size limit.
    pub(crate) fn allows(self, entry: &Entry) -> bool {
        let within_limit = match (self.size_limit, entry.size) {
            (Some(limit), Some(size)) => size <= limit,
            _ => true,
        };
        let type_allowed = entry
            .content_type
            .as_deref()
            .is_some_and(|content_type| self.allows_type(content_type));
        let reachable = self.remote || !is_remote_item(entry);
        self.shown && !entry.is_dir && !entry.is_virtual && type_allowed && within_limit && reachable
    }
}

/// Whether `entry`, or the item it stands for, is on a server or a
/// device rather than this computer, by its URI: a search result or a
/// recent item may be listed in a folder that is not.
fn is_remote_item(entry: &Entry) -> bool {
    use ox_core::location::{location_kind, LocationKind};

    std::iter::once(entry.uri.as_str())
        .chain(entry.target_uri.as_deref())
        .chain(entry.meta.recent_target.as_deref())
        .any(|uri| {
            matches!(
                location_kind(uri),
                LocationKind::Smb | LocationKind::Remote | LocationKind::Device
            )
        })
}

/// The file `entry` stands for, when it is another: a Recently used
/// files row's file (kept in its metadata, as `GVfs` names it only in the
/// row's target), or a share's or shortcut's target.
fn stands_for(entry: &Entry) -> Option<&str> {
    entry
        .meta
        .recent_target
        .as_deref()
        .or(entry.target_uri.as_deref())
}

/// A decoded preview's item and version.
type PreviewKey = (String, Option<u64>);

/// What a preview is wanted of.
#[derive(Debug, Clone)]
pub(crate) struct PreviewRequest {
    /// The item's URI.
    pub uri: String,
    /// Its content type.
    pub content_type: String,
    /// Its modification time, which tells a current thumbnail from an old.
    pub modified: Option<u64>,
    /// The edge of the picture wanted, in device pixels.
    pub pixels: i32,
    /// Only a file on this computer's own disks gets a preview: one on a
    /// network file system (a kernel share, sshfs, `GVfs`'s mounts) keeps its
    /// icon, as network previews are off ([`network_fs`]).
    pub local_only: bool,
    /// Only reuse an existing thumbnail, without generating one.
    pub existing_only: bool,
    /// The file the item stands for, when it is another (a recent item's,
    /// a search result's), which [`Self::local_only`] looks at too.
    pub target_uri: Option<String>,
}

impl PreviewRequest {
    /// The request for `entry` at `pixels` device pixels, if the entry
    /// has a type.
    pub(crate) fn for_entry(entry: &Entry, pixels: i32) -> Option<Self> {
        Some(Self {
            uri: entry.uri.clone(),
            content_type: entry.content_type.clone()?,
            modified: entry.modified,
            pixels,
            local_only: false,
            existing_only: false,
            target_uri: stands_for(entry).map(str::to_owned),
        })
    }

    fn key(&self) -> PreviewKey {
        (self.uri.clone(), self.modified)
    }
}

thread_local! {
    static DECODING: Rc<Slots> = Rc::new(Slots::new(decoding_slots()));
    static MAKING: Rc<Slots> = Rc::new(Slots::new(making_slots()));
    /// Decoded previews by item and version, keeping the highest resolution
    /// decoded so far so zooming reuses sharp textures without flashing icons.
    static KEPT: RefCell<HashMap<PreviewKey, (i32, gdk::Texture)>> = RefCell::new(HashMap::new());
    /// Item versions no thumbnail could be made of in this run.
    static FAILED: RefCell<HashSet<(String, Option<u64>)>> = RefCell::new(HashSet::new());
    /// Parent directories already checked for network filesystems.
    static NETWORK_DIRS: RefCell<HashMap<PathBuf, bool>> = RefCell::new(HashMap::new());
    /// The types gdk-pixbuf's loaders declare they read.
    static PIXBUF_TYPES: HashSet<String> = Pixbuf::formats()
        .iter()
        .flat_map(PixbufFormat::mime_types)
        .map(|mime| mime.to_string())
        .collect();
}

/// Returns an already-decoded preview from memory for `request` synchronously,
/// along with whether its resolution already satisfies `request.pixels`.
pub(crate) fn cached_preview(request: &PreviewRequest) -> Option<(gdk::Texture, bool)> {
    let key = request.key();
    let wanted = request.pixels.min(ThumbnailFlavor::Large.pixels());
    KEPT.with_borrow(|kept| {
        kept.get(&key)
            .map(|(covered, texture)| (texture.clone(), *covered >= wanted))
    })
}

/// Whether gdk-pixbuf may be asked to decode the item of `content_type`,
/// given the `declared` MIME types its loaders say they read.
///
/// Every picture qualifies, not only the types a loader declares: what a
/// loader reads and what GIO names the file differ, gdk-pixbuf's dds
/// loader declaring `image/x-dds` alone for the `image/vnd.ms-dds` files
/// GIO reports, so matching the lists exactly would leave pictures
/// without thumbnails. Decoding settles it instead: [`writer`] reads
/// what a loader reads and answers [`writer::Failure::Unreadable`] for the
/// rest, which leaves the item to a thumbnailer of the desktop's, or to
/// its icon when there is none.
fn pixbuf_may_read(content_type: &str, declared: &HashSet<String>) -> bool {
    let is_picture = content_type
        .split_once('/')
        .is_some_and(|(kind, _)| kind == "image");
    is_picture || declared.contains(content_type)
}

/// The preview `request` asks for, or `None` when the item keeps its icon.
pub(crate) async fn preview(request: PreviewRequest) -> Option<gdk::Texture> {
    let key = request.key();
    let wanted = request.pixels.min(ThumbnailFlavor::Large.pixels());
    if let Some((covered, texture)) = KEPT.with_borrow(|kept| kept.get(&key).cloned()) {
        if covered >= wanted {
            return Some(texture);
        }
    }
    if request.local_only && is_on_network(&request.uri, request.target_uri.as_deref()).await {
        return None;
    }
    let path = thumbnail_of(&request).await?;
    let is_large = is_large_thumbnail_path(&path);
    let texture = DECODING
        .with(Rc::clone)
        .blocking(move |_| decode(&path))
        .await
        .flatten();
    let texture = texture?;
    let actual_edge = texture.width().max(texture.height());
    let covered = if is_large {
        ThumbnailFlavor::Large.pixels()
    } else {
        actual_edge.max(ThumbnailFlavor::Normal.pixels())
    };
    KEPT.with_borrow_mut(|kept| {
        if kept.len() >= MAX_KEPT && !kept.contains_key(&key) {
            kept.clear();
        }
        let should_store = kept
            .get(&key)
            .is_none_or(|(existing_covered, _)| covered >= *existing_covered);
        if should_store {
            kept.insert(key, (covered, texture.clone()));
        }
    });
    Some(texture)
}

/// Whether `path` lives in a `large` (256 px) or larger thumbnail cache folder.
fn is_large_thumbnail_path(path: &Path) -> bool {
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|folder| matches!(folder, "large" | "x-large" | "xx-large"))
}

/// Whether the item at `uri`, or the file `target` it stands for, is on a
/// network file system, asked off the main thread, which a server that
/// does not answer would hold up. A URI without a local path is left to
/// [`PreviewPolicy::allows`], which tells servers and devices by their
/// URIs. Results are cached by parent directory so a folder of hundreds of
/// local files only checks its mount once.
async fn is_on_network(uri: &str, target: Option<&str>) -> bool {
    let paths: Vec<PathBuf> = std::iter::once(uri)
        .chain(target)
        .filter_map(|uri| gio::File::for_uri(uri).path())
        .collect();
    if paths.is_empty() {
        return false;
    }
    let mut uncached = Vec::new();
    let cached_hit = NETWORK_DIRS.with_borrow(|cache| {
        for path in &paths {
            let dir = path.parent().unwrap_or(path);
            if let Some(&remote) = cache.get(dir) {
                if remote {
                    return true;
                }
            } else {
                uncached.push((dir.to_path_buf(), path.clone()));
            }
        }
        false
    });
    if cached_hit {
        return true;
    }
    if uncached.is_empty() {
        return false;
    }
    let checked = gio::spawn_blocking(move || {
        uncached
            .into_iter()
            .map(|(dir, path)| (dir, network_fs::is_on_network_filesystem(&path)))
            .collect::<Vec<_>>()
    })
    .await;
    let Ok(results) = checked else {
        return true;
    };
    let mut any_remote = false;
    NETWORK_DIRS.with_borrow_mut(|cache| {
        if cache.len() >= MAX_NETWORK_DIRS {
            cache.clear();
        }
        for (dir, remote) in results {
            any_remote |= remote;
            cache.insert(dir, remote);
        }
    });
    any_remote
}

/// The cached thumbnail of the item, made first if it is missing. When the
/// caller needs more than 128 px (`Large`) and GIO only finds a 128 px
/// `normal` thumbnail, it upgrades to `Large` (256 px) and falls back to the
/// `normal` file if generating `Large` fails.
async fn thumbnail_of(request: &PreviewRequest) -> Option<PathBuf> {
    let wants_large = request.pixels > ThumbnailFlavor::Normal.pixels();
    let file = gio::File::for_uri(&request.uri);
    let info = file
        .query_info_future(
            THUMBNAIL_ATTRIBUTES,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::LOW,
        )
        .await
        .ok();
    let fallback_normal = match info.as_ref().map(cached_thumbnail) {
        Some(CachedThumbnail::Valid(path)) => {
            if !wants_large || is_large_thumbnail_path(&path) {
                return Some(path);
            }
            Some(path)
        }
        Some(CachedThumbnail::Failed) => return None,
        Some(CachedThumbnail::Missing) | None => None,
    };
    if request.existing_only {
        return fallback_normal;
    }
    let version = (request.uri.clone(), request.modified);
    if FAILED.with_borrow(|failed| failed.contains(&version)) {
        return fallback_normal;
    }
    match make_thumbnail(request).await {
        Ok(path) => Some(path),
        Err(Missing::Failed) => {
            if fallback_normal.is_none() {
                FAILED.with_borrow_mut(|failed| failed.insert(version));
            }
            fallback_normal
        }
        // Asked again next time: the user may turn them back on, or the
        // disk be less busy.
        Err(Missing::TurnedOff | Missing::Later) => fallback_normal,
    }
}

/// Why no thumbnail was made of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Missing {
    /// None could be made, and this version of the item is not tried
    /// again in this run.
    Failed,
    /// The user turned the desktop's thumbnailers off for its type, which
    /// they may turn back on at any time.
    TurnedOff,
    /// Decoding the picture took longer than its time limit; it is tried
    /// again the next time it is shown.
    Later,
}

/// Makes the missing thumbnail of the item: by the thumbnailer service
/// when it supports the type, else by gdk-pixbuf for a picture, else by
/// the desktop's own thumbnailer for the type when it installs one.
async fn make_thumbnail(request: &PreviewRequest) -> Result<PathBuf, Missing> {
    // Always generate Large (256 px) thumbnails so zooming up to Extra Large
    // stays sharp without generating the same item twice.
    let flavor = ThumbnailFlavor::Large;
    let cache_dir = glib::user_cache_dir();
    // The cache's own pictures keep their icons: a thumbnail of a
    // thumbnail would grow the cache from itself, so nothing is read, made
    // or asked of the service and the desktop's programs for them.
    if writer::inside_cache(&cache_dir, &request.uri) {
        return Err(Missing::Failed);
    }
    // GIO looks up local files only, and one size: a thumbnail this app
    // made earlier may be there all the same.
    let earlier = (cache_dir.clone(), request.uri.clone(), request.modified);
    let found = MAKING
        .with(Rc::clone)
        .blocking(move |_| {
            let (cache_dir, uri, modified) = earlier;
            writer::find_thumbnail(&cache_dir, &uri, modified?, flavor)
        })
        .await;
    if let Some(Some(path)) = found {
        return Ok(path);
    }
    if let Some(service) = service::service().await {
        if service.supports(&request.uri, &request.content_type) {
            let _slot = MAKING.with(Rc::clone).take().await;
            let made = service.make(&request.uri, &request.content_type, flavor).await;
            let path = ox_core::entry::thumbnail_file(&cache_dir, &request.uri, flavor);
            return made.then_some(path).ok_or(Missing::Failed);
        }
    }
    // Test safety: tests write thumbnails only into a private cache.
    #[cfg(test)]
    if !cache_dir.starts_with(std::env::temp_dir()) {
        return Err(Missing::Failed);
    }
    let Some(modified) = request.modified else {
        return Err(Missing::Failed);
    };
    make_thumbnail_with(request, &cache_dir, None, flavor, modified).await
}

/// The thumbnail `request` asks for: the app decodes a picture itself
/// ([`pixbuf_may_read`]), which costs less than a program of the
/// desktop's, and a program of the desktop's makes it when the type is not
/// a picture or gdk-pixbuf cannot read it (a type without its loader). `catalogue` is the desktop's own
/// list unless a test hands its own.
async fn make_thumbnail_with(
    request: &PreviewRequest,
    cache_dir: &Path,
    catalogue: Option<Arc<Catalogue>>,
    flavor: ThumbnailFlavor,
    modified: u64,
) -> Result<PathBuf, Missing> {
    if PIXBUF_TYPES.with(|declared| pixbuf_may_read(&request.content_type, declared)) {
        match by_decoding(request, cache_dir, modified, flavor).await {
            Ok(path) => return Ok(path),
            // Only a picture the app cannot read leaves room for another
            // thumbnailer, and one larger than the app decodes itself for
            // a thumbnailer whose memory is limited, as a scan of many
            // megapixels still deserves its preview. One it read and the
            // cache refused is refused for every thumbnailer, one damaged
            // or slow would be read the same way by the program, and a
            // dropped request has nobody left to show a picture to:
            // forking a program for any of them would only cost one.
            Err(writer::Failure::Unreadable) => {}
            Err(writer::Failure::TooLarge) => {
                return by_thumbnailer(request, cache_dir, modified, flavor, catalogue, true).await;
            }
            Err(writer::Failure::TimedOut) => return Err(Missing::Later),
            Err(writer::Failure::Unwritable | writer::Failure::Refused | writer::Failure::Cancelled) => {
                return Err(Missing::Failed);
            }
        }
    }
    if request.content_type.starts_with("video/") && request.uri.starts_with("file:") {
        if ox_core::integration::Sandbox::detect() == ox_core::integration::Sandbox::Flatpak {
            if let Some(path) = host_video_thumbnail(request, cache_dir, flavor, modified).await {
                return Ok(path);
            }
        }
        if let Ok(path) = by_thumbnailer(request, cache_dir, modified, flavor, catalogue, false).await {
            return Ok(path);
        }
        if let Some(path) = gstreamer_video_thumbnail(request, cache_dir, flavor, modified).await {
            return Ok(path);
        }
        return Err(Missing::Failed);
    }
    by_thumbnailer(request, cache_dir, modified, flavor, catalogue, false).await
}

async fn host_video_thumbnail(
    request: &PreviewRequest,
    cache_dir: &Path,
    flavor: ThumbnailFlavor,
    modified: u64,
) -> Option<PathBuf> {
    let uri = request.uri.clone();
    let cache_dir = cache_dir.to_path_buf();
    MAKING
        .with(Rc::clone)
        .blocking(move |cancel| {
            if cancel.is_cancelled() {
                return None;
            }
            let temporary = tempfile::tempdir_in(&cache_dir).ok()?;
            let output = temporary.path().join("preview.png");
            let status = std::process::Command::new("flatpak-spawn")
                .args([
                    "--host",
                    "timeout",
                    "--kill-after=1s",
                    "12s",
                    "/usr/bin/totem-video-thumbnailer",
                    "-s",
                ])
                .arg(flavor.pixels().to_string())
                .arg(&uri)
                .arg(&output)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()?;
            if !status.success() || cancel.is_cancelled() {
                return None;
            }
            let pixbuf = picture_guard::decode_to_fit(
                &gio::File::for_path(output),
                flavor.pixels(),
                flavor.pixels(),
                Limits::DISPLAY,
                Some(cancel),
            )
            .ok()?
            .pixbuf;
            writer::save_thumbnail(&cache_dir, &Source { uri, modified }, flavor, &pixbuf).ok()
        })
        .await
        .flatten()
}

/// Fallback video thumbnailer using `GStreamer`'s `playbin` `convert-sample` so
/// any format playable in `OpenXplorer` (`.avi`, `.mpg`, `.mkv`, `.mp4`, etc.)
/// still gets a thumbnail when `totem-video-thumbnailer` fails or is absent.
async fn gstreamer_video_thumbnail(
    request: &PreviewRequest,
    cache_dir: &Path,
    flavor: ThumbnailFlavor,
    modified: u64,
) -> Option<PathBuf> {
    let uri = request.uri.clone();
    let cache_dir = cache_dir.to_path_buf();
    MAKING
        .with(Rc::clone)
        .blocking(move |cancel| {
            if cancel.is_cancelled() {
                return None;
            }
            let pixbuf = extract_video_frame(&uri, flavor.pixels(), cancel)?;
            if cancel.is_cancelled() {
                return None;
            }
            writer::save_thumbnail(&cache_dir, &Source { uri, modified }, flavor, &pixbuf).ok()
        })
        .await
        .flatten()
}

fn extract_video_frame(uri: &str, edge: i32, cancel: &gio::Cancellable) -> Option<Pixbuf> {
    use gstreamer::prelude::*;

    struct StopOnDrop(gstreamer::Element);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            let _ = self.0.set_state(gstreamer::State::Null);
        }
    }

    gstreamer::init().ok()?;
    let playbin = gstreamer::ElementFactory::make("playbin").build().ok()?;
    let audio_sink = gstreamer::ElementFactory::make("fakesink").build().ok()?;
    let video_sink = gstreamer::ElementFactory::make("fakesink").build().ok()?;
    let text_sink = gstreamer::ElementFactory::make("fakesink").build().ok()?;
    video_sink.set_property("sync", false);
    playbin.set_property("uri", uri);
    playbin.set_property("audio-sink", &audio_sink);
    playbin.set_property("video-sink", &video_sink);
    playbin.set_property("text-sink", &text_sink);

    let guard = StopOnDrop(playbin);
    let playbin = &guard.0;

    playbin.set_state(gstreamer::State::Paused).ok()?;
    let (state_res, _, _) = playbin.state(gstreamer::ClockTime::from_seconds(5));
    state_res.ok()?;
    if cancel.is_cancelled() {
        return None;
    }

    if let Some(duration) = playbin.query_duration::<gstreamer::ClockTime>() {
        let target_ns = duration.nseconds() / 6;
        if target_ns > 0 {
            let flags = gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::KEY_UNIT;
            if playbin
                .seek_simple(flags, gstreamer::ClockTime::from_nseconds(target_ns))
                .is_ok()
            {
                let _ = playbin.state(gstreamer::ClockTime::from_seconds(3));
            }
        }
    }
    if cancel.is_cancelled() {
        return None;
    }

    let caps = gstreamer::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build();
    let sample = playbin.emit_by_name::<Option<gstreamer::Sample>>("convert-sample", &[&caps])?;
    let caps = sample.caps()?;
    let structure = caps.structure(0)?;
    let width: i32 = structure.get("width").ok()?;
    let height: i32 = structure.get("height").ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    let rowstride = width.checked_mul(4)?;
    let expected = usize::try_from(rowstride.checked_mul(height)?).ok()?;
    let slice = map.as_slice().get(..expected)?;
    let bytes = glib::Bytes::from(slice);
    let pixbuf = Pixbuf::from_bytes(
        &bytes,
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        width,
        height,
        rowstride,
    );
    // PNG thumbnails use square pixels. Fit the display dimensions rather
    // than the stored video frame, whose pixel aspect ratio can be different.
    let display_width = structure
        .get::<gstreamer::Fraction>("pixel-aspect-ratio")
        .ok()
        .filter(|par| par.numer() > 0)
        .map_or(f64::from(width), |par| {
            f64::from(width) * f64::from(par.numer()) / f64::from(par.denom())
        });
    let display_height = f64::from(height);
    let scale = (f64::from(edge) / display_width.max(display_height)).min(1.0);
    // After rounding, both edges fit in 1..=edge.
    #[allow(clippy::cast_possible_truncation)]
    let (scaled_w, scaled_h) = (
        (display_width * scale).round().max(1.0) as i32,
        (display_height * scale).round().max(1.0) as i32,
    );
    if (scaled_w, scaled_h) == (width, height) {
        Some(pixbuf)
    } else {
        pixbuf.scale_simple(scaled_w, scaled_h, gtk::gdk_pixbuf::InterpType::Bilinear)
    }
}

/// The thumbnail of `request` decoded in the app, or why there is none:
/// [`writer::Failure::Unreadable`] leaves the type to another thumbnailer,
/// the others do not.
async fn by_decoding(
    request: &PreviewRequest,
    cache_dir: &Path,
    modified: u64,
    flavor: ThumbnailFlavor,
) -> Result<PathBuf, writer::Failure> {
    let source = Source {
        uri: request.uri.clone(),
        modified,
    };
    let cache_dir = cache_dir.to_path_buf();
    let decoded = MAKING
        .with(Rc::clone)
        .blocking(move |cancel| writer::write_thumbnail(&cache_dir, &source, flavor, cancel))
        .await;
    // A worker that never ran belongs to a request already dropped.
    decoded.unwrap_or(Err(writer::Failure::Cancelled))
}

/// The thumbnail of `request` made by the desktop's thumbnailer for its
/// type, or [`Missing::TurnedOff`] when the user turned them off for it
/// and [`Missing::Failed`] when none makes it or it failed. With
/// `oversized`, a picture too large for the app to decode, only a
/// thumbnailer whose memory is limited runs.
async fn by_thumbnailer(
    request: &PreviewRequest,
    cache_dir: &Path,
    modified: u64,
    flavor: ThumbnailFlavor,
    catalogue: Option<Arc<Catalogue>>,
    oversized: bool,
) -> Result<PathBuf, Missing> {
    let job = thumbnailers::Job {
        cache_dir: cache_dir.to_path_buf(),
        uri: request.uri.clone(),
        content_type: request.content_type.clone(),
        modified,
        flavor,
    };
    MAKING
        .with(Rc::clone)
        .blocking(move |cancel| {
            // The desktop's entries are files, read in the worker rather
            // than on the thread that asked, and once per run; what the
            // user turned off is read for each request, so a change takes
            // effect at once, and with every program off the entries are
            // not even read. A test hands its own, with nothing off.
            let installed = if let Some(catalogue) = catalogue {
                catalogue
            } else {
                let disabled = thumbnailers::Disabled::of_session();
                if disabled.covers(&job.content_type) {
                    return Err(Missing::TurnedOff);
                }
                Arc::new(thumbnailers::installed().with_disabled(disabled))
            };
            if oversized && !installed.limits_memory() {
                return Err(Missing::Failed);
            }
            thumbnailers::write_thumbnail(&job, &installed, cancel).ok_or(Missing::Failed)
        })
        .await
        .unwrap_or(Err(Missing::Failed))
}

/// The thumbnail at `path`, decoded up to `Large` (256 px) so zooming across
/// icon sizes never upscales a tiny pre-shrunk texture; `None` for a file in
/// the cache that is not a sensible thumbnail (more than the largest standard
/// size, damaged, or slow to read), so the item keeps its icon.
fn decode(path: &std::path::Path) -> Option<gdk::Texture> {
    let file = gio::File::for_path(path);
    let max_edge = ThumbnailFlavor::Large.pixels();
    let decoded =
        picture_guard::decode_to_fit(&file, max_edge, max_edge, Limits::THUMBNAIL_FILE, None).ok()?;
    Some(gdk::Texture::for_pixbuf(&decoded.pixbuf))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use gtk::gdk_pixbuf::Colorspace;
    use ox_core::entry::thumbnail_file;

    use super::*;

    /// A picture whose type no loader declares is still decoded, and a
    /// file that is no picture never reaches gdk-pixbuf.
    ///
    /// parity: VIEW-057
    #[test]
    fn pictures_a_loader_does_not_declare_are_tried() {
        // gdk-pixbuf's dds loader declares `image/x-dds` alone, while GIO
        // names the same files `image/vnd.ms-dds`.
        let declared: HashSet<String> = ["image/jpeg", "image/x-dds"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        assert!(pixbuf_may_read("image/vnd.ms-dds", &declared));
        assert!(pixbuf_may_read("image/jpeg", &declared));
        assert!(!pixbuf_may_read("application/pdf", &declared));
        assert!(!pixbuf_may_read("video/mp4", &declared));
        assert!(!pixbuf_may_read("text/plain", &declared));
        // A loader's own declaration counts too, whatever its kind.
        let font = HashSet::from(["application/x-font-ttf".to_owned()]);
        assert!(pixbuf_may_read("application/x-font-ttf", &font));
    }

    /// Previews follow Settings: off, only in folders on this computer
    /// with network previews off (they are on by default), and not for
    /// files over the limit.
    ///
    /// parity: VIEW-058
    #[test]
    fn previews_follow_the_settings() {
        let mut picture = crate::test_support::file_entry("photo.jpg");
        picture.content_type = Some("image/jpeg".to_owned());
        picture.size = Some(PREVIEW_SIZE_LIMIT + 1);
        let options = ViewOptions {
            remote_previews: false,
            ..ViewOptions::default()
        };

        assert!(
            !PreviewPolicy::for_folder(&options, false).allows(&picture),
            "too large"
        );
        picture.size = Some(4096);
        assert!(PreviewPolicy::for_folder(&options, false).allows(&picture));
        assert!(
            !PreviewPolicy::for_folder(&options, true).allows(&picture),
            "a network folder"
        );
        let off = ViewOptions {
            show_previews: false,
            ..ViewOptions::default()
        };
        assert!(!PreviewPolicy::for_folder(&off, false).allows(&picture));
        let no_pictures = ViewOptions {
            preview_pictures: false,
            ..ViewOptions::default()
        };
        assert!(!PreviewPolicy::for_folder(&no_pictures, false).allows(&picture));

        // A file of a server or a phone listed in a folder of this
        // computer, as a recent item or a search result is, keeps its icon
        // unless network previews are on.
        let local = PreviewPolicy::for_folder(&options, false);
        let mut shared = picture.clone();
        shared.uri = "smb://fileserver.example/team/photo.jpg".to_owned();
        assert!(!local.allows(&shared), "a share");
        // A Recently used files row keeps its file in its metadata, not in
        // its target, which stays empty for a file.
        let mut recent = picture.clone();
        recent.uri = "recent:///3a1b2c".to_owned();
        recent.target_uri = None;
        recent.meta.recent_target = Some("mtp://Example_Phone/DCIM/photo.jpg".to_owned());
        assert!(!local.allows(&recent), "a recent item on a phone");
        let mut recent_share = recent.clone();
        recent_share.meta.recent_target = Some("smb://fileserver.example/team/photo.jpg".to_owned());
        assert!(!local.allows(&recent_share), "a recent item on a share");
        // One on this computer is previewed, and its file is what the
        // network file system check looks at.
        let mut recent_local = recent.clone();
        recent_local.meta.recent_target = Some("file:///home/demo/Pictures/photo.jpg".to_owned());
        let request = local.request(&recent_local, 128).expect("a recent local picture");
        assert!(request.local_only);
        assert_eq!(
            request.target_uri.as_deref(),
            Some("file:///home/demo/Pictures/photo.jpg")
        );
        let everywhere = ViewOptions::default();
        assert!(PreviewPolicy::for_folder(&everywhere, true).allows(&picture));
        assert!(PreviewPolicy::for_folder(&everywhere, false).allows(&shared));
        assert!(PreviewPolicy::for_folder(&everywhere, false).allows(&recent));
    }

    /// A thumbnailer of the test's own, and the file it leaves when it is
    /// started: it hands back `handed.png`, so a test tells its picture
    /// from the app's, and its start from no program run at all.
    fn fake_thumbnailer(folder: &Path) -> (Catalogue, PathBuf) {
        let started = folder.join("started");
        let handed = folder.join("handed.png");
        let picture = Pixbuf::new(Colorspace::Rgb, false, 8, 64, 64).expect("a picture");
        picture.fill(0xcc_3333ff);
        picture.savev(&handed, "png", &[]).expect("the picture");
        let script = folder.join("ox-test-thumbnailer");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\ncp '{}' \"$2\"\n",
                started.display(),
                handed.display()
            ),
        )
        .expect("the script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("the mode");
        let command = [script.to_str().expect("a path"), "%i", "%o", "%s"];
        let thumbnailer = thumbnailers::Thumbnailer::for_test(&command, &["image/png"]);
        (Catalogue::for_test(vec![thumbnailer]), started)
    }

    /// The request for the file at `path`, of `content_type` at the normal
    /// size.
    fn request_of(path: &Path, content_type: &str) -> PreviewRequest {
        PreviewRequest {
            uri: gio::File::for_path(path).uri().to_string(),
            content_type: content_type.to_owned(),
            modified: Some(1_700_000_000),
            pixels: 128,
            local_only: false,
            existing_only: false,
            target_uri: None,
        }
    }

    /// A picture the app cannot read — here a PNG that is no picture, as a
    /// type whose gdk-pixbuf loader is missing looks from the app — is
    /// made by the desktop's thumbnailer for its type, and the app writes
    /// it into the cache as it writes its own.
    ///
    /// parity: VIEW-057
    #[gtk::test]
    fn a_picture_the_app_cannot_read_falls_back_to_the_desktop_thumbnailer() {
        let folder = tempfile::tempdir().expect("a temporary folder");
        let (thumbnailers, started) = fake_thumbnailer(folder.path());
        let cache = folder.path().join("cache");
        let item = folder.path().join("photo.png");
        fs::write(&item, b"not a picture").expect("the item");
        let request = request_of(&item, "image/png");
        // Reading it is what fails, which is the one failure that leaves
        // room for another thumbnailer.
        assert_eq!(
            writer::write_thumbnail(
                &cache,
                &writer::Source {
                    uri: request.uri.clone(),
                    modified: 1_700_000_000,
                },
                ThumbnailFlavor::Normal,
                &gio::Cancellable::new(),
            ),
            Err(writer::Failure::Unreadable)
        );

        let written = glib::MainContext::default()
            .block_on(make_thumbnail_with(
                &request,
                &cache,
                Some(Arc::new(thumbnailers)),
                ThumbnailFlavor::Normal,
                1_700_000_000,
            ))
            .expect("a thumbnail");

        assert!(started.exists(), "the desktop's thumbnailer made it");
        assert_eq!(
            written,
            thumbnail_file(&cache, &request.uri, ThumbnailFlavor::Normal)
        );
        let thumbnail = Pixbuf::from_file(&written).expect("a PNG");
        assert_eq!((thumbnail.width(), thumbnail.height()), (64, 64));
        assert_eq!(
            thumbnail.option("tEXt::Thumb::URI").as_deref(),
            Some(request.uri.as_str())
        );
    }

    /// A picture larger than the app decodes itself, as a scan of many
    /// megapixels is, is made by the desktop's thumbnailer for its type,
    /// whose memory is limited, rather than keeping its icon.
    ///
    /// parity: VIEW-057
    #[gtk::test]
    fn a_picture_too_large_to_decode_goes_to_a_limited_thumbnailer() {
        let folder = tempfile::tempdir().expect("a temporary folder");
        let (thumbnailers, started) = fake_thumbnailer(folder.path());
        let cache = folder.path().join("cache");
        let item = crate::picture_guard::huge_png(&folder.path().join("Scan.png"));
        let request = request_of(&item, "image/png");

        let written = glib::MainContext::default()
            .block_on(make_thumbnail_with(
                &request,
                &cache,
                Some(Arc::new(thumbnailers)),
                ThumbnailFlavor::Normal,
                1_700_000_000,
            ))
            .expect("a thumbnail");

        assert!(started.exists(), "the desktop's thumbnailer made it");
        assert_eq!(
            written,
            thumbnail_file(&cache, &request.uri, ThumbnailFlavor::Normal)
        );
    }

    /// A picture the app reads itself is never handed to a program of the
    /// desktop's, which costs more than decoding it.
    ///
    /// parity: VIEW-057
    #[gtk::test]
    fn a_picture_the_app_reads_starts_no_thumbnailer() {
        let folder = tempfile::tempdir().expect("a temporary folder");
        let (thumbnailers, started) = fake_thumbnailer(folder.path());
        let cache = folder.path().join("cache");
        let item = folder.path().join("photo.png");
        let picture = Pixbuf::new(Colorspace::Rgb, false, 8, 400, 200).expect("a picture");
        picture.fill(0x3366_99ff);
        picture.savev(&item, "png", &[]).expect("the picture");
        let request = request_of(&item, "image/png");

        let written = glib::MainContext::default()
            .block_on(make_thumbnail_with(
                &request,
                &cache,
                Some(Arc::new(thumbnailers)),
                ThumbnailFlavor::Normal,
                1_700_000_000,
            ))
            .expect("a thumbnail");

        assert!(!started.exists(), "the app decoded the picture itself");
        assert_eq!(
            written,
            thumbnail_file(&cache, &request.uri, ThumbnailFlavor::Normal)
        );
        let thumbnail = Pixbuf::from_file(&written).expect("a PNG");
        assert_eq!((thumbnail.width(), thumbnail.height()), (128, 64));
    }

    /// A picture of the cache's own folder is decoded but never written
    /// there, and no program of the desktop's is started to try what the
    /// cache refuses whatever makes it: browsing `~/.cache/thumbnails`
    /// itself forks nothing for its pictures.
    ///
    /// parity: VIEW-057
    #[gtk::test]
    fn a_picture_of_the_cache_itself_starts_no_thumbnailer() {
        let folder = tempfile::tempdir().expect("a temporary folder");
        let (thumbnailers, started) = fake_thumbnailer(folder.path());
        let cache = folder.path().join("cache");
        let inside = cache.join("thumbnails").join(ThumbnailFlavor::Normal.as_str());
        fs::create_dir_all(&inside).expect("the cache folder");
        let item = inside.join("photo.png");
        let picture = Pixbuf::new(Colorspace::Rgb, false, 8, 400, 200).expect("a picture");
        picture.fill(0x3366_99ff);
        picture.savev(&item, "png", &[]).expect("the picture");
        let request = request_of(&item, "image/png");
        assert_eq!(
            writer::write_thumbnail(
                &cache,
                &writer::Source {
                    uri: request.uri.clone(),
                    modified: 1_700_000_000,
                },
                ThumbnailFlavor::Normal,
                &gio::Cancellable::new(),
            ),
            Err(writer::Failure::Unwritable),
            "the picture reads, but the cache refuses its thumbnail"
        );

        let made = glib::MainContext::default().block_on(make_thumbnail_with(
            &request,
            &cache,
            Some(Arc::new(thumbnailers)),
            ThumbnailFlavor::Normal,
            1_700_000_000,
        ));

        assert_eq!(
            made,
            Err(Missing::Failed),
            "the cache holds no pictures of itself"
        );
        assert!(!started.exists(), "and no program is started for one");
    }
}
