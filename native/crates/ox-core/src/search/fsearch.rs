// SPDX-License-Identifier: AGPL-3.0-only
//! Read-only interoperability with `FSearch`'s FSDB 6.0 snapshot format.
//! The search keeps names and metadata in memory; it never copies the database
//! to another disk or changes `FSearch`'s configuration.

use std::collections::HashSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufReader, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gio::prelude::*;

use super::query::{SearchHit, SearchQuery, SearchResults, MAX_QUERY_CHARS, MAX_RESULT_LIMIT};
use super::{FacetMatcher, HiddenItems, RootStatus, SearchError};
use crate::entry::EntryKind;
use crate::location::file_uri;

const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000_000;
const NO_PARENT: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Revision {
    device: u64,
    inode: u64,
    length: u64,
    seconds: i64,
    nanos: i64,
}

impl From<Metadata> for Revision {
    fn from(meta: Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            length: meta.len(),
            seconds: meta.mtime(),
            nanos: meta.mtime_nsec(),
        }
    }
}

#[derive(Debug, Default)]
struct Table {
    names: String,
    keys: String,
    rows: Vec<Record>,
}

#[derive(Debug)]
struct Record {
    name_end: u32,
    key_end: u32,
    parent: u32,
    size: Option<std::num::NonZeroU64>,
    modified: Option<std::num::NonZeroU64>,
}

impl Table {
    fn name(&self, at: usize) -> &str {
        let start = at.checked_sub(1).map_or(0, |prev| self.rows[prev].name_end);
        &self.names[start as usize..self.rows[at].name_end as usize]
    }

    fn key(&self, at: usize) -> &str {
        let start = at.checked_sub(1).map_or(0, |prev| self.rows[prev].key_end);
        &self.keys[start as usize..self.rows[at].key_end as usize]
    }

    fn read_block(input: &mut impl Read, count: usize, flags: u64, folders: usize) -> io::Result<Self> {
        let mut table = Self {
            rows: Vec::with_capacity(count),
            ..Self::default()
        };
        let mut name = Vec::new();
        for _ in 0..count {
            let prefix = usize::from(u16::from_le_bytes(take(input)?));
            let suffix = usize::from(u16::from_le_bytes(take(input)?));
            if prefix > name.len() || prefix + suffix > 16_384 {
                return invalid();
            }
            name.resize(prefix + suffix, 0);
            input.read_exact(&mut name[prefix..])?;
            // A non-UTF-8 name is not converted into a different actionable path.
            if let Some(text) = std::str::from_utf8(&name)
                .ok()
                .filter(|name| !name.contains('\0'))
            {
                table.names.push_str(text);
                if text.is_ascii() {
                    let start = table.keys.len();
                    table.keys.push_str(text);
                    table.keys[start..].make_ascii_lowercase();
                } else {
                    table.keys.push_str(&text.to_lowercase());
                }
            }
            let size = property(input, flags & 4 != 0, true)?;
            let modified = property(input, flags & 8 != 0, false)?;
            let parent = u32::from_le_bytes(take(input)?);
            if parent != NO_PARENT && parent as usize >= folders {
                return invalid();
            }
            table.rows.push(Record {
                name_end: table.names.len().try_into().map_err(|_| invalid_error())?,
                key_end: table.keys.len().try_into().map_err(|_| invalid_error())?,
                parent,
                size,
                modified,
            });
        }
        Ok(table)
    }
}

fn property(
    input: &mut impl Read,
    present: bool,
    zero_valid: bool,
) -> io::Result<Option<std::num::NonZeroU64>> {
    if !present {
        return Ok(None);
    }
    let value = i64::from_le_bytes(take(input)?);
    Ok(u64::try_from(value).ok().and_then(|value| {
        if zero_valid {
            std::num::NonZeroU64::new(value + 1)
        } else {
            std::num::NonZeroU64::new(value)
        }
    }))
}

fn take<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn invalid_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Unsupported or incomplete FSearch snapshot",
    )
}

fn invalid<T>() -> io::Result<T> {
    Err(invalid_error())
}

fn skip_string(input: &mut impl Read) -> io::Result<()> {
    let bytes = u32::from_le_bytes(take(input)?) as usize;
    if bytes > 16_384 {
        return invalid();
    }
    input.read_exact(&mut vec![0; bytes])
}

fn skip_rules(input: &mut impl Read, excludes: bool) -> io::Result<()> {
    let count = u32::from_le_bytes(take(input)?);
    if count > 4096 {
        return invalid();
    }
    for _ in 0..count {
        let _record_type = take::<4>(input)?;
        if excludes {
            let _kind_scope_target = take::<3>(input)?;
            skip_string(input)?;
            let _active = take::<1>(input)?;
        } else {
            skip_string(input)?;
            let _options_and_scan_metadata = take::<37>(input)?;
        }
    }
    if excludes {
        let _hide_hidden = take::<1>(input)?;
    }
    Ok(())
}

