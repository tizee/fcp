use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use std::array;
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fmt::Display;
use std::fs::Metadata;
use std::io;
use std::ops::BitOr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process;

pub mod error;
pub mod filesystem;

use crate::error::{Error, Result};
use crate::filesystem::{self as fs, FileType};

pub fn graceful(message: impl Display) -> ! {
    println!("{}", message);
    process::exit(0);
}

pub fn fatal(message: impl Display) -> ! {
    eprintln!("{}", message);
    process::exit(1);
}

// The boolean returned signifies whether an error occurred (`true`) or not (`false`). The purpose
// of returning just a boolean instead of the underlying error itself is that we want to display
// the error to the user as soon as it occurs (as this makes for a better user-experience during
// long-running jobs) as opposed to propagating it upwards and printing all errors at the end.
// However, at the end of the process we still need to know whether or not an error occurred at any
// point in order to set the exit code appropriately.
fn copy_file(source: &Path, source_type: Result<FileType>, dest: &Path) -> bool {
    fn __copy_file(source: &Path, source_type: Result<FileType>, dest: &Path) -> Result<bool> {
        match source_type? {
            FileType::Regular => {
                fs::copy(source, dest)?;
            }
            FileType::Directory => return copy_directory(source, dest),
            FileType::Symlink => fs::symlink(fs::read_link(source)?, dest)?,
            FileType::Fifo => fs::mkfifo(dest, fs::symlink_metadata(source)?.permissions())?,
            FileType::Socket => {
                return Err(Error::new(format!(
                    "{}: sockets cannot be copied",
                    source.display(),
                )));
            }
            FileType::CharacterDevice | FileType::BlockDevice => {
                let metadata = fs::symlink_metadata(source)?;
                let mut source = fs::open(source)?;
                let mut dest = fs::create(dest, metadata.permissions().mode())?;
                io::copy(&mut source, &mut dest)?;
            }
        }
        Ok(false)
    }

    __copy_file(source, source_type, dest).unwrap_or_else(|err| {
        eprintln!("{}", err);
        true
    })
}

fn copy_directory(source: &Path, dest: &Path) -> Result<bool> {
    fs::create_dir(dest, fs::symlink_metadata(source)?.permissions().mode())?;
    let (mut entries, mut has_err) = (Vec::new(), false);
    for entry in fs::read_dir(source)? {
        match entry {
            Ok(entry) => entries.push((entry.file_name(), fs::entry_file_type(&entry))),
            Err(err) => {
                eprintln!("{}", err);
                has_err = true;
            }
        }
    }
    entries.shrink_to_fit();
    Ok(entries
        .into_par_iter()
        .map(|(file_name, file_type)| {
            copy_file(&source.join(&file_name), file_type, &dest.join(&file_name))
        })
        .reduce(|| has_err, BitOr::bitor))
}

fn reject_self_copies(sources: &[PathBuf], dest: &Path) -> Result<()> {
    let current_dir = env::current_dir()?;
    let mut prefix = Path::new("");
    // We make `dest` absolute because for relative paths the final non-`None` value returned by
    // `Path::ancestors` is always `Some("")`, which is problematic because:
    // 1. It would cause spurious errors, as trying to query the metadata of an empty path
    //    trivially results in an error due to no file corresponding to the given path.
    // 2. In some instances self-copies would not be prevented, as we wouldn't check all of our
    //    ancestors, just the ones up to the current directory.
    let dest = if dest.is_relative() {
        prefix = current_dir.as_path();
        current_dir.join(dest)
    } else {
        dest.to_path_buf()
    };

    // Combine device number with inode number to uniquely identify a file,
    // since the same inode number could be used in a different filesystem.
    fn unique_id(meta: Metadata) -> (u64, u64) {
        (meta.dev(), meta.ino())
    }

    // We use `fs::metadata` for `ancestor_ids` since we do the exact same thing regardless of
    // whether `dest` is a directory or a symlink pointing to one.
    let ancestor_ids = dest
        .ancestors()
        .map(|ancestor| fs::metadata(ancestor).map(unique_id));

    // In contrast, we use `fs::symlink_metadata` for `source_ids` because we copy the symlinks
    // themselves, not the underlying files that they point to.
    let source_ids = sources
        .iter()
        .map(|source| fs::symlink_metadata(source).map(unique_id))
        .collect::<Box<_>>();

    let mut errors = Vec::new();

    for (ancestor, id) in dest.ancestors().zip(ancestor_ids) {
        let id = id?;
        for (source, source_id) in sources.iter().zip(source_ids.as_ref()) {
            match source_id {
                Ok(source_id) if *source_id == id => errors.push(format!(
                    "Cannot copy directory '{}' into itself '{}'",
                    source.display(),
                    ancestor.strip_prefix(prefix).unwrap_or(ancestor).display()
                )),
                Err(err) => errors.push(err.to_string()),
                _ => {}
            }
        }
    }

    if !errors.is_empty() {
        Err(Error::new(errors.join("\n")))
    } else {
        Ok(())
    }
}

