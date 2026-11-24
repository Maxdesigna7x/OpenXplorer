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
    let test = open_with_drive(&fixture);
    let sidebar = test.window.sidebar();
    assert!(descendants::<gtk::Paned>(sidebar).is_empty());

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
    row_named(&test, "Reports").activate();
    wait_until("navigation to Reports", || {
        test.window.current_uri().as_deref() == Some(file_uri(&fixture.path("Documents/Reports")).as_str())
    });

    section_chevron(&test, "Fixture drive").emit_clicked();
    assert!(!sidebar.labels().contains(&"Reports".to_owned()));
    assert!(row_named(&test, "Fixture drive").is_selected());
}
