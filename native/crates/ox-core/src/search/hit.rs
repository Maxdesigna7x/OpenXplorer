// SPDX-License-Identifier: AGPL-3.0-only
//! A search hit as a listed item, so a view shows cached results in the
//! same rows as a folder's items.
//!
//! Ports what `renderRows` in `v2.0.0:desktop/ui/app.js` read from a cached
//! result: the result carried the fields of a listed entry (`search` in
//! `v2.0.0:desktop/search_index.py` returns them), and the rows treated it as
//! one.

use super::query::SearchHit;
use crate::entry::Entry;

impl SearchHit {
    /// The hit as a listed [`Entry`].
    ///
    /// The cache stores only real, non-virtual items and never follows
    /// links (SRCH-031), so a hit can be operated on like any listed item.
    /// Guess the file type from its name so the view can request previews
    /// without opening every file in the results.
    pub fn into_entry(self) -> Entry {
        let content_type = (!self.is_dir)
            .then(|| gio::content_type_guess(Some(&self.name), None))
            .filter(|(content_type, _)| !gio::content_type_is_unknown(content_type))
            .map(|(content_type, _)| content_type.to_string());
        Entry {
            uri: self.uri,
            name: self.name,
            kind: self.kind,
            is_dir: self.is_dir,
            is_virtual: false,
            can_operate: true,
            target_uri: None,
            size: self.size,
            type_label: self.type_label,
            content_type,
            modified: self.modified,
            is_hidden: self.is_hidden,
            is_symlink: false,
            trash_orig_path: None,
            trash_deletion_date: None,
            can_rename: None,
            can_trash: None,
            can_delete: None,
            can_write: None,
            serialized_icon: None,
            meta: Box::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::entry::EntryKind;
    use crate::search::fixtures::{listed_file, listed_folder, search, ScannedShare, SHARE};

    /// parity: SRCH-009
    #[test]
    fn a_hit_lists_as_the_item_it_found() {
        let share = ScannedShare::new();
        let invoices = format!("{SHARE}/Invoices");
        share.store(&[
            listed_folder(SHARE, "Invoices"),
            listed_file(&invoices, "bank statement.pdf"),
        ]);

        let hits = search(&share.index, "bank statement");
        let [hit] = hits.as_slice() else {
            panic!("one hit: {hits:?}");
        };
        let entry = hit.clone().into_entry();

        assert_eq!(entry.name, "bank statement.pdf");
        assert_eq!(entry.uri, hit.uri);
        assert_eq!(entry.kind, EntryKind::File);
        assert!(!entry.is_dir);
        assert_eq!(entry.size, Some(12));
        assert_eq!(entry.content_type.as_deref(), Some("application/pdf"));
        assert!(entry.can_operate, "results are real items");
        assert!(!entry.is_virtual);
        assert_eq!(entry.navigation_uri(), hit.uri);
    }
}
