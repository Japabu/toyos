pub fn empty_store() -> Result<(tempfile::TempDir, gix_odb::Handle), std::io::Error> {
    let dir = tempfile::tempdir()?;
    let store = gix_odb::at(dir.path())?;
    Ok((dir, store))
}
