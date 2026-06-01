use crate::{Error, Result, id::RiftId};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn path(workspace: &Path) -> PathBuf {
    workspace.join(".rift")
}

pub(crate) fn write(workspace: &Path, id: &RiftId) -> Result<()> {
    fs::write(path(workspace), format!("{id}\n"))?;
    Ok(())
}

pub(crate) fn read(workspace: &Path) -> Result<Option<RiftId>> {
    let contents = match fs::read_to_string(path(workspace)) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(RiftId::from_stored(contents.trim().to_owned())))
}

pub(crate) fn verify(workspace: &Path, expected_id: &RiftId) -> Result<()> {
    if read(workspace)?.as_ref() == Some(expected_id) {
        return Ok(());
    }
    Err(Error::MarkerMismatch(workspace.to_path_buf()))
}
