#[cfg(unix)]
use std::fs;
use std::fs::{File, OpenOptions};
use std::path::Path;

#[cfg(unix)]
pub(super) fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

pub(super) fn open_owner_only(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    restrict_file(&file, 0o600)?;
    Ok(file)
}

#[cfg(unix)]
fn restrict_file(file: &File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict_file(_file: &File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn restrict(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
