//! Content-addressable hashing via blake3.

use std::path::Path;

/// Hash file contents with blake3.
pub fn blake3_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let data = std::fs::read(path)?;
    Ok(blake3_bytes(&data))
}

/// Hash a byte slice with blake3.
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
}
