use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::os::linux::fs::MetadataExt as LinuxMetadataExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::vec::Vec;

/// buffer size for the digest computation
const BUFFER_SIZE: usize = 1024 * 1024;

/// compute the digest of a file
pub fn file_digest(path: &PathBuf) -> io::Result<sha1::Digest> {
    let mut f = File::open(path)?;
    let mut buffer = [0; BUFFER_SIZE];
    let mut m = sha1::Sha1::new();
    loop {
        let size = f.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        m.update(&buffer[0..size]);
    }
    Ok(m.digest())
}

// /// print the file digests
// pub fn print_digests(paths: &[PathBuf]) -> io::Result<()> {
//     for path in paths {
//         let sha1 = file_digest(&path)?;
//         println!("{}  {}", sha1, path.display());
//     }
//     println!("{:?}", find_file_duplicates(paths));
//     Ok(())
// }

/// find the duplicates in the provided paths
pub fn find_file_duplicates(paths: &[PathBuf]) -> io::Result<Vec<Vec<PathBuf>>> {
    // compute a map of the digests to the path with that digest
    let mut file_map = HashMap::new();
    let mut ino_map = HashMap::new();
    for path in paths {
        if fs::metadata(path)?.len() == 0 {
            // don't hardlink empty files
            continue;
        }
        let inode = inos(path)?;
        // let digest = ino_map.get(&inode).unwrap_or_else(|| file_digest(&path)?);
        let digest = match ino_map.get(&inode) {
            Some(v) => *v,
            None => file_digest(&path)?,
        };
        file_map
            .entry(digest)
            .or_insert_with(Vec::new)
            .push(path.clone());
        ino_map.insert(inode, digest);
    }
    // then just keep the path with duplicates
    let mut res = Vec::<Vec<PathBuf>>::new();
    for (_, v) in file_map {
        if v.len() >= 2 {
            res.push(v);
        }
    }
    Ok(res)
}

/// find the duplicated files and replace them with hardlinks
pub fn hardlink_deduplicate(paths: &[PathBuf]) -> io::Result<()> {
    let dups = find_file_duplicates(paths)?;
    for dup in dups {
        file_hardlinks(&dup[0], &dup[1..])?;
    }
    Ok(())
}

pub fn file_hardlinks(path: &PathBuf, hardlinks: &[PathBuf]) -> io::Result<()> {
    let inode = inos(path)?;
    for hardlink in hardlinks {
        let hinode = inos(hardlink)?;
        if hinode != inode && hinode.0 == inode.0 {
            info!("{} -> {}", hardlink.display(), path.display());
            std::fs::remove_file(hardlink)?;
            std::fs::hard_link(path, hardlink)?;
        }
    }
    Ok(())
}

pub fn glob_to_files(paths: &Vec<String>) -> Result<Vec<PathBuf>, glob::PatternError> {
    Ok(paths
        .into_iter()
        .flat_map(|g| glob::glob(g).unwrap().into_iter().filter_map(|f| f.ok()))
        .map(|f| f.to_path_buf())
        .filter(|f| f.metadata().unwrap().file_type().is_file())
        .collect())
}

/// returns the inodes of the partition and of the file
pub fn inos(path: &PathBuf) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.st_dev(), metadata.ino()))
}
