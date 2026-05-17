use std::ffi::OsString;
use std::path::PathBuf;

/// Resolves an external CLI tool: sibling of the avxs binary first, then PATH.
pub fn external_bin(name: &str) -> OsString {
    let file_name = with_exe_suffix(name);

    if let Some(sibling) = sibling_of_exe(&file_name)
        && sibling.is_file()
    {
        return sibling.into_os_string();
    }

    OsString::from(name)
}

fn with_exe_suffix(name: &str) -> String {
    let suffix = std::env::consts::EXE_SUFFIX;
    if suffix.is_empty() || name.ends_with(suffix) {
        name.to_string()
    } else {
        format!("{name}{suffix}")
    }
}

fn sibling_of_exe(file_name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join(file_name))
}
