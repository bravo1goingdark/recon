//! Merkle tree for efficient change detection across repo snapshots.
//!
//! A flat snapshot: relative path → blake3 content hash (files only).
//! Directory hashes are NOT computed — this is a flat index, not a hierarchical tree.
//! Diffing two snapshots identifies changed paths without re-hashing unchanged files.

use recon_core::error::Error;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A flat snapshot: relative path → blake3 content hash (files only).
#[derive(Debug, Clone, Default)]
pub struct MerkleSnapshot {
    /// Map of relative file path → blake3 hash (32 bytes).
    pub hashes: BTreeMap<PathBuf, [u8; 32]>,
}

/// Diff result between two Merkle snapshots.
#[derive(Debug, Default)]
pub struct MerkleDiff {
    /// Paths that are new or have changed hashes.
    pub changed: Vec<PathBuf>,
    /// Paths that were removed.
    pub deleted: Vec<PathBuf>,
}

impl MerkleSnapshot {
    /// Build a snapshot from a list of `(relative_path, content_hash)` pairs.
    pub fn build(file_hashes: Vec<(PathBuf, [u8; 32])>) -> Self {
        let mut hashes = BTreeMap::new();
        for (path, hash) in file_hashes {
            hashes.insert(path, hash);
        }
        Self { hashes }
    }

    /// Diff this snapshot against a previous one.
    ///
    /// Returns paths that changed (new or modified) and paths that were deleted.
    pub fn diff(&self, previous: &MerkleSnapshot) -> MerkleDiff {
        let mut changed = Vec::new();
        let mut deleted = Vec::new();

        // Find changed or new paths
        for (path, hash) in &self.hashes {
            match previous.hashes.get(path) {
                Some(prev_hash) if prev_hash == hash => {} // unchanged
                _ => changed.push(path.clone()),           // new or modified
            }
        }

        // Find deleted paths
        for path in previous.hashes.keys() {
            if !self.hashes.contains_key(path) {
                deleted.push(path.clone());
            }
        }

        MerkleDiff { changed, deleted }
    }

    /// Save snapshot to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let serializable: BTreeMap<String, String> = self
            .hashes
            .iter()
            .map(|(p, h)| (p.to_string_lossy().to_string(), hex::encode(h)))
            .collect();
        let json = serde_json::to_string(&serializable)
            .map_err(|e| Error::Storage(format!("serialize merkle: {e}")))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load snapshot from a JSON file.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let json = std::fs::read_to_string(path)?;
        let serializable: BTreeMap<String, String> = serde_json::from_str(&json)
            .map_err(|e| Error::Storage(format!("deserialize merkle: {e}")))?;

        let mut hashes = BTreeMap::new();
        for (p, h) in serializable {
            let bytes = hex::decode(&h).map_err(|e| Error::Storage(format!("decode hash: {e}")))?;
            if bytes.len() != 32 {
                return Err(Error::Storage(format!(
                    "invalid hash length for {p}: {}",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            hashes.insert(PathBuf::from(p), arr);
        }

        Ok(Self { hashes })
    }

    /// Number of entries in the snapshot.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Whether the snapshot is empty.
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// Hex encoding/decoding helpers (inline to avoid adding a dep).
mod hex {
    /// Encode bytes as lowercase hex string.
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX_CHARS[(b >> 4) as usize]);
            s.push(HEX_CHARS[(b & 0xf) as usize]);
        }
        s
    }

    const HEX_CHARS: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];

    /// Decode hex string to bytes.
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        if !s.len().is_multiple_of(2) {
            return Err("odd length".into());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("hex decode: {e}")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_diff_identical() {
        let files = vec![
            (PathBuf::from("src/main.rs"), [1u8; 32]),
            (PathBuf::from("src/lib.rs"), [2u8; 32]),
        ];
        let s1 = MerkleSnapshot::build(files.clone());
        let s2 = MerkleSnapshot::build(files);
        let diff = s2.diff(&s1);
        assert!(diff.changed.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn diff_detects_new_file() {
        let s1 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [1u8; 32])]);
        let s2 = MerkleSnapshot::build(vec![
            (PathBuf::from("a.rs"), [1u8; 32]),
            (PathBuf::from("b.rs"), [2u8; 32]),
        ]);
        let diff = s2.diff(&s1);
        assert_eq!(diff.changed, vec![PathBuf::from("b.rs")]);
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn diff_detects_modified_file() {
        let s1 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [1u8; 32])]);
        let s2 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [9u8; 32])]);
        let diff = s2.diff(&s1);
        assert_eq!(diff.changed, vec![PathBuf::from("a.rs")]);
    }

    #[test]
    fn diff_detects_deleted_file() {
        let s1 = MerkleSnapshot::build(vec![
            (PathBuf::from("a.rs"), [1u8; 32]),
            (PathBuf::from("b.rs"), [2u8; 32]),
        ]);
        let s2 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [1u8; 32])]);
        let diff = s2.diff(&s1);
        assert!(diff.changed.is_empty());
        assert_eq!(diff.deleted, vec![PathBuf::from("b.rs")]);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap_path = dir.path().join("snapshot.json");

        let s1 = MerkleSnapshot::build(vec![
            (PathBuf::from("src/main.rs"), [0xAB; 32]),
            (PathBuf::from("src/lib.rs"), [0xCD; 32]),
        ]);
        s1.save(&snap_path).unwrap();

        let s2 = MerkleSnapshot::load(&snap_path).unwrap();
        assert_eq!(
            s1.hashes.get(&PathBuf::from("src/main.rs")),
            s2.hashes.get(&PathBuf::from("src/main.rs"))
        );
        assert_eq!(
            s1.hashes.get(&PathBuf::from("src/lib.rs")),
            s2.hashes.get(&PathBuf::from("src/lib.rs"))
        );
    }

    #[test]
    fn empty_snapshot() {
        let s = MerkleSnapshot::default();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }
}
