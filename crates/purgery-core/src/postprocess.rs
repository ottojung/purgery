use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

// ── Postprocess Types ────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostprocessConfig {
    #[serde(default)]
    pub steps: std::collections::BTreeMap<String, PostprocessStepDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostprocessKind {
    Subprocess,
}

impl<'de> Deserialize<'de> for PostprocessKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "subprocess" => Ok(PostprocessKind::Subprocess),
            other => Err(serde::de::Error::custom(format!(
                "unknown postprocess kind: {other}"
            ))),
        }
    }
}

impl Serialize for PostprocessKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            PostprocessKind::Subprocess => "subprocess",
        };
        s.serialize(serializer)
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostprocessStepDefinition {
    pub kind: PostprocessKind,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    #[serde(default = "default_true")]
    pub keep_original: bool,
}

impl PostprocessStepDefinition {
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

    pub fn build_args(&self, work_path: &Utf8Path) -> Vec<String> {
        self.args
            .iter()
            .map(|a| self.resolve_placeholders(work_path, a))
            .collect()
    }

    pub fn resolve_expected_outputs(
        &self,
        work_path: &Utf8Path,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        let parent = work_path
            .parent()
            .map(|p| p.to_owned())
            .unwrap_or_else(|| Utf8PathBuf::from("."));
        let mut results = Vec::with_capacity(self.expected_outputs.len());
        for pat in &self.expected_outputs {
            validate_expected_output_name(pat)?;
            let resolved = self.resolve_placeholders(work_path, pat);
            let p = Utf8Path::new(&resolved);
            let fname = p.file_name().unwrap_or(resolved.as_str());
            results.push(parent.join(fname));
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
    if name.contains("{input}") || name.contains("{parent}") {
        return Err(
            "expected output name must not use {{input}} or {{parent}} placeholders; \
             only {{file_name}}, {{file_stem}}, and {{stem}} are allowed"
                .into(),
        );
    }
    Ok(())
}
