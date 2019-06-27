use std::collections::hash_map::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chownr;
use digest::Digest;
use either::{Left, Right};
use hex;
use mkdirp;
use serde_derive::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::Sha256;
use ssri::Integrity;
use walkdir::WalkDir;

use crate::errors::Error;
use crate::put::PutOpts;

const INDEX_VERSION: &str = "5";

/// Represents a cache index entry, which points to content.
#[derive(PartialEq, Debug)]
pub struct Entry {
    pub key: String,
    pub integrity: Integrity,
    pub time: u128,
    pub size: usize,
    pub metadata: Value,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
struct SerializableEntry {
    key: String,
    integrity: Option<String>,
    time: u128,
    size: usize,
    metadata: Value,
}

pub fn insert(cache: &Path, key: &str, opts: PutOpts) -> Result<Integrity, Error> {
    let bucket = bucket_path(&cache, &key);
    if let Some(path) = mkdirp::mkdirp(bucket.parent().unwrap())? {
        chownr::chownr(&path, opts.uid, opts.gid)?;
    }
    let stringified = serde_json::to_string(&SerializableEntry {
        key: key.to_owned(),
        integrity: opts.sri.clone().map(|x| x.to_string()),
        time: opts.time.unwrap_or_else(now),
        size: opts.size.unwrap_or(0),
        metadata: opts.metadata.unwrap_or_else(|| json!(null)),
    })?;
    let str = format!("\n{}\t{}", hash_entry(&stringified), stringified);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&bucket)?
        .write_all(&str.into_bytes())?;
    chownr::chownr(&bucket, opts.uid, opts.gid)?;
    Ok(opts
        .sri
        .unwrap_or_else(|| "sha1-deadbeef".parse::<Integrity>().unwrap()))
}

pub fn find(cache: &Path, key: &str) -> Result<Option<Entry>, Error> {
    let bucket = bucket_path(cache, &key);
    Ok(bucket_entries(&bucket)?
        .into_iter()
        .fold(None, |acc, entry| {
            if entry.key == key {
                if let Some(integrity) = entry.integrity {
                    let integrity: Integrity = match integrity.parse() {
                        Ok(sri) => sri,
                        _ => return acc,
                    };
                    Some(Entry {
                        key: entry.key,
                        integrity,
                        size: entry.size,
                        time: entry.time,
                        metadata: entry.metadata,
                    })
                } else {
                    None
                }
            } else {
                acc
            }
        }))
}

pub fn delete(cache: &Path, key: &str) -> Result<(), Error> {
    insert(cache, key, PutOpts {
            algorithm: None,
            size: None,
            sri: None,
            time: None,
            metadata: None,
            uid: None,
            gid: None,
        }
    )?;
    Ok(())
}

pub fn ls(cache: &Path) -> impl Iterator<Item = Result<Entry, Error>> {
    let mut path = PathBuf::new();
    path.push(cache);
    path.push(format!("index-v{}", INDEX_VERSION));
    WalkDir::new(path)
        .into_iter()
        .map(|bucket| {
            let bucket = bucket?;
            if bucket.file_type().is_dir() {
                return Ok(core::iter::empty().collect::<Vec<Entry>>());
            }
            let entries = bucket_entries(bucket.path())?;
            let mut dedupe: HashMap<String, SerializableEntry> = HashMap::new();
            for entry in entries {
                dedupe.insert(entry.key.clone(), entry);
            }
            let iter = dedupe
                .into_iter()
                .filter(|se| se.1.integrity.is_some())
                .map(|se| {
                    let se = se.1;
                    Entry {
                        key: se.key,
                        integrity: se.integrity.unwrap().parse().unwrap(),
                        time: se.time,
                        size: se.size,
                        metadata: se.metadata,
                    }
                });
            Ok(iter.collect::<Vec<Entry>>())
        })
        .flat_map(|res| match res {
            Ok(it) => Left(it.into_iter().map(Ok)),
            Err(err) => Right(std::iter::once(Err(err))),
        })
}

