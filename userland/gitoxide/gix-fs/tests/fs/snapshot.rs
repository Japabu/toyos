use std::path::Path;

use gix_fs::SharedFileSnapshotMut;

#[test]
fn journey() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir().unwrap();
    if !has_granular_times(tmp.path())? {
        return Ok(());
    }

    let file_path = tmp.path().join("content");
    let smut = SharedFileSnapshotMut::<String>::new();

    let check = || file_path.metadata().ok()?.modified().ok();
    let open = || {
        Ok(match std::fs::read_to_string(&file_path) {
            Ok(s) => Some(s),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        })
    };
    let snap = smut.recent_snapshot(check, open)?;
    assert!(snap.is_none());

    std::fs::write(&file_path, "content")?;
    let snap = smut.recent_snapshot(check, open)?.expect("content read");
    assert_eq!(&**snap, "content", "it read the file for the first time");

    std::fs::write(&file_path, "change")?;
    let snap = smut.recent_snapshot(check, open)?.expect("content read");
    assert_eq!(&**snap, "change", "it picks up the change");

    std::fs::remove_file(&file_path)?;
    let snap = smut.recent_snapshot(check, open)?;
    assert!(snap.is_none(), "file deleted, nothing to see here");

    std::fs::write(&file_path, "new")?;
    let snap = smut.recent_snapshot(check, open)?.expect("content read again");
    let owned: String = snap.into_owned_or_cloned();
    assert_eq!(owned, "new", "owned versions are possible easily and efficiently");
    Ok(())
}

fn has_granular_times(root: &Path) -> std::io::Result<bool> {
    let n = 50;

    let paths = (0..n).map(|i| root.join(format!("{i:03}")));
    for (index, path) in paths.clone().enumerate() {
        std::fs::write(&path, index.to_string().as_bytes())?;
    }
    let mut times = Vec::new();
    for path in paths {
        times.push(path.symlink_metadata()?.modified()?);
    }
    times.sort();
    times.dedup();

    // This could be wrongly false if a filesystem has very precise timings yet is ridiculously
    // fast. Then the `journey` test wouldn't run, though it could. But that's OK, and unlikely.
    if cfg!(target_os = "macos") && is_ci::cached() {
        assert_eq!(
            times.len(),
            n,
            "should have very granular timestamps at least on macOS on CI"
        );
    }
    Ok(times.len() == n)
}
