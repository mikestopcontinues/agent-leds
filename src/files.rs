//! Reading and replacing the files the service and its hooks share with
//! other programs.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result};

/// A file's text, or None if there is no such file.
pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replaces a file's text in one step, so that no reader ever sees half a
/// file. The file keeps its permissions, since a config can hold secrets,
/// and a symbolic link keeps pointing where it did: the file it points to
/// is the one replaced.
pub fn replace(path: &Path, text: &str) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let folder = target.parent().context("a file is in a folder")?;
    fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    let mut staged = tempfile::NamedTempFile::new_in(folder)?;
    staged.write_all(text.as_bytes())?;
    if let Ok(metadata) = fs::metadata(&target) {
        staged.as_file().set_permissions(metadata.permissions())?;
    }
    staged
        .persist(&target)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Removes a file that may not be there.
pub fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn a_missing_file_reads_as_none() {
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(read_optional(&folder.path().join("none")).unwrap(), None);
        assert!(read_optional(folder.path()).is_err());
    }

    #[test]
    fn a_replaced_file_keeps_its_permissions_and_its_folder_is_made() {
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("deep/config.json");
        replace(&file, "one").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        replace(&file, "two").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_link_keeps_pointing_at_the_file_it_replaces() {
        let folder = tempfile::tempdir().unwrap();
        let real = folder.path().join("dotfiles/settings.json");
        let link = folder.path().join("settings.json");
        replace(&real, "old").unwrap();
        symlink(&real, &link).unwrap();
        replace(&link, "new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    }

    #[test]
    fn removing_a_missing_file_is_fine() {
        let folder = tempfile::tempdir().unwrap();
        remove(&folder.path().join("none")).unwrap();
    }
}
