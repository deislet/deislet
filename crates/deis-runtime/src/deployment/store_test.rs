use super::*;
use crate::app::AppKey;

async fn store() -> (tempfile::TempDir, BundleStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BundleStore::open(dir.path().join("bundles")).await.unwrap();
    (dir, store)
}

#[test]
fn accepts_the_identifiers_the_control_plane_actually_uses() {
    for id in ["my-app", "app_1", "0.1.0", "b3f1c2d4-aaaa"] {
        check_identifier("app_id", id).unwrap();
    }
}

#[test]
fn refuses_an_identifier_that_could_escape_the_bundle_directory() {
    for id in ["", ".", "..", "../etc", "a/b", "a\\b", "a b", "app$"] {
        assert!(
            check_identifier("app_id", id).is_err(),
            "{id:?} was accepted as a path segment"
        );
    }
}

#[tokio::test]
async fn a_traversing_identifier_never_reaches_the_filesystem() {
    let (_dir, store) = store().await;
    assert!(store
        .bundle_path(&AppKey::production("../../etc"), "v1")
        .is_err());
    assert!(store
        .bundle_path(&AppKey::new("app", "../../etc"), "v1")
        .is_err());
    assert!(store
        .bundle_path(&AppKey::production("app"), "../../etc")
        .is_err());
    // A dot in either half would make the tenant id split into three parts,
    // so one pair could impersonate another.
    assert!(store
        .bundle_path(&AppKey::production("app.preview"), "v1")
        .is_err());
}

#[tokio::test]
async fn a_bundle_lands_under_its_app_and_version() {
    let (_dir, store) = store().await;
    let temp = store.temp_path();
    tokio::fs::write(&temp, b"zip bytes").await.unwrap();

    let path = store
        .adopt(&temp, &AppKey::production("my-app"), "v1")
        .await
        .unwrap();

    assert!(
        path.ends_with("apps/my-app/production/v1.zip"),
        "{}",
        path.display()
    );
    assert_eq!(store.read_bundle(&path).await.unwrap(), b"zip bytes");
    assert!(!temp.exists(), "the temp file should have been moved");
}

#[tokio::test]
async fn the_record_survives_being_written_and_read_back() {
    let (_dir, store) = store().await;
    let record = InstalledRecord {
        app_id: "my-app".into(),
        canonical_application_id: None,
        version_id: "v1".into(),
        environment: "production".into(),
        canonical_environment_id: None,
        bundle_sha: "abc123".into(),
        environment_snapshot_id: None,
    };

    store.remember(record.clone()).await;
    assert_eq!(store.records().await, vec![record]);
}

#[tokio::test]
async fn canonical_identity_survives_the_installed_record() {
    let (_dir, store) = store().await;
    let record = InstalledRecord {
        app_id: "my-app".into(),
        canonical_application_id: Some("018f2518-7e38-7f80-8000-000000000001".parse().unwrap()),
        version_id: "v1".into(),
        environment: "production".into(),
        canonical_environment_id: Some("018f2518-7e38-7f80-8000-000000000002".parse().unwrap()),
        bundle_sha: "abc123".into(),
        environment_snapshot_id: Some("snapshot-1".into()),
    };

    store.remember(record.clone()).await;
    let read = store.records().await;
    assert_eq!(read, vec![record]);
    assert_eq!(
        read[0].canonical_tenant().unwrap().unwrap().to_string(),
        "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002"
    );
}

#[tokio::test]
async fn a_partial_canonical_identity_is_not_recovered() {
    let (_dir, store) = store().await;
    let path = store.root.join(RECORD_FILE);
    tokio::fs::write(
        path,
        r#"[{
          "app_id":"my-app",
          "canonical_application_id":"018f2518-7e38-7f80-8000-000000000001",
          "version_id":"v1",
          "environment":"production",
          "bundle_sha":"abc123"
        }]"#,
    )
    .await
    .unwrap();

    assert!(store.records().await.is_empty());
}

