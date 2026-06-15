use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

// ── Transform Types ────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformConfig {
    #[serde(default)]
    pub steps: std::collections::BTreeMap<String, TransformStepDefinition>,
}

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

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformStepDefinition {
    pub kind: TransformKind,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub expected_outputs: Vec<String>,
    #[serde(default = "default_true")]
    pub keep_original: bool,
}

impl TransformStepDefinition {
    pub fn resolve_placeholders(&self, work_path: &Utf8Path, s: &str) -> String {
        let input = work_path.as_str();
        let parent = work_path.parent().map(|p| p.as_str()).unwrap_or("");
        let file_name = work_path.file_name().unwrap_or("");
        let file_stem = work_path.file_stem().unwrap_or("");
        s.replace("{input}", input)
            .replace("{parent}", parent)
            .replace("{file_name}", file_name)
            .replace("{file_stem}", file_stem)
            .replace("{stem}", file_stem)
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
        target_directory: &Utf8Path,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        let mut results = Vec::with_capacity(self.expected_outputs.len());
        for pat in &self.expected_outputs {
            validate_expected_output_name(pat)?;
            let resolved = self.resolve_placeholders(work_path, pat);
            let p = Utf8Path::new(&resolved);
            let fname = p.file_name().unwrap_or(resolved.as_str());
            results.push(target_directory.join(fname));
        }
        Ok(results)
    }
}

pub fn validate_expected_output_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("expected output name is empty".into());
    }
    if name == "." || name == ".." {
        return Err(format!("expected output name must not be '{name}'"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err("expected output name must not contain path separators".into());
    }
    if Utf8Path::new(name).is_absolute() {
        return Err("expected output name must not be absolute".into());
    }
    if name.contains("{input}") || name.contains("{parent}") || name.contains("{target_directory}")
    {
        return Err(
            "expected output name must not use {{input}}, {{parent}}, or {{target_directory}} \
             placeholders; only {{file_name}}, {{file_stem}}, and {{stem}} are allowed"
                .into(),
        );
    }
    Ok(())
}
