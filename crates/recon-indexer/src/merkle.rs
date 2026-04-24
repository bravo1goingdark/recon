//! Merkle tree for efficient change detection across repo snapshots.
//!
//! A flat snapshot: relative path → (blake3 content hash, mtime).
//! Directory hashes are NOT computed — this is a flat index, not a hierarchical tree.
//! Diffing two snapshots identifies changed paths without re-hashing unchanged files.
//! Mtime pre-filtering avoids reading file content when mtime hasn't changed.

use recon_core::error::Error;
use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// Entry in a Merkle snapshot: content hash + file mtime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapshotEntry {
    /// Blake3 content hash (32 bytes).
    pub hash: [u8; 32],
    /// File modification time as Unix epoch seconds.
    pub mtime: i64,
}

/// A flat snapshot: relative path → (content hash, mtime).
#[derive(Debug, Clone, Default)]
pub struct MerkleSnapshot {
    /// Map of relative file path → snapshot entry (hash + mtime).
    pub entries: BTreeMap<PathBuf, SnapshotEntry>,
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
    /// Build a snapshot from a list of `(relative_path, content_hash, mtime)` triples.
    pub fn build(file_entries: Vec<(PathBuf, [u8; 32], i64)>) -> Self {
        let mut entries = BTreeMap::new();
        for (path, hash, mtime) in file_entries {
            entries.insert(path, SnapshotEntry { hash, mtime });
        }
        Self { entries }
    }

    /// Diff this snapshot against a previous one.
    ///
    /// Returns paths that changed (new or modified) and paths that were deleted.
    pub fn diff(&self, previous: &MerkleSnapshot) -> MerkleDiff {
        let mut changed = Vec::new();
        let mut deleted = Vec::new();

        // Find changed or new paths
        for (path, entry) in &self.entries {
            match previous.entries.get(path) {
                Some(prev) if prev.hash == entry.hash => {} // unchanged
                _ => changed.push(path.clone()),            // new or modified
            }
        }

        // Find deleted paths
        for path in previous.entries.keys() {
            if !self.entries.contains_key(path) {
                deleted.push(path.clone());
            }
        }

        MerkleDiff { changed, deleted }
    }

    /// Check if a file is unchanged compared to this snapshot.
    /// Returns true if the path exists in the snapshot with the same mtime.
    /// This is a fast pre-check before reading/hashing file content.
    pub fn is_unchanged(&self, path: &Path, mtime: i64) -> bool {
        self.entries
            .get(path)
            .is_some_and(|entry| entry.mtime == mtime)
    }

    /// Get the stored hash for a path, if present.
    pub fn get_hash(&self, path: &Path) -> Option<[u8; 32]> {
        self.entries.get(path).map(|e| e.hash)
    }

    /// Save snapshot to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let serializable: BTreeMap<String, (String, i64)> = self
            .entries
            .iter()
            .map(|(p, e)| {
                (
                    p.to_string_lossy().to_string(),
                    (hex::encode(&e.hash), e.mtime),
                )
            })
            .collect();
        let json = serde_json::to_string(&serializable)
            .map_err(|e| Error::Storage(format!("serialize merkle: {e}")))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load snapshot from a JSON file.
    ///
    /// Streams the file through a BufReader → serde_json parser. On a
    /// 50K-file repo the snapshot can reach 7–10 MB; `read_to_string`
    /// would allocate the whole buffer AND run UTF-8 validation over it
    /// before parsing could start. CLAUDE.md: no `read_to_string` on
    /// potentially large files.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let serializable: BTreeMap<String, (String, i64)> = serde_json::from_reader(reader)
            .map_err(|e| Error::Storage(format!("deserialize merkle: {e}")))?;

        let mut entries = BTreeMap::new();
        for (p, (h, mtime)) in serializable {
            let bytes = hex::decode(&h).map_err(|e| Error::Storage(format!("decode hash: {e}")))?;
            if bytes.len() != 32 {
                return Err(Error::Storage(format!(
                    "invalid hash length for {p}: {}",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            entries.insert(PathBuf::from(p), SnapshotEntry { hash: arr, mtime });
        }

        Ok(Self { entries })
    }

    /// Number of entries in the snapshot.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the snapshot is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
            (PathBuf::from("src/main.rs"), [1u8; 32], 1000i64),
            (PathBuf::from("src/lib.rs"), [2u8; 32], 1000i64),
        ];
        let s1 = MerkleSnapshot::build(files.clone());
        let s2 = MerkleSnapshot::build(files);
        let diff = s2.diff(&s1);
        assert!(diff.changed.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn diff_detects_new_file() {
        let s1 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [1u8; 32], 1000i64)]);
        let s2 = MerkleSnapshot::build(vec![
            (PathBuf::from("a.rs"), [1u8; 32], 1000i64),
            (PathBuf::from("b.rs"), [2u8; 32], 1000i64),
        ]);
        let diff = s2.diff(&s1);
        assert_eq!(diff.changed, vec![PathBuf::from("b.rs")]);
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn diff_detects_modified_file() {
        let s1 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [1u8; 32], 1000i64)]);
        let s2 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [9u8; 32], 1001i64)]);
        let diff = s2.diff(&s1);
        assert_eq!(diff.changed, vec![PathBuf::from("a.rs")]);
    }

    #[test]
    fn diff_detects_deleted_file() {
        let s1 = MerkleSnapshot::build(vec![
            (PathBuf::from("a.rs"), [1u8; 32], 1000i64),
            (PathBuf::from("b.rs"), [2u8; 32], 1000i64),
        ]);
        let s2 = MerkleSnapshot::build(vec![(PathBuf::from("a.rs"), [1u8; 32], 1000i64)]);
        let diff = s2.diff(&s1);
        assert!(diff.changed.is_empty());
        assert_eq!(diff.deleted, vec![PathBuf::from("b.rs")]);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap_path = dir.path().join("snapshot.json");

        let s1 = MerkleSnapshot::build(vec![
            (PathBuf::from("src/main.rs"), [0xAB; 32], 12345i64),
            (PathBuf::from("src/lib.rs"), [0xCD; 32], 12346i64),
        ]);
        s1.save(&snap_path).unwrap();

        let s2 = MerkleSnapshot::load(&snap_path).unwrap();
        assert_eq!(
            s1.entries.get(&PathBuf::from("src/main.rs")),
            s2.entries.get(&PathBuf::from("src/main.rs"))
        );
        assert_eq!(
            s1.entries.get(&PathBuf::from("src/lib.rs")),
            s2.entries.get(&PathBuf::from("src/lib.rs"))
        );
    }

    #[test]
    fn empty_snapshot() {
        let s = MerkleSnapshot::default();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn is_unchanged_checks_mtime() {
        let s = MerkleSnapshot::build(vec![
            (PathBuf::from("a.rs"), [1u8; 32], 1000i64),
            (PathBuf::from("b.rs"), [2u8; 32], 2000i64),
        ]);
        assert!(s.is_unchanged(&PathBuf::from("a.rs"), 1000));
        assert!(!s.is_unchanged(&PathBuf::from("a.rs"), 9999));
        assert!(!s.is_unchanged(&PathBuf::from("c.rs"), 1000));
    }
}
