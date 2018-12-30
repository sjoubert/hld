use bincode;
use blake2_rfc::blake2b::Blake2b;
use custom_error::custom_error;
use fs2::FileExt;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::os::linux::fs::MetadataExt as LinuxMetadataExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::result;
use std::sync::Mutex;
use std::vec::Vec;

custom_error! {pub Error
    PathIo {
        source: io::Error,
        path: PathBuf
    } = @{format!("{}: {}", path.display(), source)},
    // no need for this one for now, and not having it ensures we get a compilation error
    // when an io::Error is not properly converted to Error::PathIo
    // Io {source: io::Error} = "{source}",
    Glob {source: glob::PatternError} = "{source}",
    Cache {source: bincode::Error} = "{source}",
}

/// Alias for a `Result` with the error type `hld::Error`.
pub type Result<T> = result::Result<T, Error>;

trait ToPathIOErr<T> {
    fn with_path(self: Self, path: &Path) -> Result<T>;
}

impl<T> ToPathIOErr<T> for io::Result<T> {
    fn with_path(self: Self, path: &Path) -> Result<T> {
        self.map_err(|e| Error::PathIo {
            source: e,
            path: path.to_path_buf(),
        })
    }
}

type Digest = [u8; 32];

/// compute the digest of a file
fn file_digest(path: &Path) -> Result<Digest> {
    debug!("computing digest of {}", path.display());
    let mut file = fs::File::open(&path).with_path(&path)?;
    let mut hasher = Blake2b::new(32);
    io::copy(&mut file, &mut hasher).with_path(&path)?;
    let mut hash: Digest = Default::default();
    hash.copy_from_slice(hasher.finalize().as_bytes());
    Ok(hash)
}

// /// print the file digests
// fn print_digests(paths: &[PathBuf]) -> Result<()> {
//     for path in paths {
//         let sha1 = file_digest(&path)?;
//         println!("{}  {}", sha1, path.display());
//     }
//     println!("{:?}", find_file_duplicates(paths));
//     Ok(())
// }

/// find the duplicates in the provided paths
fn find_file_duplicates<'a>(
    paths: &'a [PathBuf],
    caches: &[PathBuf],
    dry_run: bool,
    cache_path: &Path,
) -> Result<Vec<Vec<&'a PathBuf>>> {
    // compute a map of the digests to the path with that digest
    let ino_map = Mutex::new(HashMap::new());
    let cache = update_cache(caches, dry_run, cache_path)?;

    // get some metadata and filter out the empty files
    let mut path_inos: Vec<(&'a PathBuf, (u64, u64))> = Vec::new();
    for path in paths {
        let metadata = fs::metadata(path).with_path(path)?;
        if metadata.len() > 0 {
            path_inos.push((path, inos_m(&metadata)));
        }
    }

    // compute the digests
    let digests = path_inos
        .par_iter()
        .map(|(path, inode)| -> Result<(&'a PathBuf, Digest)> {
            let ino_digest: Option<Digest> = ino_map
                .lock()
                .unwrap()
                .get(inode)
                .map_or(None, |v| Some(*v));
            let digest = if let Some(digest) = ino_digest {
                digest
            } else {
                let digest = if let Some(digest) = cache.get(*path) {
                    *digest
                } else {
                    file_digest(path)?
                };
                ino_map.lock().unwrap().insert(*inode, digest);
                digest
            };
            Ok((path, digest))
        })
        .collect::<Result<Vec<(&'a PathBuf, Digest)>>>()?;

    // merge the digests in a hashmap
    let mut res = hashmap! {};
    for (path, digest) in digests {
        res.entry(digest).or_insert_with(Vec::new).push(path);
    }

    // then just keep the paths with duplicates
    Ok(res
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(_, v)| v)
        .collect())
}

fn update_cache(paths: &[PathBuf], dry_run: bool, cache_path: &Path) -> Result<HashMap<PathBuf, Digest>> {
    let cache: HashMap<PathBuf, Digest> = File::open(&cache_path).ok().map_or_else(
        || HashMap::new(),
        |reader| bincode::deserialize_from(reader).unwrap_or_default(),
    );
    let original_cache_size = cache.len();

    // remove dead entries
    let mut live_cache: HashMap<_, _> = cache
        .into_iter()
        .collect::<Vec<(_, _)>>()
        .par_iter()
        .cloned()
        .filter(|(path, _)| path.exists())
        .collect();
    let live_cache_size = live_cache.len();
    let updated = original_cache_size != live_cache_size;
    // compute the digest for the entries not already there
    let new_digests = paths
        .par_iter()
        .map(|path| {
            let digest = live_cache
                .get(path)
                .map_or_else(|| file_digest(path), |d| Ok(*d))?;
            Ok((path.clone(), digest))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    live_cache.extend(new_digests.clone());
    let updated = updated || live_cache_size != live_cache.len();

    if updated && !dry_run {
        debug!("saving updated cache");
        let output_file = File::create(&cache_path).with_path(&cache_path)?;
        output_file.lock_exclusive().with_path(&cache_path)?;
        bincode::serialize_into(&output_file, &live_cache)?;
        output_file.unlock().with_path(&cache_path)?;
    }

    Ok(new_digests)
}

/// find the duplicated files and replace them with hardlinks
pub fn hardlink_deduplicate(paths: &[PathBuf], caches: &[PathBuf], dry_run: bool, cache_path: &Path) -> Result<()> {
    let dups = find_file_duplicates(paths, caches, dry_run, cache_path)?;
    for dup in dups {
        file_hardlinks(&dup[0], &dup[1..], dry_run)?;
    }
    Ok(())
}

fn file_hardlinks(path: &Path, hardlinks: &[&PathBuf], dry_run: bool) -> Result<()> {
    let inode = inos(path)?;
    for hardlink in hardlinks {
        let hinode = inos(hardlink)?;
        if hinode != inode && hinode.0 == inode.0 {
            info!("{} -> {}", hardlink.display(), path.display());
            if !dry_run {
                std::fs::remove_file(hardlink).with_path(hardlink)?;
                std::fs::hard_link(path, hardlink).with_path(path)?;
            }
        }
    }
    Ok(())
}

pub fn glob_to_files(paths: &Vec<String>) -> Result<Vec<PathBuf>> {
    Ok(paths
        .into_iter()
        .flat_map(|g| glob::glob(g).unwrap().into_iter().filter_map(|f| f.ok()))
        .map(|f| f.to_path_buf())
        .filter(|f| f.metadata().unwrap().file_type().is_file())
        .collect())
}

/// returns the inodes of the partition and of the file
fn inos(path: &Path) -> Result<(u64, u64)> {
    Ok(inos_m(&fs::metadata(path).with_path(path)?))
}

fn inos_m(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.st_dev(), metadata.ino())
}
