use crate::ResolvedDestinationPlan;
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
    fn expand_placeholders(
        &self,
        work_path: &Utf8Path,
        target: Option<&ResolvedDestinationPlan>,
        legacy_target_directory: Option<&Utf8Path>,
        template: &str,
    ) -> String {
        let mut expanded = String::with_capacity(template.len());
        let mut remaining = template;
        while let Some(open) = remaining.find('{') {
            expanded.push_str(&remaining[..open]);
            let candidate = &remaining[open..];
            let Some(close) = candidate.find('}') else {
                expanded.push_str(candidate);
                return expanded;
            };
            let token = &candidate[..=close];
            let replacement = match token {
                "{input}" => Some(work_path.as_str()),
                "{parent}" => Some(work_path.parent().map(|p| p.as_str()).unwrap_or("")),
                "{file_name}" => Some(work_path.file_name().unwrap_or("")),
                "{file_stem}" => Some(work_path.file_stem().unwrap_or("")),
                "{target_path}" => target.map(|plan| plan.target_path.as_str()),
                "{target_directory}" => target
                    .map(|plan| plan.target_directory.as_str())
                    .or_else(|| legacy_target_directory.map(Utf8Path::as_str)),
                "{target_file_name}" => {
                    target.map(|plan| plan.target_path.file_name().unwrap_or(""))
                }
                "{target_file_stem}" => {
                    target.map(|plan| plan.target_path.file_stem().unwrap_or(""))
                }
                _ => None,
            };
            if let Some(value) = replacement {
                expanded.push_str(value);
            } else {
                expanded.push_str(token);
            }
            remaining = &candidate[close + 1..];
        }
        expanded.push_str(remaining);
        expanded
    }

    pub fn build_args_for_target(
        &self,
        work_path: &Utf8Path,
        target: &ResolvedDestinationPlan,
    ) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| self.expand_placeholders(work_path, Some(target), None, arg))
            .collect()
    }

    pub fn resolve_expected_outputs_for_target(
        &self,
        work_path: &Utf8Path,
        target: &ResolvedDestinationPlan,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        self.expected_outputs
            .iter()
            .map(|pattern| {
                validate_expected_output_name(pattern)?;
                let expanded = self.expand_placeholders(work_path, Some(target), None, pattern);
                Ok(if Utf8Path::new(&expanded).is_absolute() {
                    Utf8PathBuf::from(expanded)
                } else {
                    target.target_directory.join(expanded)
                })
            })
            .collect()
    }
    pub fn resolve_placeholders(&self, work_path: &Utf8Path, s: &str) -> String {
        self.expand_placeholders(work_path, None, None, s)
    }

    pub fn build_args(&self, work_path: &Utf8Path, target_directory: &Utf8Path) -> Vec<String> {
        self.args
            .iter()
            .map(|a| self.expand_placeholders(work_path, None, Some(target_directory), a))
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
            let expanded = self.expand_placeholders(work_path, None, Some(target_directory), pat);
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
             {{target_path}}, {{target_directory}}, {{target_file_name}}, and \
             {{target_file_stem}} are allowed"
            .into());
    }
    Ok(())
}
