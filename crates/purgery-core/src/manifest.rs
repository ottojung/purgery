use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::path::*;
use crate::IdentityVerificationError;
use crate::ManifestError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFileIdentity {
    pub local_path: Utf8PathBuf,
    pub size: u64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub mtime_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

pub(crate) fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

// ── Manifest Types ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub run_id: RunId,
    pub nickname: Nickname,
    #[serde(default)]
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub local_path: ClientLocalPath,
    pub staged_path: NormalizedRelativePath,
    pub relative_path: NormalizedRelativePath,
    #[serde(default)]
    pub kind: ManifestEntryKind,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub mtime_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<Utf8PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postprocess_steps: Vec<String>,
}

impl ManifestEntry {
    pub fn identity(&self) -> ManifestFileIdentity {
        ManifestFileIdentity {
            local_path: Utf8PathBuf::from(self.local_path.as_str()),
            size: self.size,
            mtime_ns: self.mtime_ns,
            sha256: self.sha256.clone(),
        }
    }

    pub fn verify_staged(&self, staged_path: &Utf8Path) -> Result<(), IdentityVerificationError> {
        let metadata = std::fs::metadata(staged_path.as_std_path()).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                IdentityVerificationError::NotFound(staged_path.to_owned())
            } else {
                IdentityVerificationError::Io(e)
            }
        })?;

        let actual_size = metadata.len();
        if actual_size != self.size {
            return Err(IdentityVerificationError::SizeMismatch {
                expected: self.size,
                actual: actual_size,
            });
        }

        if let Some(ref expected_sha) = self.sha256 {
            let actual_sha = compute_sha256(staged_path)?;
            if &actual_sha != expected_sha {
                return Err(IdentityVerificationError::Sha256Mismatch);
            }
        }

        Ok(())
    }
}

impl Manifest {
    pub fn from_toml(input: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = toml::from_str(input)?;
        if manifest.entries.is_empty() {
            return Err(ManifestError::NoEntries);
        }
        for entry in &manifest.entries {
            let invalid = match entry.kind {
                ManifestEntryKind::Directory => {
                    entry.size != 0
                        || entry.mtime_ns != 0
                        || entry.sha256.is_some()
                        || entry.link_target.is_some()
                }
                ManifestEntryKind::RegularFile => entry.link_target.is_some(),
                ManifestEntryKind::Symlink => {
                    entry.link_target.is_none()
                        || entry.size != 0
                        || entry.mtime_ns != 0
                        || entry.sha256.is_some()
                }
            };
            if invalid {
                return Err(ManifestError::InvalidEntry(format!(
                    "{} has fields incompatible with {:?}",
                    entry.relative_path.as_str(),
                    entry.kind
                )));
            }
        }
        Ok(manifest)
    }

    pub fn to_toml(&self) -> Result<String, ManifestError> {
        toml::to_string(self).map_err(|e| ManifestError::TomlSerialize(e.to_string()))
    }
}

pub fn compute_sha256(path: &Utf8Path) -> Result<String, std::io::Error> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path.as_std_path())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        use std::io::Read;
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