#[tokio::test]
async fn canonical_identity_can_be_learned_but_never_reassigned_or_erased() {
    let (_dir, store) = store().await;
    let application = "018f2518-7e38-7f80-8000-000000000001".parse().unwrap();
    let environment = "018f2518-7e38-7f80-8000-000000000002".parse().unwrap();
    let stored = deis_wire::CanonicalTenantId::new(application, environment);
    store
        .remember(InstalledRecord {
            app_id: "my-app".into(),
            canonical_application_id: Some(application),
            version_id: "v1".into(),
            environment: "production".into(),
            canonical_environment_id: Some(environment),
            bundle_sha: "abc123".into(),
            environment_snapshot_id: None,
        })
        .await;

    let key = AppKey::production("my-app");
    let mut omitted_by_old_control = None;
    store
        .reconcile_canonical_tenant(&key, &mut omitted_by_old_control)
        .await
        .unwrap();
    assert_eq!(omitted_by_old_control, Some(stored));

    let other_environment = "018f2518-7e38-7f80-8000-000000000003".parse().unwrap();
    let received = deis_wire::CanonicalTenantId::new(application, other_environment);
    let mut reassigned = Some(received);
    assert_eq!(
        store
            .reconcile_canonical_tenant(&key, &mut reassigned)
            .await
            .unwrap_err(),
        CanonicalTenantConflict::Changed { stored, received }
    );
}

#[tokio::test]
async fn a_second_version_of_an_app_replaces_its_line_rather_than_adding_one() {
    let (_dir, store) = store().await;
    for version in ["v1", "v2"] {
        store
            .remember(InstalledRecord {
                app_id: "my-app".into(),
                canonical_application_id: None,
                version_id: version.into(),
                environment: "production".into(),
                canonical_environment_id: None,
                bundle_sha: "sha".into(),
                environment_snapshot_id: None,
            })
            .await;
    }

    let records = store.records().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].version_id, "v2");
}

#[tokio::test]
async fn forgetting_an_app_removes_its_bundles_and_its_line() {
    let (_dir, store) = store().await;
    let temp = store.temp_path();
    tokio::fs::write(&temp, b"zip").await.unwrap();
    let path = store
        .adopt(&temp, &AppKey::production("my-app"), "v1")
        .await
        .unwrap();
    store
        .remember(InstalledRecord {
            app_id: "my-app".into(),
            canonical_application_id: None,
            version_id: "v1".into(),
            environment: "production".into(),
            canonical_environment_id: None,
            bundle_sha: "sha".into(),
            environment_snapshot_id: None,
        })
        .await;

    store.forget("my-app").await;

    assert!(!path.exists(), "the bundle outlived the app");
    assert!(store.records().await.is_empty());
}

#[tokio::test]
async fn pruning_keeps_only_the_version_still_in_use() {
    let (_dir, store) = store().await;
    for version in ["v1", "v2"] {
        let temp = store.temp_path();
        tokio::fs::write(&temp, b"zip").await.unwrap();
        store
            .adopt(&temp, &AppKey::production("my-app"), version)
            .await
            .unwrap();
    }

    store.prune(&AppKey::production("my-app"), "v2").await;

    assert!(!store
        .bundle_path(&AppKey::production("my-app"), "v1")
        .unwrap()
        .exists());
    assert!(store
        .bundle_path(&AppKey::production("my-app"), "v2")
        .unwrap()
        .exists());
}

#[tokio::test]
async fn a_partial_download_from_a_previous_run_is_swept_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("bundles");
    let store = BundleStore::open(root.clone()).await.unwrap();
    let leftover = store.temp_path();
    tokio::fs::write(&leftover, b"half a bundle").await.unwrap();

    // A second open stands for a restart.
    let _store = BundleStore::open(root).await.unwrap();

    assert!(
        !leftover.exists(),
        "an unverified partial download survived a restart"
    );
}

#[tokio::test]
async fn an_unreadable_record_is_ignored_rather_than_failing_startup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("bundles");
    tokio::fs::create_dir_all(&root).await.unwrap();
    tokio::fs::write(root.join("installed.json"), b"{not json")
        .await
        .unwrap();

    let store = BundleStore::open(root).await.unwrap();

    assert!(store.records().await.is_empty());
}
