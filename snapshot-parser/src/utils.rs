use serde::de::DeserializeOwned;
use serde::Serialize;
use solana_native_token::LAMPORTS_PER_SOL;
use std::path::Path;
use std::{
    fs,
    fs::File,
    io::{BufReader, BufWriter, Write},
};

pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL as f64
}

// The pipeline treats an existing file as a complete one, so a partial write must never reach it
fn write_atomic<F>(out_path: &str, write: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut BufWriter<File>) -> anyhow::Result<()>,
{
    let tmp_path = format!("{out_path}.tmp");
    let result = (|| {
        let mut writer = BufWriter::new(File::create(&tmp_path)?);
        write(&mut writer)?;
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|err| anyhow::anyhow!("Failed to flush {tmp_path}: {err}"))?
            .sync_all()?;

        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
        return result;
    }
    fs::rename(&tmp_path, out_path)?;

    Ok(())
}

pub fn write_to_json_file<T: Serialize>(data: &T, out_path: &str) -> anyhow::Result<()> {
    write_atomic(out_path, |writer| {
        serde_json::to_writer_pretty(writer, data)?;

        Ok(())
    })
}

pub fn write_to_json_file_compact<T: Serialize>(data: &T, out_path: &str) -> anyhow::Result<()> {
    write_atomic(out_path, |writer| {
        serde_json::to_writer(writer, data)?;

        Ok(())
    })
}

pub fn read_from_json_file<P: AsRef<Path>, T: DeserializeOwned>(in_path: &P) -> anyhow::Result<T> {
    let file = File::open(in_path)?;
    let reader = BufReader::new(file);
    let result: T = serde_json::from_reader(reader)?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A fixed name in the shared temp dir collides between concurrent test runs on one host
    struct OutDir(std::path::PathBuf);

    impl OutDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "snapshot-parser-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> String {
            self.0.join("out.json").to_string_lossy().into_owned()
        }
    }

    impl Drop for OutDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pretty_writer_keeps_the_previous_artifact_bytes() {
        let data = json!({"epoch": 1002, "stake_metas": [{"pubkey": "abc", "lamports": 1}]});
        let out = OutDir::new("pretty");
        write_to_json_file(&data, &out.path()).unwrap();

        assert_eq!(
            fs::read_to_string(out.path()).unwrap(),
            serde_json::to_string_pretty(&data).unwrap()
        );
    }

    #[test]
    fn writers_leave_no_temporary_file_behind() {
        let data = json!({"epoch": 1002});
        let out = OutDir::new("no-tmp");
        write_to_json_file_compact(&data, &out.path()).unwrap();

        assert_eq!(fs::read_to_string(out.path()).unwrap(), r#"{"epoch":1002}"#);
        assert!(!Path::new(&format!("{}.tmp", out.path())).exists());
    }

    #[test]
    fn a_failing_serialization_leaves_the_destination_untouched() {
        let out = OutDir::new("preserved");
        write_to_json_file_compact(&json!({"complete": true}), &out.path()).unwrap();

        assert!(write_to_json_file_compact(&NonSerializable, &out.path()).is_err());
        assert_eq!(
            fs::read_to_string(out.path()).unwrap(),
            r#"{"complete":true}"#,
            "a failed write must not clobber the previous complete file"
        );
        assert!(!Path::new(&format!("{}.tmp", out.path())).exists());
    }

    struct NonSerializable;

    impl Serialize for NonSerializable {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("boom"))
        }
    }
}
