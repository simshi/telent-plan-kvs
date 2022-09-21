use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Deserializer;

use crate::{KvsEngine, KvsError, Result};

// compact if more than `threshold` bytes can be saved
const COMPACTION_THRESHOLD: u64 = 1024 * 1024;

/// The `KvStore` stores string key/value pairs.
///
/// Key/value pairs are persisted to disk in log files. Log files are named after
/// monotonically increasing generation numbers with a `log` extension name.
/// A `BTreeMap` in memory stores the keys and the value locations for fast query.
///
/// Example:
///
/// ```rust
/// # use kvs::{KvStore, Result};
/// # fn try_main() -> Result<()> {
/// use std::env::current_dir;
/// use kvs::KvsEngine;
/// let mut store = KvStore::open(current_dir()?)?;
/// store.set("key".to_owned(), "value".to_owned())?;
/// let val = store.get("key".to_owned())?;
/// assert_eq!(val, Some("value".to_owned()));
/// # Ok(())
/// # }
/// ```
pub struct KvStore {
    // path for the log files.
    path: PathBuf,
    index: BTreeMap<String, CommandPos>,
    // map gen to file reader
    readers: HashMap<u64, BufReader<File>>,
    current_gen: u64,
    // log writer
    writer: BufWriterWithPos<File>,
    // the number of bytes representing "stale" commands that could be
    // deleted during a compaction.
    stale_bytes: u64,
}

impl KvStore {
    /// Open the KvStore at a given path. Return the KvStore.
    ///
    /// This will create a new directory if the given one does not exist.
    ///
    /// # Errors
    /// It propagates I/O or deserialization errors during the log replay.
    pub fn open(path: impl Into<PathBuf>) -> Result<KvStore> {
        let path = path.into();
        fs::create_dir_all(&path)?;

        let mut index = BTreeMap::new();
        // build all exist logs into readers
        let mut readers = HashMap::new();

        let gen_list = sorted_gen_list(&path)?;
        let mut stale_bytes = 0;

        for &gen in &gen_list {
            let filepath = log_file_path(&path, gen);
            let mut reader = BufReader::new(File::open(&filepath)?);
            stale_bytes += load_log(gen, &mut reader, &mut index)?;
            readers.insert(gen, reader);
        }

        let current_gen = gen_list.last().unwrap_or(&0) + 1;
        let writer = new_log_file(&path, current_gen, &mut readers)?;

        Ok(KvStore {
            path,
            index,
            readers,
            current_gen,
            writer,
            stale_bytes,
        })
    }

    /// Collect space by writing entries to a new log file then remove old log
    /// files, staled bytes then gone.
    fn compact(&mut self) -> Result<()> {
        // current_gen + 1 for the compaction log.
        let compaction_gen = self.current_gen + 1;
        self.current_gen += 2;
        self.writer = self.new_log_file(self.current_gen)?;

        // write all KV to a new log file.
        let mut compaction_writer = self.new_log_file(compaction_gen)?;
        let mut new_pos = 0;
        for cmd_pos in self.index.values_mut() {
            let rdr = self
                .readers
                .get_mut(&cmd_pos.gen)
                .expect("Cannot find log reader");

            rdr.seek(SeekFrom::Start(cmd_pos.pos))?;
            let mut entry_rdr = rdr.take(cmd_pos.len);
            let len = io::copy(&mut entry_rdr, &mut compaction_writer)?;

            *cmd_pos = (compaction_gen, new_pos, len).into();
            new_pos = compaction_writer.pos;
        }
        compaction_writer.flush()?;

        // remove staled log files.
        // `cloned` to break the reference to `self`
        let staled_gens: Vec<_> = self
            .readers
            .keys()
            .filter(|&&gen| gen < compaction_gen)
            .cloned()
            .collect();

        for staled_gen in staled_gens {
            self.readers.remove(&staled_gen);
            fs::remove_file(log_file_path(&self.path, staled_gen))?;
        }

        // fresh as new born
        self.stale_bytes = 0;

        Ok(())
    }

    /// Create a new log file with given generation number and add the reader to the readers map.
    ///
    /// Returns the writer to the log.
    fn new_log_file(&mut self, gen: u64) -> Result<BufWriterWithPos<File>> {
        new_log_file(&self.path, gen, &mut self.readers)
    }
}

impl KvsEngine for KvStore {
    /// Sets the value of a string key to a string.
    ///
    /// If the key already exists, the previous value will be overwritten.
    ///
    /// # Errors
    /// It propagates I/O or serialization errors during writing the log.
    fn set(&mut self, key: String, value: String) -> Result<()> {
        let cmd = Command::set(key, value);
        let pos = self.writer.pos;
        serde_json::to_writer(&mut self.writer, &cmd)?;
        self.writer.flush()?;
        let len = self.writer.pos - pos;

        if let Command::Set { key, .. } = cmd {
            // println!("insert key {:?} value {:?}", key, value);
            if let Some(CommandPos { len, .. }) =
                self.index.insert(key, (self.current_gen, pos, len).into())
            {
                // overwritten case
                self.stale_bytes += len;
            }
        }

        if self.stale_bytes > COMPACTION_THRESHOLD {
            self.compact()?;
        }
        // println!("set: {:?}", serde_json::to_string(&cmd).unwrap());
        Ok(())
    }

