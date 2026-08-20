use log::error;
use std::path::{Path, PathBuf};

pub struct TempFileGuard {
    pub path: Option<PathBuf>,
}

impl TempFileGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn promote<P: AsRef<Path>>(&mut self, new_name: P) -> std::io::Result<()> {
        let path = self
            .path
            .as_ref()
            .expect("cannot promote non-existent file");
        std::fs::rename(path, new_name)?;
        self.path = None;
        Ok(())
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if let Err(e) = std::fs::remove_file(path) {
                error!("Failed to remove temp DB: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "snapshot-parser-temp-file-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_promoted_file_lands_under_its_final_name_and_is_not_removed() {
        let dir = temp_dir();
        let temp = dir.join("_snapshot.db.tmp");
        let final_path = dir.join("snapshot.db");
        std::fs::write(&temp, b"db").unwrap();

        let mut guard = TempFileGuard::new(temp.clone());
        guard.promote(&final_path).unwrap();
        drop(guard);

        assert!(!temp.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"db");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_was_never_promoted_is_removed() {
        let dir = temp_dir();
        let temp = dir.join("_snapshot.db.tmp");
        std::fs::write(&temp, b"db").unwrap();

        drop(TempFileGuard::new(temp.clone()));

        assert!(!temp.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_promotion_still_removes_the_temporary_file() {
        let dir = temp_dir();
        let temp = dir.join("_snapshot.db.tmp");
        std::fs::write(&temp, b"db").unwrap();
        let occupied = dir.join("occupied");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("child"), b"x").unwrap();

        let mut guard = TempFileGuard::new(temp.clone());
        guard
            .promote(&occupied)
            .expect_err("promoting onto a non-empty directory must fail");
        assert!(temp.exists(), "the guard must still own the file");
        drop(guard);

        assert!(!temp.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
