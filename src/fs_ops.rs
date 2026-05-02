use anyhow::Result;
use std::path::Path;

/// Delete by moving to the macOS Trash (reversible).
/// Uses the `trash` crate which calls into Finder's file manager APIs on macOS.
pub fn delete_to_trash(path: &Path) -> Result<()> {
    trash::delete(path)?;
    Ok(())
}
