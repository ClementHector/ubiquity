use crate::util::hash_value;
use bincode::{self, deserialize_from, serialize_into};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use fs2::FileExt;
use generic_array::GenericArray;
use std::collections::hash_map;
use std::convert::From;
use std::fmt;
use std::fs;
use std::io;
use std::io::Seek;
use std::path::{Path, PathBuf};

use crate::state::ArchiveEntryPerReplica;
use crate::util::FnvHashMap;
use crate::NumRoots;
use serde::{Deserialize, Serialize};

const ARCHIVE_VERSION: u32 = 3;

pub type HashedPath = u64;

#[derive(Debug, Serialize, Deserialize)]
/// The `Archive` struct stores the state of the replicas after the last syncing operation.
/// It is used to detect differences to replicas more quickly, and must be kept up to date after propagating changes.
pub struct Archive {
    pub directory: PathBuf,
}

impl Archive {
    /// Initializes a directory at the provided path and gets ready to start reading/writing archive data.
    pub fn new(directory: PathBuf) -> Result<Self, io::Error> {
        // creates the archive directory
        if !directory.exists() {
            fs::create_dir_all(&directory)?;
        }
        Ok(Archive { directory })
    }

    /// Constructs an `ArchiveFile` representing the entire `directory` in the replicas.
    pub fn for_directory(&self, directory: &Path) -> ArchiveFile {
        self.for_hashed_directory(Self::hash(directory))
    }

    /// Constructs an `ArchiveFile` from a hashed directory, representing an entire directory in the replicas.
    pub fn for_hashed_directory(&self, directory: HashedPath) -> ArchiveFile {
        let path = self.directory.join(directory.to_string());

        ArchiveFile::new(path)
    }

    pub fn hash(path: &Path) -> HashedPath {
        hash_value(path)
    }
}

/// Abstracts over operations on a single archive file.
/// Remember each 'file' in the archive represents an entire directory (not recursive) in the replicas.
pub struct ArchiveFile {
    path: PathBuf,
    file: Option<fs::File>,
}

impl ArchiveFile {
    /// Creates a new wrapper around the given archive file.
    fn new(path: PathBuf) -> ArchiveFile {
        ArchiveFile { path, file: None }
    }

    /// Remove all entries from this file.
    /// This just slightly more efficient than writing an empty Vec.
    pub fn remove_all(&mut self) -> Result<(), io::Error> {
        if self.path.exists() {
            debug!("Removing {} (because entries are empty)", self);
            self.file = None;
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }

    /// Reads the archive entries into a Vec
    /// This may acquire (or wait for) a lock,
    /// ensuring that multiple threads/processes aren't reading/writing to/from the same archive file.
    pub fn read<N: NumRoots>(&mut self) -> Result<ArchiveEntries<N>, ReadError> {
        if let Some(ref mut file) = self.file {
            let data = read_from_file(file, &self.path)?;
            Ok(ArchiveEntries::new(data))
        } else if self.path.exists() {
            let mut file = self.open_file()?;
            let res = read_from_file(&mut file, &self.path)?;
            self.file = Some(file);
            Ok(ArchiveEntries::new(res))
        } else {
            Ok(ArchiveEntries::empty()) // an empty set of entries
        }
    }

    fn open_file(&self) -> Result<fs::File, io::Error> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.path)?;
        trace!("Acquiring shared lock for {}", self);
        file.lock_exclusive().unwrap();
        trace!("Acquired lock");
        Ok(file)
    }

    /// Writes entries to disk
    pub fn write<N: NumRoots>(
        &mut self,
        entries: &mut ArchiveEntries<N>,
    ) -> Result<(), WriteError> {
        // prevents the archive sizes exploding
        entries.prune_deleted();

        let entries = &entries.entries;
        if entries.is_empty() {
            self.remove_all()?;
        } else if let Some(ref mut file) = self.file {
            write_to_file(file, &self.path, &entries)?;
        } else {
            let mut file = self.open_file()?;
            write_to_file(&mut file, &self.path, &entries)?;
            self.file = Some(file);
        }

        Ok(())
    }
}

impl fmt::Display for ArchiveFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Archive({})",
            self.path.file_name().unwrap().to_str().unwrap()
        )
    }
}

impl Drop for ArchiveFile {
    fn drop(&mut self) {
        if self.path.exists() {
            let f = fs::File::open(&self.path).unwrap();
            trace!("Unlocking archive file {:?}", self.path);
            f.unlock().unwrap();
            trace!("Unlocked");
        }
    }
}

type ArchiveEntryMap<N> = FnvHashMap<HashedPath, GenericArray<ArchiveEntryPerReplica, N>>;

/// Stores all the archive entries for a specific directory
pub struct ArchiveEntries<N: NumRoots> {
    entries: ArchiveEntryMap<N>,
    dirty: bool,
}

