//! A tiny, deterministic archive of an agent crate folder, embedded into a built agent binary.
//!
//! `huggr build` [`pack`]s the agent's source files into a
//! single blob written next to the generated shim crate and `include_bytes!`d
//! into the binary. At startup the binary [`unpack`]s that blob into a stable
//! per-agent home directory, so a shipped artifact carries its whole definition
//! (manifest + prompt + tool data) and needs no repo checkout to run.
//!
//! The format is intentionally trivial (no compression, no external crate) and
//! **deterministic** (entries sorted by path), so a rebuild of the same folder
//! produces byte-identical output. Runtime-only directories (the trace store,
//! the scratchpad) are excluded at pack time via `exclude_top`, so re-unpacking
//! on every run never clobbers persisted traces.

use std::fs;
use std::io;
use std::path::{Component, Path};

/// Magic + format version prefixing every bundle.
const MAGIC: &[u8; 10] = b"HUGGRBNDL\x01";

/// One packed file: a forward-slash relative path and its bytes.
struct Entry {
    path: String,
    data: Vec<u8>,
}

/// Pack every regular file under `dir` into a single deterministic blob,
/// skipping any entry whose root-relative path starts with an entry of
/// `exclude` (component-wise, so `data/traces` excludes exactly that subtree
/// and `target` still excludes the whole top-level dir). Symlinks are skipped.
pub fn pack(dir: &Path, exclude: &[&str]) -> io::Result<Vec<u8>> {
    pack_with_files(dir, exclude, &[])
}

/// Pack a tree and replace or add the supplied in-memory files.
pub fn pack_with_files(
    dir: &Path,
    exclude: &[&str],
    files: &[(&str, &[u8])],
) -> io::Result<Vec<u8>> {
    let mut entries = Vec::new();
    collect(dir, dir, exclude, &mut entries)?;
    for (path, data) in files {
        let rel = sanitized_rel(path)?;
        let path = rel_to_forward_slash(&rel);
        entries.retain(|entry| entry.path != path);
        entries.push(Entry {
            path,
            data: data.to_vec(),
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    write_u32(&mut out, entries.len() as u32);
    for entry in &entries {
        let path_bytes = entry.path.as_bytes();
        write_u32(&mut out, path_bytes.len() as u32);
        out.extend_from_slice(path_bytes);
        write_u64(&mut out, entry.data.len() as u64);
        out.extend_from_slice(&entry.data);
    }
    Ok(out)
}

fn collect(root: &Path, dir: &Path, exclude: &[&str], out: &mut Vec<Entry>) -> io::Result<()> {
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let path = child.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if exclude.iter().any(|ex| rel.starts_with(Path::new(ex))) {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue; // never bundle symlinks (portability + escape safety)
        }
        if meta.is_dir() {
            collect(root, &path, exclude, out)?;
        } else if meta.is_file() {
            let rel_str = rel_to_forward_slash(rel);
            let data = fs::read(&path)?;
            out.push(Entry {
                path: rel_str,
                data,
            });
        }
    }
    Ok(())
}

/// Unpack a bundle into `dest`, creating parent directories as needed. Paths
/// are validated to be relative and free of `..` before any file is written
/// (defence in depth — `pack` only ever emits jailed relative paths).
pub fn unpack(bytes: &[u8], dest: &Path) -> io::Result<()> {
    let mut cursor = Reader::new(bytes)?;
    let count = cursor.read_u32()?;
    for _ in 0..count {
        let path = cursor.read_string()?;
        let data = cursor.read_blob()?;
        let rel = sanitized_rel(&path)?;
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &data)?;
    }
    Ok(())
}

/// Read a single file's bytes out of a bundle without unpacking it (used to
/// pull `huggr.toml` in memory to resolve the agent home before unpacking).
pub fn get(bytes: &[u8], path: &str) -> io::Result<Option<Vec<u8>>> {
    let mut cursor = Reader::new(bytes)?;
    let count = cursor.read_u32()?;
    for _ in 0..count {
        let entry_path = cursor.read_string()?;
        let data = cursor.read_blob()?;
        if entry_path == path {
            return Ok(Some(data));
        }
    }
    Ok(None)
}

fn rel_to_forward_slash(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn sanitized_rel(path: &str) -> io::Result<std::path::PathBuf> {
    let rel = Path::new(path);
    for comp in rel.components() {
        match comp {
            Component::Normal(_) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe bundle path: {path}"),
                ));
            }
        }
    }
    Ok(rel.to_path_buf())
}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A minimal forward-only reader over a bundle blob.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> io::Result<Self> {
        if buf.len() < MAGIC.len() || &buf[..MAGIC.len()] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a huggr bundle (bad magic)",
            ));
        }
        Ok(Self {
            buf,
            pos: MAGIC.len(),
        })
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(truncated)?;
        if end > self.buf.len() {
            return Err(truncated());
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> io::Result<u64> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }

    fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?.to_vec();
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bundle path not utf-8"))
    }

    fn read_blob(&mut self) -> io::Result<Vec<u8>> {
        let len = self.read_u64()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "truncated huggr bundle")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, data: &[u8]) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    #[test]
    fn round_trips_a_tree_and_excludes_top_dirs() {
        let src = std::env::temp_dir().join(format!("huggr-bundle-src-{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        write(&src, "huggr.toml", b"[agent]\nname='x'\n");
        write(&src, "SYSTEM.md", b"prompt");
        write(&src, "docs/a.md", b"alpha");
        write(&src, "docs/sub/b.md", b"beta");
        // Runtime dirs that must be excluded.
        write(&src, "traces/t.json", b"{}");
        write(&src, "target/junk", b"x");
        // A nested state root: exclude exactly it, not its parent.
        write(&src, "data/traces/t.json", b"{}");
        write(&src, "data/input.csv", b"kept");

        let excludes = ["traces", "target", "data/traces"];
        let bytes = pack(&src, &excludes).unwrap();
        // Determinism: a second pack of the same tree is byte-identical.
        assert_eq!(bytes, pack(&src, &excludes).unwrap());

        let dest = std::env::temp_dir().join(format!("huggr-bundle-dst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        unpack(&bytes, &dest).unwrap();

        assert_eq!(
            fs::read(dest.join("huggr.toml")).unwrap(),
            b"[agent]\nname='x'\n"
        );
        assert_eq!(fs::read(dest.join("docs/a.md")).unwrap(), b"alpha");
        assert_eq!(fs::read(dest.join("docs/sub/b.md")).unwrap(), b"beta");
        assert!(!dest.join("traces").exists(), "excluded dir not packed");
        assert!(!dest.join("target").exists(), "excluded dir not packed");
        assert!(!dest.join("data/traces").exists(), "nested root excluded");
        assert_eq!(fs::read(dest.join("data/input.csv")).unwrap(), b"kept");

        // `get` pulls one file in memory.
        assert_eq!(
            get(&bytes, "SYSTEM.md").unwrap().as_deref(),
            Some(&b"prompt"[..])
        );
        assert_eq!(get(&bytes, "missing").unwrap(), None);

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(unpack(b"nope", std::env::temp_dir().as_path()).is_err());
        assert!(get(b"nope", "x").is_err());
    }
}