#[derive(Debug)]
struct Snapshot {
    folders: Table,
    files: Table,
}

impl Snapshot {
    fn read(file: File, length: u64) -> io::Result<Self> {
        if length > MAX_SNAPSHOT_BYTES {
            return invalid();
        }
        let mut input = BufReader::with_capacity(1024 * 1024, file);
        if take::<7>(&mut input)? != *b"FSDB\x06\0\x01" {
            return invalid();
        }
        let flags = u64::from_le_bytes(take(&mut input)?);
        let _sort_flags = take::<8>(&mut input)?;
        skip_rules(&mut input, false)?;
        skip_rules(&mut input, true)?;
        let folders = u32::from_le_bytes(take(&mut input)?) as usize;
        let files = u32::from_le_bytes(take(&mut input)?) as usize;
        let folder_bytes = u64::from_le_bytes(take(&mut input)?);
        let file_bytes = u64::from_le_bytes(take(&mut input)?);
        let _header_checksum = take::<16>(&mut input)?;
        if folders + files > MAX_ENTRIES
            || folder_bytes.saturating_add(file_bytes) > length
            || folder_bytes / 8 < folders as u64
            || file_bytes / 8 < files as u64
        {
            return invalid();
        }
        let mut folder_block = (&mut input).take(folder_bytes);
        let folder_table = Table::read_block(&mut folder_block, folders, flags, folders)?;
        if folder_block.limit() != 0 {
            return invalid();
        }
        let mut file_block = input.take(file_bytes);
        let file_table = Table::read_block(&mut file_block, files, flags, folders)?;
        if file_block.limit() != 0 {
            return invalid();
        }
        Ok(Self {
            folders: folder_table,
            files: file_table,
        })
    }

    // Parent chains can refer forward. The bound also guards against a
    // cyclic chain in a damaged or externally modified snapshot.
    fn folder_path(&self, mut at: usize) -> Option<PathBuf> {
        let mut parts = Vec::new();
        for _ in 0..256 {
            let name = self.folders.name(at);
            if name.is_empty() {
                return None;
            }
            let parent = self.folders.rows[at].parent as usize;
            if parent == at || parent == NO_PARENT as usize {
                let base = Path::new(name);
                if !base.is_absolute()
                    || base
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
                {
                    return None;
                }
                let mut path = base.to_path_buf();
                path.extend(parts.into_iter().rev());
                return Some(path);
            }
            if !is_component(name) {
                return None;
            }
            parts.push(name);
            at = parent;
        }
        None
    }
}

fn is_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

fn local_path(path: &Path, mounted: &[PathBuf], network: &[PathBuf]) -> bool {
    if network.iter().any(|root| path.starts_with(root)) {
        return false;
    }
    let external = ["/media", "/mnt", "/run/media"]
        .iter()
        .any(|root| path.starts_with(root));
    !external || mounted.iter().any(|root| path.starts_with(root))
}

#[derive(Debug, Default)]
struct Matcher {
    terms: Vec<String>,
    wildcards: Vec<super::NamePattern>,
    paths: Vec<String>,
    extensions: Vec<String>,
    directories: Option<bool>,
}

impl Matcher {
    fn parse(text: &str) -> Self {
        let mut matcher = Self::default();
        let mut quoted = false;
        let mut term = String::new();
        for ch in text.chars().chain(std::iter::once(' ')) {
            if ch == '"' {
                quoted = !quoted;
            } else if ch.is_whitespace() && !quoted {
                matcher.add(&term);
                term.clear();
            } else {
                term.push(ch);
            }
        }
        matcher.add(term.trim_end());
        matcher
    }

    fn add(&mut self, term: &str) {
        let word = term.to_lowercase();
        match word.as_str() {
            "" => (),
            "file:" => self.directories = Some(false),
            "folder:" => self.directories = Some(true),
            _ => {
                if let Some(ext) = word.strip_prefix("ext:") {
                    self.extensions
                        .extend(ext.split(';').filter(|ext| !ext.is_empty()).map(str::to_owned));
                } else if let Some(path) = word.strip_prefix("path:") {
                    self.paths.push(path.to_owned());
                } else if word.contains('/') {
                    self.paths.push(word);
                } else if super::is_wildcard(&word) {
                    self.wildcards.push(super::NamePattern::new(&word));
                } else {
                    self.terms.push(word);
                }
            }
        }
    }