impl<N: NumRoots> fmt::Debug for ArchiveEntries<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.entries.fmt(formatter)
    }
}

impl<N: NumRoots> ArchiveEntries<N> {
    pub fn empty() -> Self {
        ArchiveEntries {
            entries: Default::default(),
            dirty: false,
        }
    }

    fn new(entries: ArchiveEntryMap<N>) -> Self {
        ArchiveEntries {
            entries,
            dirty: false,
        }
    }

    /// Returns an iterator over the entries.
    pub fn iter(&self) -> hash_map::Iter<HashedPath, GenericArray<ArchiveEntryPerReplica, N>> {
        self.entries.iter()
    }

    pub fn get(&self, path: &Path) -> Option<&GenericArray<ArchiveEntryPerReplica, N>> {
        self.entries.get(&Archive::hash(path))
    }

    pub fn insert(&mut self, path: &Path, entries: GenericArray<ArchiveEntryPerReplica, N>) {
        let hashed_path = Archive::hash(path);
        self.entries.insert(hashed_path, entries);
        self.dirty = true;
    }

    // Loops through each ArchiveEntry and removes it if all replicas are empty.
    // Without this archive sizes will probably explode if enough files are created,
    // synced and then deleted.
    pub fn prune_deleted(&mut self) {
        let empties: Vec<_> = self
            .entries
            .iter()
            .filter(|&(_, ref entry)| {
                let mut delete = true;
                for replica in entry.iter() {
                    match replica {
                        ArchiveEntryPerReplica::Empty => {}
                        _ => delete = false,
                    }
                }

                if delete {
                    info!("Removing empty entry before writing");
                }
                delete
            })
            .map(|(k, _)| *k)
            .collect();
        for empty in empties {
            self.entries.remove(&empty);
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

fn read_from_file<N: NumRoots>(
    file: &mut fs::File,
    path: &Path,
) -> Result<ArchiveEntryMap<N>, ReadError> {
    debug!("Reading archive file {:?}", path);
    file.seek(io::SeekFrom::Start(0))?;
    match read_entries(file) {
        Ok(i) => Ok(i),
        Err(ReadError::InvalidArchiveVersion(version)) => {
            error!("Invalid archive version {} for file {:?}", version, path);
            Ok(Default::default())
        }
        // TODO: if have forgotten when this error case is encountered
        // out of safety, I have removed this case and instead a hard error will be thrown
        //
        // Err(ReadError::BincodeError(bincode::Error::Serde(serde::de::value::Error::EndOfStream))) => {
        //     error!("End of stream when reading archive file at path {:?}", path);
        //     Ok(Default::default())
        // },
        Err(e) => Err(e),
    }
}

fn write_to_file<N: NumRoots>(
    file: &mut fs::File,
    path: &Path,
    entries: &ArchiveEntryMap<N>,
) -> Result<(), WriteError> {
    info!("Writing to archive file {:?}: {:#?}", path, entries);
    file.set_len(0)?;

    let pos = file.seek(io::SeekFrom::Start(0))?;
    assert_eq!(pos, 0);

    write_entries(file, entries)?;

    Ok(())
}

/// reads a set of entries from a binary stream
fn read_entries<R, N>(read: &mut R) -> Result<ArchiveEntryMap<N>, ReadError>
where
    R: io::Read,
    N: NumRoots,
{
    let version = read.read_u32::<LittleEndian>()?;
    if version != ARCHIVE_VERSION {
        return Err(ReadError::InvalidArchiveVersion(version));
    }
    let result = deserialize_from(read)?;
    Ok(result)
}

// writes a set of entries to a binary stream
fn write_entries<W, N>(out: &mut W, entries: &ArchiveEntryMap<N>) -> Result<(), WriteError>
where
    W: io::Write,
    N: NumRoots,
{
    out.write_u32::<LittleEndian>(ARCHIVE_VERSION)?;
    serialize_into(out, &entries)?;
    Ok(())
}

#[derive(Debug)]
/// Various errors explaining why an archive file couldn't be read
pub enum ReadError {
    InvalidArchiveVersion(u32),
    IoError(io::Error),
    BincodeError(bincode::Error),
}

impl From<bincode::Error> for ReadError {
    fn from(e: bincode::Error) -> Self {
        ReadError::BincodeError(e)
    }
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::IoError(e)
    }
}

#[derive(Debug)]
/// Various errors explaining why an archive file couldn't be written to
pub enum WriteError {
    IoError(io::Error),
    BincodeError(bincode::Error),
}

impl From<bincode::Error> for WriteError {
    fn from(e: bincode::Error) -> Self {
        WriteError::BincodeError(e)
    }
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::IoError(e)
    }
}
