use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;
use std::{
    fs::File,
    io::{BufReader, BufWriter, Write},
};

pub fn write_to_json_file<T: Serialize>(data: &T, out_path: &str) -> anyhow::Result<()> {
    let file = File::create(out_path)?;
    let mut writer = BufWriter::new(file);
    let json = serde_json::to_string_pretty(data)?;
    writer.write_all(json.as_bytes())?;
    writer.flush()?;

    Ok(())
}

pub fn read_from_json_file<P: AsRef<Path>, T: DeserializeOwned>(in_path: &P) -> anyhow::Result<T> {
    let file = File::open(in_path)?;
    let reader = BufReader::new(file);
    let result: T = serde_json::from_reader(reader)?;

    Ok(result)
}
