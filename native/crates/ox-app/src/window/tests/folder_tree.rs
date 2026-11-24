// SPDX-License-Identifier: AGPL-3.0-only
//! Mounted drive folders in the navigation pane.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use ox_core::location::file_uri;

use super::sidebar::{row_named, section_chevron};
use crate::test_support::harness::{descendants, wait_until, Fixture, TestWindow};
use crate::test_support::mounted_volume;
use crate::volumes::VolumeKind;

pub(super) fn open_with_drive(fixture: &Fixture) -> TestWindow {
    let test = TestWindow::open(ox_core::location::PC_URI);
    test.window.imp().volumes.replace(vec![mounted_volume(
        "Fixture drive",
        &fixture.uri(),
        VolumeKind::Drive,
    )]);
    test.window.render_places();
    test
}

/// parity: SIDE-028
#[gtk::test]
fn drive_folders_expand_in_the_same_sidebar_list() {
    let fixture = Fixture::standard();
    std::fs::create_dir(fixture.path("Documents/Reports")).unwrap();
    std::fs::create_dir(fixture.path("Documents/ZArchive")).unwrap();
    std::fs::create_dir_all(fixture.path("Documents/ZArchive/Year")).unwrap();
    std::fs::write(fixture.path("Documents/Reports/report.txt"), b"fixture").unwrap();
    let test = open_with_drive(&fixture);
    let sidebar = test.window.sidebar();
    assert!(descendants::<gtk::Paned>(sidebar).is_empty());

    wait_until("an arrow on the drive with folders", || {
        section_chevron(&test, "Fixture drive").is_visible()
    });
    assert!(!sidebar.labels().iter().any(|label| label == "Reports"));
    section_chevron(&test, "Fixture drive").emit_clicked();
    wait_until("the drive's folders", || {
        let labels = sidebar.labels();
        labels
            .iter()
            .position(|label| label == "Fixture drive")
            .is_some_and(|drive| labels.get(drive + 1).is_some_and(|label| label == "Documents"))
    });
    let drive = row_named(&test, "Fixture drive");
    let mut next = drive.index() + 1;
    // Known folders may share a name with folders on the fixture drive.
    let folders = sidebar.labels();
    while folders[next as usize] != "Documents" {
        next += 1;
        assert!(next < folders.len() as i32);
    }
    let documents = sidebar.list().row_at_index(next).unwrap();
    assert!(documents.has_css_class("branch"));
    documents.activate();
    wait_until("navigation to Documents", || {
        test.window.current_uri().as_deref() == Some(fixture.uri_of("Documents").as_str())
    });
    wait_until("the new folder's branch", || {
        sidebar.labels().contains(&"Reports".to_owned())
    });
    wait_until("an arrow on the folder that has a subfolder", || {
        section_chevron(&test, "ZArchive").is_visible()
    });
    assert!(
        !section_chevron(&test, "Reports").is_visible(),
        "a folder with only files has no arrow"
    );
    row_named(&test, "Reports").activate();
    wait_until("navigation to Reports", || {
        test.window.current_uri().as_deref() == Some(file_uri(&fixture.path("Documents/Reports")).as_str())
    });

    section_chevron(&test, "Fixture drive").emit_clicked();
    assert!(!sidebar.labels().contains(&"Reports".to_owned()));
    assert!(row_named(&test, "Fixture drive").is_selected());
}

#[gtk::test]
fn alternating_tabs_on_two_drives_keeps_the_window_and_folder_branches() {
    let first = Fixture::standard();
    let second = Fixture::standard();
    std::fs::create_dir_all(first.path("Documents/First folder/Inside first")).unwrap();
    std::fs::create_dir_all(second.path("Documents/Second folder/Inside second")).unwrap();
    let test = open_with_drive(&first);
    let window = &test.window;
    window
        .imp()
        .volumes
        .borrow_mut()
        .push(mounted_volume("Second drive", &second.uri(), VolumeKind::Drive));
    window.render_places();
    let first_uri = file_uri(&first.path("Documents/First folder"));
    let second_uri = file_uri(&second.path("Documents/Second folder"));
    window.navigate(&first_uri).unwrap();
    test.wait_for_listing("first drive folder");
    window.add_tab(&second_uri).unwrap();
    test.wait_for_listing("second drive folder");
    for _ in 0..30 {
        test.activate("previous-tab", None);
        assert_eq!(window.current_uri().as_deref(), Some(first_uri.as_str()));
        test.activate("next-tab", None);
        assert_eq!(window.current_uri().as_deref(), Some(second_uri.as_str()));
        crate::test_support::harness::settle();
    }
    wait_until("the paths in both drives", || {
        let labels = window.sidebar().labels();
        labels.iter().any(|label| label == "Inside first")
            && labels.iter().any(|label| label == "Inside second")
    });
    assert!(window.is_mapped());
}

#[gtk::test]
fn opening_a_drive_root_keeps_it_folded() {
    let fixture = Fixture::standard();
    let test = open_with_drive(&fixture);
    test.window.navigate(&fixture.uri()).expect("drive root");
    wait_until("drive chevron", || {
        section_chevron(&test, "Fixture drive").is_visible()
    });
    assert_eq!(
        section_chevron(&test, "Fixture drive").tooltip_text().as_deref(),
        Some("Expand Fixture drive")
    );
}
