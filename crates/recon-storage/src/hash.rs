//! Content-addressable hashing via blake3.

use std::io::Read;
use std::path::Path;

const MMAP_THRESHOLD: u64 = 65_536; // 64KB

/// Hash file contents with blake3 — streaming for efficiency.
pub fn blake3_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();

    if size > MMAP_THRESHOLD {
        // Memory-map large files
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(*blake3::hash(&mmap).as_bytes())
    } else {
        // Read small files directly — avoids mmap overhead
        let mut file = std::fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(*hasher.finalize().as_bytes())
    }
}

/// Hash a byte slice with blake3.
#[inline]
pub fn blake3_bytes(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_deterministic() {
        let a = blake3_bytes(b"hello world");
        let b = blake3_bytes(b"hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn hash_differs() {
        let a = blake3_bytes(b"hello");
        let b = blake3_bytes(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("test.txt");
        std::fs::write(&p, b"content").unwrap();
        let h = blake3_file(&p).unwrap();
        assert_eq!(h, blake3_bytes(b"content"));
    }

    #[test]
    fn hash_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("large.bin");
        let data = vec![0xABu8; 128_000]; // > MMAP_THRESHOLD
        std::fs::write(&p, &data).unwrap();
        let h = blake3_file(&p).unwrap();
        assert_eq!(h, blake3_bytes(&data));
    }
}
