use std::fs;
use std::path::PathBuf;

pub fn path_parser(path: &str) -> Result<PathBuf, String> {
    let tilde_expanded_path = shellexpand::tilde(path);
    fs::canonicalize(tilde_expanded_path.to_string())
        .map_err(|err| format!("Unable to access path '{path}': {err}"))
}
