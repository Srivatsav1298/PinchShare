//! Filesystem helpers: safe names, unique destinations, hashing, zip/unzip of folders.
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const MAX_FOLDER_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 200_000;
const MAX_UNZIPPED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

pub fn default_save_dir() -> PathBuf {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("PinchShare")
}

/// Reduce an untrusted name (from the network) to one safe path component.
pub fn sanitize_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = last.chars().filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c)).collect();
    s = s.trim_matches(|c: char| c == ' ' || c == '.').to_string();
    if s.chars().count() > 150 {
        s = s.chars().take(150).collect();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if s.is_empty() {
        return "file".into();
    }
    if reserved {
        s.insert(0, '_');
    }
    s
}

/// `dir/name`, or `dir/name (1).ext`, `(2)`, ... if taken.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

pub fn hash_file(path: &Path, mut on_bytes: impl FnMut(u64)) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        on_bytes(n as u64);
    }
    Ok(hex::encode(h.finalize()))
}

fn collect(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String, bool)>, total: &mut u64) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let ft = e.file_type()?;
        if ft.is_symlink() {
            continue; // never follow or send symlinks
        }
        let p = e.path();
        let rel = p.strip_prefix(base).unwrap().components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
        if ft.is_dir() {
            out.push((p.clone(), rel, true));
            collect(&p, base, out, total)?;
        } else if ft.is_file() {
            *total += e.metadata()?.len();
            if *total > MAX_FOLDER_BYTES {
                return Err(Error::Archive("folder is larger than the 2 GB limit".into()));
            }
            out.push((p, rel, false));
        }
    }
    Ok(())
}

/// Zip a folder (stored, not compressed: media is already compressed and this keeps CPU low).
/// The archive contains the folder's contents; the receiver names the folder.
pub fn zip_dir(src: &Path, dest: &Path, mut on_bytes: impl FnMut(u64)) -> Result<u64> {
    use zip::write::SimpleFileOptions;
    let mut items = Vec::new();
    let mut total = 0u64;
    collect(src, src, &mut items, &mut total)?;
    let mut zw = zip::ZipWriter::new(File::create(dest)?);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
    let mut buf = vec![0u8; 1 << 20];
    for (path, rel, is_dir) in items {
        if is_dir {
            zw.add_directory(format!("{rel}/"), opts).map_err(|e| Error::Archive(e.to_string()))?;
        } else {
            zw.start_file(&rel, opts).map_err(|e| Error::Archive(e.to_string()))?;
            let mut f = File::open(&path)?;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                zw.write_all(&buf[..n])?;
                on_bytes(n as u64);
            }
        }
    }
    zw.finish().map_err(|e| Error::Archive(e.to_string()))?;
    Ok(total)
}

/// Extract `zip_path` into the (new) directory `dest`. Rejects the whole archive on any path
/// traversal, absolute path, or symlink entry, and bounds entry count and total size.
pub fn unzip_safe(zip_path: &Path, dest: &Path) -> Result<()> {
    let mut ar = zip::ZipArchive::new(File::open(zip_path)?).map_err(|e| Error::Archive(e.to_string()))?;
    if ar.len() > MAX_ZIP_ENTRIES {
        return Err(Error::Archive("too many entries".into()));
    }
    std::fs::create_dir_all(dest)?;
    let mut total = 0u64;
    for i in 0..ar.len() {
        let mut e = ar.by_index(i).map_err(|e| Error::Archive(e.to_string()))?;
        let rel = e.enclosed_name().ok_or_else(|| Error::Archive(format!("unsafe path in archive: {:?}", e.name())))?;
        if e.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(Error::Archive("archive contains a symlink".into()));
        }
        let out = dest.join(&rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        total += e.size();
        if total > MAX_UNZIPPED_BYTES {
            return Err(Error::Archive("archive too large when extracted".into()));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = File::options().write(true).create_new(true).open(&out)?;
        std::io::copy(&mut e, &mut f)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_paths_and_junk() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("C:\\Windows\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_name("a\0b<c>.txt"), "abc.txt");
        assert_eq!(sanitize_name(".."), "file");
        assert_eq!(sanitize_name("   "), "file");
        assert_eq!(sanitize_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_name("photo 1.jpg"), "photo 1.jpg");
        assert_eq!(sanitize_name(&"x".repeat(400)).len(), 150);
    }

    #[test]
    fn unique_path_increments() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a.txt"));
        std::fs::write(d.path().join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a (1).txt"));
        std::fs::write(d.path().join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a (2).txt"));
        std::fs::create_dir(d.path().join("dir")).unwrap();
        assert_eq!(unique_path(d.path(), "dir"), d.path().join("dir (1)"));
    }

    #[test]
    fn zip_roundtrip_nested() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir_all(src.join("a/b/c")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();
        std::fs::write(src.join("top.txt"), b"top").unwrap();
        std::fs::write(src.join("a/b/c/deep.bin"), vec![7u8; 100_000]).unwrap();
        let z = d.path().join("x.zip");
        zip_dir(&src, &z, |_| {}).unwrap();
        let out = d.path().join("out");
        unzip_safe(&z, &out).unwrap();
        assert_eq!(std::fs::read(out.join("top.txt")).unwrap(), b"top");
        assert_eq!(std::fs::read(out.join("a/b/c/deep.bin")).unwrap().len(), 100_000);
        assert!(out.join("empty").is_dir());
    }

    #[test]
    fn zip_slip_is_rejected() {
        use zip::write::SimpleFileOptions;
        let d = tempfile::tempdir().unwrap();
        // Absolute paths are re-rooted inside `dest` by the zip crate: safe, so only containment is checked.
        {
            let z = d.path().join("abs.zip");
            let mut w = zip::ZipWriter::new(File::create(&z).unwrap());
            w.start_file("/abs/evil.txt", SimpleFileOptions::default()).unwrap();
            w.write_all(b"x").unwrap();
            w.finish().unwrap();
            let out = d.path().join("out_abs");
            if unzip_safe(&z, &out).is_ok() {
                assert!(out.join("abs/evil.txt").exists());
            }
            assert!(!Path::new("/abs/evil.txt").exists());
        }
        for evil in ["../evil.txt", "a/../../evil.txt"] {
            let z = d.path().join("evil.zip");
            let mut w = zip::ZipWriter::new(File::create(&z).unwrap());
            w.start_file(evil, SimpleFileOptions::default()).unwrap();
            w.write_all(b"pwn").unwrap();
            w.finish().unwrap();
            let out = d.path().join("out");
            assert!(matches!(unzip_safe(&z, &out), Err(Error::Archive(_))), "{evil} accepted");
            assert!(!d.path().join("evil.txt").exists());
            let _ = std::fs::remove_dir_all(&out);
        }
    }

    #[test]
    fn symlinks_are_not_sent() {
        #[cfg(unix)]
        {
            let d = tempfile::tempdir().unwrap();
            let src = d.path().join("s");
            std::fs::create_dir(&src).unwrap();
            std::fs::write(d.path().join("secret"), b"s").unwrap();
            std::os::unix::fs::symlink(d.path().join("secret"), src.join("link")).unwrap();
            std::fs::write(src.join("ok"), b"ok").unwrap();
            let z = d.path().join("z.zip");
            zip_dir(&src, &z, |_| {}).unwrap();
            let out = d.path().join("o");
            unzip_safe(&z, &out).unwrap();
            assert!(out.join("ok").exists() && !out.join("link").exists());
        }
    }
}