fn bucket_path(cache: &Path, key: &str) -> PathBuf {
    let hashed = hash_key(&key);
    let mut path = PathBuf::new();
    path.push(cache);
    path.push(format!("index-v{}", INDEX_VERSION));
    path.push(&hashed[0..2]);
    path.push(&hashed[2..4]);
    path.push(&hashed[4..]);
    path
}

fn hash_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.input(&key);
    hex::encode(hasher.result())
}

fn hash_entry(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.input(&key);
    hex::encode(hasher.result())
}

fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn bucket_entries(bucket: &Path) -> Result<Vec<SerializableEntry>, Error> {
    let lines = match fs::read_to_string(bucket) {
        Ok(data) => Ok(data),
        Err(ref e) if e.kind() == ErrorKind::NotFound => Ok(String::from("")),
        err => err,
    }?;
    Ok(lines.split('\n').fold(vec![], |mut acc, entry: &str| {
        if entry.is_empty() {
            return acc;
        }
        let entry_str = match entry.split('\t').collect::<Vec<&str>>()[..] {
            [hash, entry_str] => {
                if hash_entry(entry_str) != hash {
                    // Hash is no good! Corruption or malice? Doesn't matter!
                    // EJECT EJECT
                    return acc;
                } else {
                    entry_str
                }
            }
            // Something's wrong with the entry. Abort.
            _ => return acc,
        };
        if let Ok(entry) = serde_json::from_str::<SerializableEntry>(entry_str) {
            acc.push(entry)
        }
        acc
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    const MOCK_ENTRY: &str = "\n251d18a2b33264ea8655695fd23c88bd874cdea2c3dc9d8f9b7596717ad30fec\t{\"key\":\"hello\",\"integrity\":\"sha1-deadbeef\",\"time\":1234567,\"size\":0,\"metadata\":null}";

    #[test]
    fn insert_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        let sri: Integrity = "sha1-deadbeef".parse().unwrap();
        let time = 1_234_567;
        let opts = PutOpts::new().integrity(sri).time(time);
        insert(&dir, "hello", opts).unwrap();
        let entry = std::fs::read_to_string(bucket_path(&dir, "hello")).unwrap();
        assert_eq!(entry, MOCK_ENTRY);
    }

    #[test]
    fn find_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        let sri: Integrity = "sha1-deadbeef".parse().unwrap();
        let time = 1_234_567;
        let bucket = bucket_path(&dir, "hello");
        mkdirp::mkdirp(bucket.parent().unwrap()).unwrap();
        fs::write(bucket, MOCK_ENTRY).unwrap();
        let entry = find(&dir, "hello").unwrap().unwrap();
        assert_eq!(
            entry,
            Entry {
                key: String::from("hello"),
                integrity: sri,
                time,
                size: 0,
                metadata: json!(null)
            }
        );
    }

    #[test]
    fn find_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        assert_eq!(find(&dir, "hello").unwrap(), None);
    }

    #[test]
    fn delete_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        let sri: Integrity = "sha1-deadbeef".parse().unwrap();
        let time = 1_234_567;
        let opts = PutOpts::new().integrity(sri).time(time);
        insert(&dir, "hello", opts).unwrap();
        delete(&dir, "hello").unwrap();
        assert_eq!(find(&dir, "hello").unwrap(), None);
    }

    #[test]
    fn ls_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        let sri: Integrity = "sha1-deadbeef".parse().unwrap();
        let time = 1_234_567;
        let opts = PutOpts::new().integrity(sri.clone()).time(time);
        insert(&dir, "hello", opts).unwrap();
        let opts = PutOpts::new().integrity(sri).time(time);
        insert(&dir, "world", opts).unwrap();

        let mut entries = ls(&dir)
            .map(|x| Ok(x?.key))
            .collect::<Result<Vec<_>, Error>>()
            .unwrap();
        entries.sort();
        assert_eq!(entries, vec![String::from("hello"), String::from("world")])
    }
}