    fn matches_name(&self, name: &str) -> bool {
        self.terms.iter().all(|term| name.contains(term))
            && self.wildcards.iter().all(|word| word.matches_lowercase(name, ""))
            && (self.extensions.is_empty()
                || name
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| self.extensions.iter().any(|choice| choice == ext)))
    }
}

/// A process-wide reader of the user's `FSearch` snapshot. It reloads if `FSearch`
/// replaces the file, and shares the decoded tables between concurrent searches.
#[derive(Debug, Default)]
pub struct FsearchSnapshot {
    cached: Mutex<Option<(Revision, Option<Arc<Snapshot>>)>>,
}

impl FsearchSnapshot {
    /// Searches the current snapshot. Missing or incompatible files return
    /// no source so callers can use their own index.
    ///
    /// # Errors
    /// An invalid or cancelled query.
    pub fn search(
        &self,
        file: &Path,
        query: &SearchQuery,
        offset: usize,
        facets: FacetMatcher,
        local_paths: (&[PathBuf], &[PathBuf]),
        cancellable: &gio::Cancellable,
    ) -> Result<Option<SearchResults>, SearchError> {
        if query.text.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchError::QueryTooLong);
        }
        if cancellable.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let started = Instant::now();
        let Some(snapshot) = self.load(file) else {
            return Ok(None);
        };
        let context = Selection {
            matcher: Matcher::parse(&query.text),
            facets,
            mounted: local_paths.0,
            network: local_paths.1,
            hidden: query.hidden_items == HiddenItems::Include,
            offset,
            limit: query.limit.clamp(1, MAX_RESULT_LIMIT),
        };
        let hits = snapshot.find(&context, cancellable)?;
        let is_truncated = hits.len() > context.limit;
        Ok(Some(SearchResults {
            hits: hits.into_iter().take(context.limit).collect(),
            limit: context.limit,
            is_truncated,
            elapsed: started.elapsed(),
        }))
    }

    fn load(&self, path: &Path) -> Option<Arc<Snapshot>> {
        if !std::fs::symlink_metadata(path).ok()?.is_file() {
            return None;
        }
        let flags = (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(i32::try_from(flags).ok()?)
            .open(path)
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() {
            return None;
        }
        let revision = Revision::from(meta);
        let mut cached = self.cached.lock().ok()?;
        if let Some((_, snapshot)) = cached.as_ref().filter(|(seen, _)| *seen == revision) {
            return snapshot.clone();
        }
        let snapshot = Snapshot::read(file, revision.length).ok().map(Arc::new);
        *cached = Some((revision, snapshot.clone()));
        snapshot
    }
}

struct Selection<'a> {
    matcher: Matcher,
    facets: FacetMatcher,
    mounted: &'a [PathBuf],
    network: &'a [PathBuf],
    hidden: bool,
    offset: usize,
    limit: usize,
}

impl Snapshot {
    fn find(
        &self,
        selection: &Selection<'_>,
        cancellable: &gio::Cancellable,
    ) -> Result<Vec<SearchHit>, SearchError> {
        let mut seen = HashSet::new();
        let mut hits = Vec::new();
        let parent_terms: Vec<_> = selection
            .matcher
            .paths
            .iter()
            .filter_map(|path| {
                let (parent, _) = path.rsplit_once('/')?;
                (!parent.is_empty()).then_some(parent)
            })
            .collect();
        let mut viable_parents = if parent_terms.is_empty() {
            Vec::new()
        } else {
            vec![None; self.folders.rows.len()]
        };
        for (table, dir) in [(&self.folders, true), (&self.files, false)] {
            if selection.matcher.directories.is_some_and(|only| only != dir) {
                continue;
            }
            for (at, row) in table.rows.iter().enumerate() {
                if at % 1024 == 0 && cancellable.is_cancelled() {
                    return Err(SearchError::Cancelled);
                }
                if !selection.matcher.matches_name(table.key(at)) {
                    continue;
                }
                if !dir && !parent_terms.is_empty() && row.parent != NO_PARENT {
                    let parent = row.parent as usize;
                    if !*viable_parents[parent].get_or_insert_with(|| {
                        self.folder_path(parent).is_some_and(|path| {
                            let key = path.to_string_lossy().to_lowercase();
                            parent_terms.iter().all(|part| key.contains(part))
                        })
                    }) {
                        continue;
                    }
                }
                let name = table.name(at);
                let size = row.size.map(|size| size.get() - 1).filter(|_| !dir);
                let modified = row.modified.map(std::num::NonZeroU64::get);
                if !selection.facets.matches_metadata(name, dir, size, modified, None) {
                    continue;
                }
                let path = if dir {
                    self.folder_path(at)
                } else if is_component(name) && row.parent != NO_PARENT {
                    self.folder_path(row.parent as usize)
                        .map(|parent| parent.join(name))
                } else {
                    None
                };
                let Some(path) = path else { continue };
                let hidden = path
                    .components()
                    .any(|part| part.as_os_str().as_encoded_bytes().first() == Some(&b'.'));
                if (!selection.hidden && hidden) || !local_path(&path, selection.mounted, selection.network) {
                    continue;
                }
                if !selection.matcher.paths.is_empty() {
                    let key = path.to_string_lossy().to_lowercase();
                    if !selection.matcher.paths.iter().all(|part| key.contains(part)) {
                        continue;
                    }
                }
                if path.file_name().is_none() || !seen.insert(path.clone()) || seen.len() <= selection.offset
                {
                    continue;
                }
                if let Some(hit) = snapshot_hit(&path, name, dir, size, modified, hidden) {
                    hits.push(hit);
                }
                if hits.len() > selection.limit {
                    return Ok(hits);
                }
            }
        }
        Ok(hits)
    }
}

