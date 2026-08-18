//! Plan module: IR types, I/O, and normalization.

pub mod bundle;
pub mod normalization;
pub mod steps;
pub mod types;

pub use types::*;

use crate::error::PlanError;
use std::io::Write;
use std::path::Path;

const TEMP_FILE_SUFFIX: &str = ".tmp";

/// Load a plan from a JSON file path.
pub fn load_from_file(path: &Path) -> Result<Plan, PlanError> {
    let raw = read_file(path)?;
    let plan: Plan = serde_json::from_str(&raw)?;
    Ok(plan)
}

/// Serialise a plan to pretty-printed JSON and write it to a file.
pub fn save_to_file(plan: &Plan, path: &Path) -> Result<(), PlanError> {
    let json = serde_json::to_string_pretty(plan)?;
    write_file_atomically(path, &json)
}

/// Parse a plan from a JSON string.
pub fn from_json(json: &str) -> Result<Plan, PlanError> {
    let plan: Plan = serde_json::from_str(json)?;
    Ok(plan)
}

/// Serialise a plan to a pretty-printed JSON string.
pub fn to_json(plan: &Plan) -> Result<String, PlanError> {
    Ok(serde_json::to_string_pretty(plan)?)
}

pub(crate) fn read_file(path: &Path) -> Result<String, PlanError> {
    std::fs::read_to_string(path).map_err(|error| contextual_io_error("read", path, error))
}

/// Write complete content to a unique sibling and atomically replace `path`.
///
/// Syncing both the temporary file and its parent directory keeps the old or
/// new complete artifact durable across an interruption; a partial target is
/// never exposed.
pub(crate) fn write_file_atomically(path: &Path, content: &str) -> Result<(), PlanError> {
    let parent = parent_directory(path);
    std::fs::create_dir_all(parent)
        .map_err(|error| contextual_io_error("create parent directory for", path, error))?;

    let file_name = path.file_name().ok_or_else(|| {
        PlanError::Invalid(format!(
            "cannot write plan artifact without a file name: '{}'",
            path.display()
        ))
    })?;
    let temp_path = path.with_file_name(format!(
        "{}.{}{TEMP_FILE_SUFFIX}",
        file_name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));

    let result = (|| -> Result<(), std::io::Error> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp_path, path)?;
        sync_parent_directory(path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result.map_err(|error| contextual_io_error("write atomically", path, error))
}

fn contextual_io_error(operation: &str, path: &Path, error: std::io::Error) -> PlanError {
    PlanError::Io(std::io::Error::new(
        error.kind(),
        format!("{operation} '{}': {error}", path.display()),
    ))
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>, std::io::Error> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "plan artifact path contains an embedded NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that live
    // through the call. The documented flags request replacement and durable
    // write-through semantics, and the return value is checked.
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sync_parent_directory(_path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    std::fs::File::open(parent_directory(_path))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn sample_plan(name: &str) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: name.to_owned(),
            description: None,
            inputs: Vec::new(),
            config: IndexMap::new(),
            steps: Vec::new(),
            outputs: Vec::new(),
        }
    }

    #[test]
    fn direct_plan_save_atomically_creates_and_replaces() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("plan.json");

        save_to_file(&sample_plan("first"), &path).unwrap();
        save_to_file(&sample_plan("second"), &path).unwrap();

        let loaded = load_from_file(&path).unwrap();
        assert_eq!(loaded.name, "second");
        let siblings = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(siblings.len(), 1, "temporary files must be cleaned up");
    }

    #[test]
    fn failed_atomic_write_keeps_existing_target_and_reports_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let sentinel = target.join("sentinel");
        std::fs::write(&sentinel, "unchanged").unwrap();

        let error = write_file_atomically(&target, "replacement").unwrap_err();

        assert!(error.to_string().contains(&target.display().to_string()));
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
        let temporary_files = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(TEMP_FILE_SUFFIX)
            })
            .count();
        assert_eq!(temporary_files, 0);
    }

    #[test]
    fn failed_read_reports_the_target_path() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.json");

        let error = read_file(&missing).unwrap_err();

        assert!(error.to_string().contains(&missing.display().to_string()));
    }
}
