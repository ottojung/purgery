use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

// ── Transform Types ────────────────────────────────────────────────

/// Server-side transform definition.
///
/// Each instance represents a named transform that clients can request. Transform
/// definitions are deserialised from `[[transform]]` array-of-tables in server config.
/// Duplicate `name` values are rejected during config validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformKind {
    Subprocess,
}

impl<'de> Deserialize<'de> for TransformKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "subprocess" => Ok(TransformKind::Subprocess),
            other => Err(serde::de::Error::custom(format!(
                "unknown transform kind: {other}"
            ))),
        }
    }
}

impl Serialize for TransformKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            TransformKind::Subprocess => "subprocess",
        };
        s.serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformDefinition {
    /// Unique name for this transform, used as the key for client requests.
    pub name: String,
    pub kind: TransformKind,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub expected_outputs: Vec<String>,
}

impl TransformDefinition {
    pub fn resolve_placeholders(&self, work_path: &Utf8Path, s: &str) -> String {
        let input = work_path.as_str();
        let parent = work_path.parent().map(|p| p.as_str()).unwrap_or("");
        let file_name = work_path.file_name().unwrap_or("");
        let file_stem = work_path.file_stem().unwrap_or("");
        s.replace("{input}", input)
            .replace("{parent}", parent)
            .replace("{file_name}", file_name)
            .replace("{file_stem}", file_stem)
    }

    pub fn build_args(&self, work_path: &Utf8Path, target_directory: &Utf8Path) -> Vec<String> {
        self.args
            .iter()
            .map(|a| {
                self.resolve_placeholders(work_path, a)
                    .replace("{target_directory}", target_directory.as_str())
            })
            .collect()
    }

    pub fn resolve_expected_outputs(
        &self,
        work_path: &Utf8Path,
        destination_root: &Utf8Path,
        target_directory: &Utf8Path,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        let mut results = Vec::with_capacity(self.expected_outputs.len());
        for pat in &self.expected_outputs {
            validate_expected_output_name(pat)?;
            let expanded = self
                .resolve_placeholders(work_path, pat)
                .replace("{target_directory}", target_directory.as_str());
            let path = if Utf8Path::new(&expanded).is_absolute() {
                Utf8PathBuf::from(expanded)
            } else {
                destination_root.join(&expanded)
            };
            results.push(path);
        }
        Ok(results)
    }
}

/// Validate a single transform definition's `program` and `expected_outputs`.
///
/// Does not check whether the program binary exists on disk — that is a
/// separate concern handled by `server_check`.
pub fn validate_transform_definition(def: &TransformDefinition) -> Result<(), String> {
    if def.program.is_empty() {
        return Err("program is empty".into());
    }
    for output in &def.expected_outputs {
        validate_expected_output_name(output)
            .map_err(|e| format!("expected_output {output:?}: {e}"))?;
    }
    Ok(())
}

pub fn validate_expected_output_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("expected output name is empty".into());
    }
    if name == "." || name == ".." {
        return Err(format!("expected output name must not be '{name}'"));
    }
    if name.split('/').any(|c| c == "..") || name.split('\\').any(|c| c == "..") {
        return Err("expected output path must not contain '..' components".into());
    }
    if name.contains("{input}") || name.contains("{parent}") {
        return Err("expected output name must not use {{input}} or {{parent}} \
             placeholders; only {{file_name}}, {{file_stem}}, and \
             {{target_directory}} are allowed"
            .into());
    }
    Ok(())
}