fn snapshot_hit(
    path: &Path,
    name: &str,
    dir: bool,
    size: Option<u64>,
    modified: Option<u64>,
    hidden: bool,
) -> Option<SearchHit> {
    let parent = path.parent()?;
    let base = path.file_name()?;
    let type_label = if dir {
        crate::i18n::gettext("Folder")
    } else {
        let (kind, _) = gio::content_type_guess(Some(name), None);
        gio::content_type_get_description(&kind).into()
    };
    Some(SearchHit {
        uri: file_uri(path),
        parent_uri: file_uri(parent),
        name: base.to_string_lossy().into_owned(),
        is_dir: dir,
        size,
        modified,
        type_label,
        kind: if dir {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        is_hidden: hidden,
        path: path.to_string_lossy().into_owned(),
        cached_at: None,
        root_updated: None,
        root_status: RootStatus::Ready,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(items: &[(&str, u32)]) -> Vec<u8> {
        let mut data = Vec::new();
        for (name, parent) in items {
            data.extend(0_u16.to_le_bytes());
            data.extend(u16::try_from(name.len()).unwrap().to_le_bytes());
            data.extend(name.as_bytes());
            data.extend(42_i64.to_le_bytes());
            data.extend(100_i64.to_le_bytes());
            data.extend(parent.to_le_bytes());
        }
        data
    }

    fn fixture(path: &Path, files: &[(&str, u32)]) {
        let folders = block(&[("/media/demo/disk", 0), ("/mnt/missing", 1), (".hidden", 0)]);
        let names = block(files);
        let mut data = b"FSDB\x06\0\x01".to_vec();
        data.extend(15_u64.to_le_bytes());
        data.extend(0_u64.to_le_bytes());
        data.extend([0; 9]);
        data.extend(3_u32.to_le_bytes());
        data.extend(u32::try_from(files.len()).unwrap().to_le_bytes());
        data.extend((folders.len() as u64).to_le_bytes());
        data.extend((names.len() as u64).to_le_bytes());
        data.extend([0; 16]);
        data.extend(folders);
        data.extend(names);
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn reads_a_snapshot_filters_unmounted_and_hidden_paths_and_reloads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fsearch.db");
        fixture(
            &path,
            &[
                ("Alpha.PDF", 0),
                ("Alpha.PDF", 0),
                ("Beta.pdf", 0),
                ("Hidden.pdf", 2),
                ("Offline.pdf", 1),
            ],
        );
        let reader = FsearchSnapshot::default();
        let mounted = [PathBuf::from("/media/demo/disk")];
        let cancellable = gio::Cancellable::new();
        let mut query = SearchQuery::new("file: ext:pdf");
        query.limit = 1;
        let search = |offset| {
            reader
                .search(
                    &path,
                    &query,
                    offset,
                    FacetMatcher::default(),
                    (&mounted, &[]),
                    &cancellable,
                )
                .unwrap()
                .unwrap()
        };
        let first = search(0);
        assert!(first.is_truncated);
        assert_eq!(first.hits[0].name, "Alpha.PDF");
        assert_eq!(first.hits[0].size, Some(42));
        assert_eq!(first.hits[0].modified, Some(100));
        let last = search(1);
        assert!(!last.is_truncated);
        assert_eq!(last.hits[0].name, "Beta.pdf");
        let replacement = path.with_extension("new");
        fixture(&replacement, &[("Replaced.pdf", 0)]);
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(search(0).hits[0].name, "Replaced.pdf");
        assert!(search(1).hits.is_empty());

        std::fs::write(&path, b"FSDB").unwrap();
        assert!(reader
            .search(
                &path,
                &query,
                0,
                FacetMatcher::default(),
                (&mounted, &[]),
                &cancellable,
            )
            .unwrap()
            .is_none());
    }
}