fn file_names(sources: &[PathBuf]) -> Result<Vec<&OsStr>> {
    let source_file_names = sources
        .iter()
        .map(|source| {
            source.file_name().ok_or_else(|| {
                Error::new(format!(
                    "{}: path does not end with a file name",
                    source.display()
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut sources_by_name: HashMap<_, Vec<_>> = HashMap::new();
    for (source, file_name) in sources.iter().zip(&source_file_names) {
        sources_by_name.entry(file_name).or_default().push(source);
    }
    let errors = sources_by_name
        .values()
        .filter_map(|source_group| {
                (source_group.len() > 1).then(|| {
                format!(
                "{}: paths have the same file name and thus would be copied to the same destination",
                source_group
                    .iter()
                    .map(|source| format!("{}", source.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            })
        })
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        Err(Error::new(errors.join("\n")))
    } else {
        Ok(source_file_names)
    }
}

/// Copy each file in `sources` into the directory `dest`.
fn copy_into(sources: &[PathBuf], dest: &Path) -> bool {
    if let Some(err) = match fs::metadata(dest) {
        Err(err) => Some(err),
        Ok(metadata) if !metadata.is_dir() => {
            Some(Error::new(format!("{} is not a directory", dest.display())))
        }
        _ => reject_self_copies(sources, dest).err(),
    } {
        fatal(err)
    }

    sources
        .iter()
        .zip(file_names(sources).unwrap_or_else(|err| fatal(err)))
        .collect::<Box<_>>()
        .into_par_iter()
        .map(|(source, file_name)| copy_file(source, fs::file_type(source), &dest.join(file_name)))
        .reduce(|| false, BitOr::bitor)
}

// The `allow` here is present because clippy doesn't realize that `source` must be of
// type `&PathBuf` in order for the call to `array::from_ref` to typecheck.
#[allow(clippy::ptr_arg)]
fn copy_single(source: &PathBuf, dest: &Path) -> bool {
    let source_metadata = fs::symlink_metadata(source).unwrap_or_else(|err| fatal(err));
    match (fs::metadata(dest), fs::symlink_metadata(dest)) {
        (Ok(metadata), _) if metadata.is_dir() => copy_into(array::from_ref(source), dest),
        (_, Ok(metadata)) if source_metadata.ino() == metadata.ino() => fatal(format!(
            "Cannot overwrite file '{}' with itself '{}'",
            source.display(),
            dest.display()
        )),
        _ => copy_file(source, fs::file_type(source), dest),
    }
}

pub fn fcp(args: &[String]) -> bool {
    let args: Box<_> = args.iter().map(PathBuf::from).collect();
    match args.as_ref() {
        [] | [_] => fatal("Please provide at least two arguments (run 'fcp --help' for details)"),
        [source, dest] => copy_single(source, dest),
        [sources @ .., dest] => copy_into(sources, dest),
    }
}