    /// Gets the string value of a given string key.
    ///
    /// Returns `None` if the given key does not exist.
    ///
    /// # Errors
    /// It returns `KvsError::UnexpectedCommandType` if the given command type unexpected.
    fn get(&mut self, key: String) -> Result<Option<String>> {
        if let Some(cmd_pos) = self.index.get(&key) {
            // eprintln!("get: {:?}, pos:{:?}", key, &cmd_pos);
            let reader = self
                .readers
                .get_mut(&cmd_pos.gen)
                .expect("Cannot find log reader");

            reader.seek(SeekFrom::Start(cmd_pos.pos))?;
            let cmd_rdr = reader.take(cmd_pos.len);
            if let Command::Set { value, .. } = serde_json::from_reader(cmd_rdr)? {
                Ok(Some(value))
            } else {
                Err(KvsError::UnexpectedCommandType)
            }
        } else {
            Ok(None)
        }
    }

    /// Remove a given key.
    ///
    /// # Errors
    /// It returns `KvsError::KeyNotFound` if the given key is not found.
    /// It propagates I/O or serialization errors during writing the log.
    fn remove(&mut self, key: String) -> Result<()> {
        // don't remove the key immediately, make sure writer successful first!
        if self.index.contains_key(&key) {
            // println!("find key: {:?}", &key);
            let cmd = Command::remove(key);
            serde_json::to_writer(&mut self.writer, &cmd)?;
            self.writer.flush()?;

            // flushed, now we're safe to remove the key
            if let Command::Remove { key } = cmd {
                if let Some(CommandPos { len, .. }) = self.index.remove(&key) {
                    self.stale_bytes += len;
                }
            }
            Ok(())
        } else {
            // println!("not find key: {:?}", key);
            Err(KvsError::KeyNotFound)
        }
    }
}
/// Struct representing a command.
#[derive(Debug, Serialize, Deserialize)]
enum Command {
    Set { key: String, value: String },
    Remove { key: String },
}
impl Command {
    fn set(key: String, value: String) -> Self {
        Command::Set { key, value }
    }

    fn remove(key: String) -> Self {
        Command::Remove { key }
    }
}

/// Represents the position and length of a (json)serialized command in the log.
#[derive(Debug)]
struct CommandPos {
    gen: u64,
    pos: u64,
    len: u64,
}
impl From<(u64, u64, u64)> for CommandPos {
    fn from((gen, pos, len): (u64, u64, u64)) -> Self {
        CommandPos { gen, pos, len }
    }
}

// trace pos/len because `serde_json::to_write()` doesn't return written size
struct BufWriterWithPos<W: Write + Seek> {
    inner: BufWriter<W>,
    pos: u64,
}
impl<W: Write + Seek> BufWriterWithPos<W> {
    fn new(mut inner: W) -> Result<Self> {
        let pos = inner.seek(SeekFrom::Current(0))?;
        Ok(BufWriterWithPos {
            inner: BufWriter::new(inner),
            pos,
        })
    }
}

impl<W: Write + Seek> Write for BufWriterWithPos<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = self.inner.write(buf)?;
        self.pos += len as u64;

        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl<W: Write + Seek> Seek for BufWriterWithPos<W> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

fn log_file_path(dir: &Path, gen: u64) -> PathBuf {
    dir.join(format!("{}.log", gen))
}

/// Create a new log file with given generation number and add the reader to the readers map.
///
/// Returns the writer to the log.
fn new_log_file(
    path: &Path,
    gen: u64,
    readers: &mut HashMap<u64, BufReader<File>>,
) -> Result<BufWriterWithPos<File>> {
    let filepath = log_file_path(path, gen);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&filepath)?;
    let writer = BufWriterWithPos::new(file)?;

    readers.insert(gen, BufReader::new(File::open(&filepath)?));

    Ok(writer)
}

/// Returns sorted generation numbers in the given directory.
fn sorted_gen_list(path: &Path) -> Result<Vec<u64>> {
    let mut gen_list: Vec<u64> = fs::read_dir(&path)?
        .flat_map(|res| -> Result<_> { Ok(res?.path()) })
        .filter(|path| path.is_file() && path.extension() == Some("log".as_ref()))
        .flat_map(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .map(|s| s.trim_end_matches(".log"))
                .map(str::parse::<u64>)
        })
        .flatten()
        .collect();

    gen_list.sort_unstable();
    Ok(gen_list)
}

/// Load the whole log file and store value locations in the index map.
///
/// Returns how many bytes can be saved after a compaction.
fn load_log(
    gen: u64,
    reader: &mut BufReader<File>,
    index: &mut BTreeMap<String, CommandPos>,
) -> Result<u64> {
    // To make sure we read from the beginning of the file.
    let mut pos = reader.seek(SeekFrom::Start(0))?;
    let mut stream = Deserializer::from_reader(reader).into_iter::<Command>();
    let mut staled_bytes = 0; // number of bytes that can be saved after a compaction.

    while let Some(cmd) = stream.next() {
        let new_pos = stream.byte_offset() as u64;
        match cmd? {
            Command::Set { key, .. } => {
                let cmd_pos = CommandPos {
                    gen,
                    pos,
                    len: new_pos - pos,
                };
                if let Some(old_cmd) = index.insert(key, cmd_pos) {
                    staled_bytes += old_cmd.len;
                }
            }
            Command::Remove { key } => {
                if let Some(old_cmd) = index.remove(&key) {
                    staled_bytes += old_cmd.len;
                }
                // the "remove" command itself can be deleted in the next compaction.
                // so we add its length to `uncompacted`.
                staled_bytes += new_pos - pos;
            }
        }
        pos = new_pos;
    }

    Ok(staled_bytes)
}
