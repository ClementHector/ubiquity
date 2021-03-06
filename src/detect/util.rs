use std::fs;
use std::path::{Path, PathBuf};

use generic_array::GenericArray;

use crate::config::{Ignore, SyncInfo};
use crate::error::SyncError;
use crate::state::ArchiveEntryPerReplica;
use crate::util::FnvHashMap;
use crate::NumRoots;

pub fn check_all_roots_exist<'a, I: Iterator<Item = &'a PathBuf>>(
    roots: I,
) -> Result<(), SyncError> {
    for root in roots {
        if !root.exists() {
            return Err(SyncError::RootDoesntExist(root.to_path_buf()));
        }
    }
    Ok(())
}

/// checks that all the archive files for this path are identical
pub fn are_archive_files_identical<N: NumRoots>(
    a: &GenericArray<ArchiveEntryPerReplica, N>,
    b: &GenericArray<ArchiveEntryPerReplica, N>,
) -> bool {
    for (a, b) in a.iter().zip(b.iter()) {
        if a != b {
            return false;
        }
    }
    true
}

/// checks if the path is on the ignore list
pub fn is_ignored(ignore: &Ignore, path: &Path) -> bool {
    for ignore in &ignore.paths {
        //trace!("{:?} starts with {:?} = {}", path, ignore, path.starts_with(ignore));
        if path.starts_with(ignore) {
            return true;
        }
    }
    for ignore in &ignore.regexes {
        //trace!("{:?} is match {:?} = {}", ignore, path.to_str().unwrap(), ignore.is_match(path.to_str().unwrap()));
        if ignore.is_match(path.to_str().unwrap()) {
            return true;
        }
    }

    false
}

pub fn scan_directory_contents<N>(
    directory: &Path,
    current_entries: &mut FnvHashMap<PathBuf, GenericArray<ArchiveEntryPerReplica, N>>,
    config: &SyncInfo<N>,
) -> Result<(), SyncError>
where
    N: NumRoots,
{
    // when looking at the contents of this search directory, we must check if the
    // search directory itself is present across replicas. if it is, then we will add it to the list
    // of paths to check.
    let mut sd_present_in_all_replicas = true;

    // search the contents of this directory, collecting a list of
    // all items across all replicas and storing it inside `current_entries`
    for root in config.roots.iter() {
        let absolute_directory = root.join(directory);
        if absolute_directory.is_dir() {
            // loop through dir
            for item in fs::read_dir(absolute_directory)? {
                let relative_path = item?.path();
                let relative_path = relative_path.strip_prefix(root).unwrap_or_else(|_| {
                    panic!("couldn't strip prefix {:?} from {:?}", root, relative_path)
                });

                if is_ignored(&config.ignore, &relative_path) {
                    info!("Ignoring entry {:?}", relative_path);
                    continue;
                }

                trace!("Adding entry {:?}", relative_path);

                // insert current filesystem state
                current_entries
                    .entry(relative_path.to_path_buf())
                    .or_insert_with(|| {
                        ArchiveEntryPerReplica::from_roots::<N>(&config.roots, &relative_path)
                    });
            }
        } else {
            sd_present_in_all_replicas = false;
            info!("{:?} isn't a directory", absolute_directory);
        }
    }

    if !sd_present_in_all_replicas {
        current_entries
            .entry(directory.to_path_buf())
            .or_insert_with(|| ArchiveEntryPerReplica::from_roots::<N>(&config.roots, &directory));
    }

    Ok(())
}
