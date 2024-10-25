use std::fs;
use std::path::PathBuf;

pub fn path_parser(path: &str) -> Result<PathBuf, &'static str> {
    let tilde_expanded_path = shellexpand::tilde(path);
    Ok(
        fs::canonicalize(tilde_expanded_path.to_string()).unwrap_or_else(|err| {
            panic!("Unable to access path '{}': {}", path, err);
        }),
    )
}
