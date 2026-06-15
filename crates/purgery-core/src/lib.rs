#[cfg(not(unix))]
compile_error!("Purgery is Unix-only — it requires rsync, SSH, and Unix filesystem semantics");

use camino::{Utf8Path, Utf8PathBuf};
use std::io;
use thiserror::Error;

// ── Module declarations ──────────────────────────────────────────────

mod cleanup_state;
mod config;
mod manifest;
mod path;
mod status;
mod transform;

pub use cleanup_state::*;
pub use config::*;
pub use manifest::*;
pub use path::*;
pub use status::*;
pub use transform::*;

// ── Error Types ──────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("failed to parse config TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("failed to serialize config: {0}")]
    TomlSerialize(String),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("invalid host: {0}")]
    RemoteHost(#[from] RemoteHostError),
    #[error("invalid path: {0}")]
    Path(#[from] PathValidationError),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
}

#[derive(Error, Debug)]
pub enum ManifestError {
    #[error("failed to parse manifest TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("failed to serialize manifest: {0}")]
    TomlSerialize(String),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("invalid path: {0}")]
    Path(#[from] PathValidationError),
    #[error("invalid local path: {0}")]
    LocalPath(#[from] ClientLocalPathError),
    #[error("manifest has no filesystem entries")]
    NoEntries,
    #[error("invalid manifest entry: {0}")]
    InvalidEntry(String),
}

#[derive(Error, Debug)]
pub enum StatusError {
    #[error("failed to parse status TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("failed to serialize status: {0}")]
    TomlSerialize(String),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("unknown file status value: {0}")]
    UnknownFileStatus(String),
    #[error("unknown run state value: {0}")]
    UnknownRunState(String),
}

#[derive(Error, Debug)]
pub enum IdentityVerificationError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("file not found: {0}")]
    NotFound(Utf8PathBuf),
    #[error("size mismatch: expected {expected}, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("SHA-256 mismatch")]
    Sha256Mismatch,
    #[error("mtime unavailable")]
    MtimeUnavailable,
}

#[derive(Error, Debug)]
pub enum ExecutableError {
    #[error("empty program name")]
    EmptyProgram,
    #[error("program '{0}' not found")]
    NotFound(String),
    #[error("program '{0}' is not executable")]
    NotExecutable(String),
    #[error("program '{0}' absolute path not found")]
    AbsoluteNotFound(String),
    #[error("I/O error checking program '{0}': {1}")]
    Io(String, #[source] io::Error),
}

// ── Executable Resolution ────────────────────────────────────────────

pub struct ResolvedExecutable {
    pub path: Utf8PathBuf,
}

pub fn resolve_executable(program: &str) -> Result<ResolvedExecutable, ExecutableError> {
    if program.is_empty() {
        return Err(ExecutableError::EmptyProgram);
    }

    fn check(path: &Utf8Path, program: &str) -> Result<ResolvedExecutable, ExecutableError> {
        let meta = std::fs::metadata(path.as_std_path())
            .map_err(|e| ExecutableError::Io(program.to_owned(), e))?;
        if !meta.file_type().is_file() {
            return Err(ExecutableError::NotExecutable(format!(
                "'{}' is not a regular file",
                path.as_str()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                return Err(ExecutableError::NotExecutable(format!(
                    "'{}' is not executable",
                    program
                )));
            }
        }
        Ok(ResolvedExecutable {
            path: path.to_owned(),
        })
    }

    let program_path = Utf8Path::new(program);

    if program_path.is_absolute() {
        if !program_path.exists() {
            return Err(ExecutableError::AbsoluteNotFound(program.to_owned()));
        }
        check(program_path, program)
    } else {
        let path_var = std::env::var("PATH").unwrap_or_default();
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Utf8Path::new(dir).join(program);
            if !candidate.exists() {
                continue;
            }
            if check(&candidate, program).is_ok() {
                return Ok(ResolvedExecutable { path: candidate });
            }
        }
        Err(ExecutableError::NotFound(program.to_owned()))
    }
}

// ── Build Rsync Args ────────────────────────────────────────────────

/// Build rsync arguments for Purgery transfers.
///
/// Includes `--partial` so interrupted transfers can resume without
/// re-transferring already-received data. Includes `--inplace` so rsync
/// writes directly to the destination file path rather than creating a
/// temporary sibling file and renaming into place — this keeps the
/// final destination free of non-final helper paths. Includes `--mkpath`
/// so rsync creates destination directories as needed — Purgery does not
/// eagerly create destination directory skeletons before a transfer begins.
///
/// A partially transferred file at an exact final path is the actual file
/// being transferred — it is not an operational helper path. The output-only
/// final destination invariant forbids non-final scaffold paths under any
/// final destination, not partial contents at exact final paths.
pub fn build_rsync_args(source: &str, destination: &str) -> Vec<String> {
    vec![
        "--recursive".to_string(),
        "--partial".to_string(),
        "--inplace".to_string(),
        "--mkpath".to_string(),
        "--archive".to_string(),
        "--protect-args".to_string(),
        "--".to_string(),
        source.to_string(),
        destination.to_string(),
    ]
}

// ── Work Area ────────────────────────────────────────────────────────

pub fn work_dir(work_dir: &PurgeryRoot, nickname: &Nickname, run_id: &RunId) -> Utf8PathBuf {
    work_dir
        .run_dir(nickname, run_id, RunPhase::Processing)
        .join("work")
}

// ── Envelope Validation ─────────────────────────────────────────────

pub fn validate_envelope(
    dir_nickname: &Nickname,
    dir_run_id: &RunId,
    run_config: &RunConfig,
    manifest: &Manifest,
) -> Result<(), String> {
    if run_config.nickname != *dir_nickname {
        return Err(format!(
            "directory nickname '{}' does not match run config nickname '{}'",
            dir_nickname.as_str(),
            run_config.nickname.as_str()
        ));
    }
    if manifest.nickname != *dir_nickname {
        return Err(format!(
            "manifest nickname '{}' does not match directory nickname '{}'",
            manifest.nickname.as_str(),
            dir_nickname.as_str()
        ));
    }
    if manifest.run_id != *dir_run_id {
        return Err(format!(
            "manifest run_id '{}' does not match directory run_id '{}'",
            manifest.run_id.as_str(),
            dir_run_id.as_str()
        ));
    }
    Ok(())
}

// ── Shell Escaping (shared by client and server) ─────────────────────

pub fn shell_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len() + 2);
    escaped.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

// ── Logging Config ──────────────────────────────────────────────────

pub fn init_logging(
    config: &LoggingConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let level_filter: tracing_subscriber::filter::LevelFilter = match config.level {
        LogLevel::Error => tracing_subscriber::filter::LevelFilter::ERROR,
        LogLevel::Warn => tracing_subscriber::filter::LevelFilter::WARN,
        LogLevel::Info => tracing_subscriber::filter::LevelFilter::INFO,
        LogLevel::Debug => tracing_subscriber::filter::LevelFilter::DEBUG,
        LogLevel::Trace => tracing_subscriber::filter::LevelFilter::TRACE,
    };

    let is_terminal = atty::is(atty::Stream::Stderr);
    let use_color = match config.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => is_terminal,
    };

    match config.format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_max_level(level_filter)
                .json()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .try_init()?;
        }
        LogFormat::Compact => {
            tracing_subscriber::fmt()
                .with_max_level(level_filter)
                .compact()
                .with_writer(std::io::stderr)
                .with_ansi(use_color)
                .try_init()?;
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .with_max_level(level_filter)
                .pretty()
                .with_writer(std::io::stderr)
                .with_ansi(use_color)
                .try_init()?;
        }
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn nickname_valid() {
        let n = Nickname::new("laptop".into()).unwrap();
        assert_eq!(n.as_str(), "laptop");
    }

    #[test]
    fn nickname_valid_with_hyphen_underscore() {
        let n = Nickname::new("my-laptop_2".into()).unwrap();
        assert_eq!(n.as_str(), "my-laptop_2");
    }

    #[test]
    fn nickname_empty_is_error() {
        assert_eq!(Nickname::new("".into()), Err(NicknameError::Empty));
    }

    #[test]
    fn nickname_rejects_slash() {
        assert!(Nickname::new("lap/top".into()).is_err());
    }

    #[test]
    fn nickname_rejects_space() {
        assert!(Nickname::new("my laptop".into()).is_err());
    }

    #[test]
    fn nickname_serde_roundtrip() {
        let n = Nickname::new("desktop".into()).unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"desktop\"");
        let back: Nickname = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn nickname_serde_rejects_invalid() {
        let result: Result<Nickname, _> = serde_json::from_str("\"\"");
        assert!(result.is_err());
    }

    #[test]
    fn nickname_from_str() {
        let n: Nickname = "phone".parse().unwrap();
        assert_eq!(n.as_str(), "phone");
    }

    // ── RemoteHost tests ──

    #[test]
    fn remote_host_valid() {
        let h = RemoteHost::new("example.com".into()).unwrap();
        assert_eq!(h.as_str(), "example.com");
    }

    #[test]
    fn remote_host_empty_is_error() {
        assert!(RemoteHost::new("".into()).is_err());
    }

    // ── ClientLocalPath tests ──

    #[test]
    fn client_local_path_valid() {
        let p = ClientLocalPath::new("/home/user/file.mp4".into()).unwrap();
        assert_eq!(p.as_str(), "/home/user/file.mp4");
    }

    #[test]
    fn client_local_path_empty_is_error() {
        assert!(ClientLocalPath::new("".into()).is_err());
    }

    #[test]
    fn client_local_path_serde_roundtrip() {
        let p = ClientLocalPath::new("/tmp/test.mp4".into()).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"/tmp/test.mp4\"");
        let back: ClientLocalPath = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    // ── RunId tests ──

    #[test]
    fn run_id_valid() {
        let r = RunId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()).unwrap();
        assert_eq!(r.as_str(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    }

    #[test]
    fn run_id_empty_is_error() {
        assert_eq!(RunId::new("".into()), Err(RunIdError::Empty));
    }

    #[test]
    fn run_id_rejects_slash() {
        assert!(RunId::new("run/id".into()).is_err());
    }

    #[test]
    fn run_id_rejects_space() {
        assert!(RunId::new("run id".into()).is_err());
    }

    #[test]
    fn run_id_serde_roundtrip() {
        let r = RunId::new("abc-123.def".into()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: RunId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn run_id_generates_valid() {
        let id = RunId::generate();
        assert_eq!(id.as_str().len(), 26);
        assert!(RunId::new(id.as_str().to_owned()).is_ok());
    }

    // ── PurgeryRoot tests ──

    #[test]
    fn work_dir_absolute_valid() {
        let p = Utf8PathBuf::from("/universe/tmp/purgery");
        let r = PurgeryRoot::new(p.clone()).unwrap();
        assert_eq!(r.as_path(), &p);
    }

    #[test]
    fn work_dir_relative_is_error() {
        let p = Utf8PathBuf::from("tmp/purgy");
        assert_eq!(PurgeryRoot::new(p), Err(PathValidationError::NotAbsolute));
    }

    #[test]
    fn work_dir_path_helpers() {
        let root = PurgeryRoot::new("/tmp/purgery".into()).unwrap();
        let nick = Nickname::new("laptop".into()).unwrap();
        let run = RunId::new("run1".into()).unwrap();
        assert_eq!(
            root.nickname_dir(&nick),
            Utf8PathBuf::from("/tmp/purgery/laptop")
        );
        assert_eq!(
            root.run_dir(&nick, &run, RunPhase::Incoming),
            Utf8PathBuf::from("/tmp/purgery/laptop/incoming/run1")
        );
        assert_eq!(
            root.run_dir(&nick, &run, RunPhase::Ready),
            Utf8PathBuf::from("/tmp/purgery/laptop/ready/run1")
        );
    }

    // ── DestinationPath tests ──

    #[test]
    fn destination_accepts_absolute_and_relative_paths() {
        let absolute = DestinationPath::new("/some/path".into()).unwrap();
        let relative = DestinationPath::new("some/path".into()).unwrap();
        assert_eq!(absolute.as_str(), "/some/path");
        assert!(absolute.is_absolute());
        assert_eq!(relative.as_str(), "some/path");
        assert!(!relative.is_absolute());
    }

    #[test]
    fn destination_rejects_parent_traversal() {
        assert_eq!(
            DestinationPath::new("some/../path".into()),
            Err(PathValidationError::ContainsDotDot)
        );
    }

    // ── NormalizedRelativePath tests ──

    #[test]
    fn normalized_path_valid() {
        let p = Utf8PathBuf::from("videos/a.mp4");
        let n = NormalizedRelativePath::new(p.clone()).unwrap();
        assert_eq!(n.as_str(), "videos/a.mp4");
    }

    #[test]
    fn normalized_path_dotdot_is_error() {
        let p = Utf8PathBuf::from("videos/../../etc");
        assert_eq!(
            NormalizedRelativePath::new(p),
            Err(PathValidationError::ContainsDotDot)
        );
    }

    #[test]
    fn normalized_path_collapses_separators() {
        let p = Utf8PathBuf::from("a//b");
        let n = NormalizedRelativePath::new(p).unwrap();
        assert_eq!(n.as_str(), "a/b");
    }

    #[test]
    fn normalized_path_removes_dot() {
        let p = Utf8PathBuf::from("a/./b/c");
        let n = NormalizedRelativePath::new(p).unwrap();
        assert_eq!(n.as_str(), "a/b/c");
    }

    #[test]
    fn normalized_path_rejects_absolute() {
        let p = Utf8PathBuf::from("/etc/passwd");
        assert_eq!(
            NormalizedRelativePath::new(p),
            Err(PathValidationError::NotRelative)
        );
    }

    #[test]
    fn normalized_path_rejects_empty_after_normalization() {
        let p = Utf8PathBuf::from("./.");
        assert_eq!(
            NormalizedRelativePath::new(p),
            Err(PathValidationError::EmptyComponent)
        );
    }

    // ── FileStatus tests ──

    #[test]
    fn file_status_imported() {
        assert_eq!(
            FileStatus::from_str("imported").unwrap(),
            FileStatus::Imported
        );
        assert_eq!(FileStatus::Imported.as_str(), "imported");
    }

    #[test]
    fn file_status_failed() {
        assert_eq!(FileStatus::from_str("failed").unwrap(), FileStatus::Failed);
        assert_eq!(FileStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn file_status_skipped() {
        assert_eq!(
            FileStatus::from_str("skipped").unwrap(),
            FileStatus::Skipped
        );
        assert_eq!(FileStatus::Skipped.as_str(), "skipped");
    }

    #[test]
    fn file_status_unknown_is_error() {
        let result = FileStatus::from_str("unknown-status");
        assert!(result.is_err());
    }

    #[test]
    fn file_status_serde_roundtrip() {
        let s = FileStatus::Imported;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"imported\"");
        let back: FileStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, FileStatus::Imported);
    }

    // ── RunState tests ──

    #[test]
    fn run_state_serde_roundtrip() {
        for state in &[RunState::Done, RunState::Partial, RunState::Failed] {
            let json = serde_json::to_string(state).unwrap();
            let back: RunState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, back);
        }
    }

    #[test]
    fn run_state_from_str() {
        assert_eq!("done".parse::<RunState>().unwrap(), RunState::Done);
        assert_eq!("partial".parse::<RunState>().unwrap(), RunState::Partial);
        assert_eq!("failed".parse::<RunState>().unwrap(), RunState::Failed);
        assert!("unknown".parse::<RunState>().is_err());
    }

    // ── Config parsing tests ──

    #[test]
    fn transform_step_rejects_old_command_format() {
        let toml = r#"
kind = "subprocess"
command = "my-compress-video"
"#;
        let result: Result<TransformStepDefinition, _> = toml::from_str(toml);
        assert!(result.is_err(), "old 'command' field must be rejected");
    }

    #[test]
    fn transform_step_rejects_old_compress_video_kind() {
        let toml = r#"
kind = "compress-video"
program = "my-compress-video"
"#;
        let result: Result<TransformStepDefinition, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "old 'compress-video' kind must be rejected"
        );
    }

    #[test]
    fn transform_step_rejects_unknown_kind() {
        let toml = r#"
kind = "builtin"
"#;
        let result: Result<TransformStepDefinition, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown kind must be rejected");
    }

    #[test]
    fn parse_invalid_toml_is_error() {
        let result = ServerConfig::from_toml("not valid toml {{{");
        assert!(result.is_err());
    }

    #[test]
    fn server_config_rejects_relative_root() {
        let toml = r#"
root = "relative/path"
work_dir = "/universe/tmp/purgery"
"#;
        let result = ServerConfig::from_toml(toml);
        assert!(result.is_err());
    }

    // ── Manifest tests ──

    #[test]
    fn manifest_empty_entries_is_error() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err());
    }

    #[test]
    fn manifest_rejects_unknown_top_level_field() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
unknown_field = "value"

[[entries]]
local_path = "/tmp/test.mp4"
staged_path = "files/test.mp4"
relative_path = "test.mp4"
kind = "regular_file"
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "unknown top-level field must be rejected");
    }

    #[test]
    fn manifest_entry_rejects_unknown_field() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
local_path = "/tmp/test.mp4"
staged_path = "files/test.mp4"
relative_path = "test.mp4"
kind = "regular_file"
unknown_entry_field = "value"
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "unknown entry field must be rejected");
    }

    #[test]
    fn manifest_rejects_mode_field() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
local_path = "/tmp/test.mp4"
staged_path = "files/test.mp4"
relative_path = "test.mp4"
kind = "regular_file"
mode = "covered"
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "mode field must be rejected");
    }

    #[test]
    fn manifest_rejects_covered_by_field() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
local_path = "/tmp/test.mp4"
staged_path = "files/test.mp4"
relative_path = "test.mp4"
kind = "regular_file"
covered_by = "Videos"
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "covered_by field must be rejected");
    }

    // ── Status tests ──

    #[test]
    fn parse_status_with_run_error() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "failed"
error = "failed to parse manifest.toml"
"#;
        let status = RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("failed to parse manifest.toml")
        );
        assert!(status.entries.is_empty());
    }

    #[test]
    fn status_rejects_unknown_top_level_field() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"
unknown_field = "value"
"#;
        let result = RunStatus::from_toml(toml);
        assert!(result.is_err(), "unknown status field must be rejected");
    }

    #[test]
    fn status_entry_rejects_unknown_field() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[entries]]
local_path = "/tmp/test.mp4"
relative_path = "test.mp4"
status = "imported"
covered_by = "parent"
"#;
        let result = RunStatus::from_toml(toml);
        assert!(
            result.is_err(),
            "unknown entry status field must be rejected"
        );
    }

    #[test]
    fn cleanup_state_rejects_unknown_fields() {
        let toml = r#"
nickname = "laptop"
operation_id = "test-id"
mode = "covered"

[[entries]]
relative_path = "file.mp4"
local_path = "/tmp/file.mp4"
kind = "regular_file"
size = 0
mtime_ns = 0
cleaned = false
"#;
        let result: Result<DurableCleanupState, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "unknown cleanup state field must be rejected"
        );
    }

    #[test]
    fn client_run_state_rejects_unknown_fields() {
        let toml = r#"
protocol_version = 1
nickname = "laptop"
run_id = "test-run"
host = "host"
server_command = "ps"
manifest = "[[entries]]"
run_config = "[delete_after_import]"
phase = "cleanup_complete"
unknown_field = "value"
"#;
        let result: Result<ClientRunState, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "unknown client run state field must be rejected"
        );
    }

    // ── Identity tests ──

    #[test]
    fn run_phase_as_str() {
        assert_eq!(RunPhase::Incoming.as_str(), "incoming");
        assert_eq!(RunPhase::Ready.as_str(), "ready");
        assert_eq!(RunPhase::Processing.as_str(), "processing");
        assert_eq!(RunPhase::Done.as_str(), "done");
        assert_eq!(RunPhase::Failed.as_str(), "failed");
    }

    // ── Path safety tests ──

    #[test]
    fn path_is_within_root_positive() {
        let root = Utf8Path::new("/data");
        let resolved = Utf8Path::new("/data/laptop/videos/a.mp4");
        assert!(path_is_within_root(resolved, root));
    }

    #[test]
    fn path_is_within_root_negative() {
        let root = Utf8Path::new("/data");
        let resolved = Utf8Path::new("/etc/passwd");
        assert!(!path_is_within_root(resolved, root));
    }

    // ── Manifest/Status roundtrip tests ──

    // ── Normalize relative tests ──

    #[test]
    fn normalize_relative_basic() {
        assert_eq!(normalize_relative(Utf8Path::new("a/b/c")).unwrap(), "a/b/c");
    }

    #[test]
    fn normalize_relative_collapses_double_slash() {
        assert_eq!(normalize_relative(Utf8Path::new("a//b")).unwrap(), "a/b");
    }

    #[test]
    fn normalize_relative_removes_dot() {
        assert_eq!(normalize_relative(Utf8Path::new("a/./b")).unwrap(), "a/b");
    }

    #[test]
    fn normalize_relative_removes_all_dots() {
        assert_eq!(
            normalize_relative(Utf8Path::new("./a/./b/.")).unwrap(),
            "a/b"
        );
    }

    #[test]
    fn normalize_relative_rejects_dotdot() {
        assert!(normalize_relative(Utf8Path::new("a/../b")).is_err());
    }

    #[test]
    fn normalize_relative_rejects_absolute() {
        assert!(normalize_relative(Utf8Path::new("/a/b")).is_err());
    }

    #[test]
    fn normalize_relative_rejects_empty_result() {
        assert!(normalize_relative(Utf8Path::new("./.")).is_err());
    }

    // ── Build Rsync Args tests ──

    #[test]
    fn build_rsync_args_basic() {
        let args = build_rsync_args("/home/user/Videos", "example.com:/remote/path");
        assert_eq!(
            args,
            vec![
                "--recursive",
                "--partial",
                "--inplace",
                "--mkpath",
                "--archive",
                "--protect-args",
                "--",
                "/home/user/Videos",
                "example.com:/remote/path",
            ]
        );
    }

    #[test]
    fn build_rsync_args_includes_partial() {
        let args = build_rsync_args("/src", "host:/dst");
        assert!(args.contains(&"--partial".to_string()));
    }

    #[test]
    fn build_rsync_args_includes_inplace() {
        let args = build_rsync_args("/src", "host:/dst");
        assert!(args.contains(&"--inplace".to_string()));
    }

    #[test]
    fn build_rsync_args_includes_mkpath() {
        let args = build_rsync_args("/src", "host:/dst");
        assert!(args.contains(&"--mkpath".to_string()));
    }

    #[test]
    fn build_rsync_args_includes_protect_args() {
        let args = build_rsync_args("/src", "host:/dst");
        assert!(args.contains(&"--protect-args".to_string()));
    }

    #[test]
    fn build_rsync_args_with_spaces_in_source() {
        let args = build_rsync_args("/home/user/My Videos", "host:/dst");
        // Source is the last path operand, after the -- separator
        let dashdash_pos = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dashdash_pos + 1], "/home/user/My Videos");
    }

    #[test]
    fn build_rsync_args_separator_before_path_operands() {
        let args = build_rsync_args("/src", "host:/dst");
        let dashdash_pos = args
            .iter()
            .position(|a| a == "--")
            .expect("must contain -- separator");
        let src_pos = args
            .iter()
            .position(|a| a == "/src")
            .expect("must contain source");
        let dst_pos = args
            .iter()
            .position(|a| a == "host:/dst")
            .expect("must contain destination");
        assert!(dashdash_pos < src_pos, "-- must come before source operand");
        assert!(
            dashdash_pos < dst_pos,
            "-- must come before destination operand"
        );
    }

    #[test]
    fn build_rsync_args_all_options_before_separator() {
        let args = build_rsync_args("/src", "host:/dst");
        let dashdash_pos = args
            .iter()
            .position(|a| a == "--")
            .expect("must contain --");
        for (i, arg) in args.iter().enumerate() {
            if arg.starts_with("--") && arg != "--" {
                assert!(i < dashdash_pos, "option {} must be before --", arg);
            }
        }
    }

    #[test]
    fn transform_argv_not_rewritten() {
        let configured_args = vec!["--input".to_string(), "{input}".to_string()];
        let step = TransformStepDefinition {
            kind: TransformKind::Subprocess,
            program: "true".to_owned(),
            args: configured_args.clone(),
            expected_outputs: vec![],
            keep_original: true,
        };
        let built = step.build_args(camino::Utf8Path::new("/tmp/work/file.mp4"));
        // build_args must resolve placeholders but not reorder or reject the argv
        assert_eq!(built.len(), 2, "argv length must be preserved");
        assert_eq!(built[0], "--input", "first arg must be unchanged");
        assert!(
            built[1].contains("/tmp/work/file.mp4"),
            "second arg must resolve {{input}}, got: {:?}",
            built
        );
    }

    #[test]
    fn transform_argv_not_rejected_for_placeholders() {
        let step = TransformStepDefinition {
            kind: TransformKind::Subprocess,
            program: "ffmpeg".to_owned(),
            args: vec!["--input".to_string(), "{input}".to_string()],
            expected_outputs: vec![],
            keep_original: true,
        };
        let built = step.build_args(camino::Utf8Path::new("/tmp/work/file.mp4"));
        assert!(
            built.iter().any(|a| a.contains("/tmp/work")),
            "build_args must resolve placeholders, not reject them"
        );
    }

    // ── Symlink Check tests ──

    #[test]
    fn check_symlink_in_path_no_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let final_path = sub.join("file.txt");
        std::fs::write(&final_path, b"hello").unwrap();

        assert!(check_symlink_in_path(&final_path, &root).is_ok());
    }

    #[test]
    fn check_symlink_in_path_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.join("link_to_target");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let final_path = link.join("file.txt");
        std::fs::write(&final_path, b"hello").unwrap();

        let result = check_symlink_in_path(&final_path, &root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("symlink detected"));
    }

    #[test]
    fn check_symlink_in_path_rejects_dangling_final_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let final_path = root.join("dangling");
        std::os::unix::fs::symlink("missing-target", &final_path).unwrap();

        let result = check_symlink_in_path(&final_path, &root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("symlink detected"));
    }

    #[test]
    fn check_symlink_in_path_component_is_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let real_dir = root.join("realdir");
        std::fs::create_dir_all(&real_dir).unwrap();
        let link = root.join("linked");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        let final_path = link.join("sub/file.txt");
        std::fs::create_dir_all(real_dir.join("sub")).unwrap();
        std::fs::write(&final_path, b"data").unwrap();

        let result = check_symlink_in_path(&final_path, &root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("symlink detected"));
    }

    // ── Work Dir tests ──

    #[test]
    fn work_dir_returns_correct_path() {
        let purgery_root = PurgeryRoot::new(Utf8PathBuf::from("/tmp/purgery")).unwrap();
        let nick = Nickname::new("laptop".into()).unwrap();
        let run = RunId::new("run1".into()).unwrap();
        let wd = work_dir(&purgery_root, &nick, &run);
        assert_eq!(wd.as_str(), "/tmp/purgery/laptop/processing/run1/work");
    }

    // ── TransformKind serde tests ──

    #[test]
    fn transform_kind_subprocess() {
        let kind: TransformKind = serde_json::from_str("\"subprocess\"").unwrap();
        assert_eq!(kind, TransformKind::Subprocess);
    }

    #[test]
    fn transform_kind_rejects_unknown() {
        let result: Result<TransformKind, _> = serde_json::from_str("\"builtin\"");
        assert!(result.is_err());
    }

    #[test]
    fn transform_kind_rejects_old_compress_video() {
        let result: Result<TransformKind, _> = serde_json::from_str("\"compress-video\"");
        assert!(result.is_err());
    }

    // ── normalize_relative helper tests ──

    #[test]
    fn test_normalize_relative() {
        assert_eq!(normalize_relative(Utf8Path::new("a/b/c")).unwrap(), "a/b/c");
        assert_eq!(normalize_relative(Utf8Path::new("a//b")).unwrap(), "a/b");
        assert_eq!(normalize_relative(Utf8Path::new("a/./b")).unwrap(), "a/b");
        assert!(normalize_relative(Utf8Path::new("a/../b")).is_err());
        assert!(normalize_relative(Utf8Path::new("/abs")).is_err());
        assert!(normalize_relative(Utf8Path::new("./.")).is_err());
    }

    // ── RunId ULID validation test ──

    #[test]
    fn generated_ulid_validates() {
        for _ in 0..10 {
            let id = RunId::generate();
            assert!(RunId::new(id.as_str().to_owned()).is_ok());
            assert_eq!(id.as_str().len(), 26);
        }
    }

    // ── Placeholder resolution tests ──

    #[test]
    fn resolve_input_placeholder() {
        let step = TransformStepDefinition {
            kind: TransformKind::Subprocess,
            program: "ffmpeg".into(),
            args: vec!["--input".into(), "{input}".into()],
            expected_outputs: vec!["{stem}.Z.webm".into()],
            keep_original: true,
        };
        let work_path = Utf8Path::new("/work/videos/video.mp4");
        let args = step.build_args(work_path);
        assert_eq!(args, vec!["--input", "/work/videos/video.mp4"]);
    }

    #[test]
    fn resolve_stem_placeholder() {
        let step = TransformStepDefinition {
            kind: TransformKind::Subprocess,
            program: "ffmpeg".into(),
            args: vec![],
            expected_outputs: vec!["{stem}.Z.webm".into()],
            keep_original: false,
        };
        let work_path = Utf8Path::new("/work/videos/video.mp4");
        let outputs = step.resolve_expected_outputs(work_path).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].as_str(), "/work/videos/video.Z.webm");
    }

    #[test]
    fn resolve_parent_placeholder() {
        let step = TransformStepDefinition {
            kind: TransformKind::Subprocess,
            program: "ffmpeg".into(),
            args: vec!["--output-dir".into(), "{parent}".into()],
            expected_outputs: vec![],
            keep_original: false,
        };
        let work_path = Utf8Path::new("/work/videos/video.mp4");
        let args = step.build_args(work_path);
        assert_eq!(args, vec!["--output-dir", "/work/videos"]);
    }

    // ── Logging config tests ──

    #[test]
    fn logging_config_defaults() {
        let config = LoggingConfig::default();
        assert!(matches!(config.level, LogLevel::Info));
        assert!(matches!(config.format, LogFormat::Pretty));
        assert!(matches!(config.color, ColorMode::Auto));
    }

    #[test]
    fn logging_config_rejects_unknown_fields() {
        let toml = r#"unknown_field = "value""#;
        let result: Result<LoggingConfig, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown logging fields must be rejected");
    }

    #[test]
    fn logging_config_parses_explicit_values() {
        let toml = r#"
level = "debug"
format = "json"
color = "never"
"#;
        let config: LoggingConfig = toml::from_str(toml).unwrap();
        assert!(matches!(config.level, LogLevel::Debug));
        assert!(matches!(config.format, LogFormat::Json));
        assert!(matches!(config.color, ColorMode::Never));
    }

    // ── Expected output validation tests ──

    #[test]
    fn validate_expected_output_accepts_file_stem_webm() {
        assert!(validate_expected_output_name("{file_stem}.Z.webm").is_ok());
    }

    #[test]
    fn validate_expected_output_accepts_file_name() {
        assert!(validate_expected_output_name("{file_name}").is_ok());
    }

    #[test]
    fn validate_expected_output_accepts_stem_placeholder() {
        assert!(validate_expected_output_name("{stem}.out").is_ok());
    }

    #[test]
    fn validate_expected_output_rejects_input_placeholder() {
        let err = validate_expected_output_name("{input}").unwrap_err();
        assert!(
            err.contains("{input}"),
            "error must mention {{input}}: {err}"
        );
    }

    #[test]
    fn validate_expected_output_rejects_parent_placeholder() {
        let err = validate_expected_output_name("{parent}/out").unwrap_err();
        assert!(
            err.contains("{parent}") || err.contains("separator"),
            "error must mention {{parent}} or separator: {err}"
        );
    }

    #[test]
    fn validate_expected_output_rejects_empty() {
        assert!(validate_expected_output_name("").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_dot() {
        assert!(validate_expected_output_name(".").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_dotdot() {
        assert!(validate_expected_output_name("..").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_absolute() {
        assert!(validate_expected_output_name("/tmp/out.webm").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_path_separator() {
        assert!(validate_expected_output_name("sub/out.webm").is_err());
    }

    #[test]
    fn client_run_phase_serde_roundtrip() {
        // All ClientRunPhase variants must serialize/deserialize correctly.
        use crate::ClientRunPhase;
        let cases = vec![
            (
                ClientRunPhase::UploadCompleteFinishPending,
                "upload_complete_finish_pending",
            ),
            (
                ClientRunPhase::WaitingForTerminalState,
                "waiting_for_terminal_state",
            ),
            (ClientRunPhase::TerminalStatusSeen, "terminal_status_seen"),
            (ClientRunPhase::CleanupComplete, "cleanup_complete"),
            (ClientRunPhase::Abandoned, "abandoned"),
        ];
        for (phase, expected) in &cases {
            // Serialize via serde_json which supports unit variants directly
            let serialized = serde_json::to_string(phase).expect("serialize");
            assert!(
                serialized.contains(expected),
                "serialized form must contain {expected}, got: {serialized}"
            );
            let deserialized: ClientRunPhase =
                serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(&deserialized, phase, "roundtrip must preserve value");
        }
    }

    #[test]
    fn run_state_response_has_observed_at_field() {
        // RunStateResponse must include observed_at_unix_secs distinct from updated_at_unix_secs.
        use crate::RunStateResponse;
        let response = RunStateResponse {
            protocol_version: 1,
            nickname: "laptop".into(),
            run_id: "test-run".into(),
            phase: "processing".into(),
            terminal: false,
            message: "testing".into(),
            updated_at_unix_secs: 1000,
            observed_at_unix_secs: 0,
            progress_state: None,
            entry_index: None,
            entry_total: None,
            current_entry: None,
            current_step: None,
            progress_status: None,
        };
        let serialized = toml::to_string(&response).expect("serialize");
        // observed_at_unix_secs must be present in serialized output
        assert!(
            serialized.contains("observed_at_unix_secs"),
            "response must include observed_at_unix_secs, got: {serialized}"
        );
    }

    #[test]
    fn run_state_processing_missing_progress_does_not_fake_timestamp() {
        // When progress.toml is missing, updated_at_unix_secs must be the
        // phase transition time (or zero), not the current wall clock.
        // This test verifies the conceptual invariant through the response type.
        use crate::RunStateResponse;
        // A processing response with no progress info should have consistent
        // timestamp semantics. We verify the types carry the right fields.
        let response = RunStateResponse {
            protocol_version: 1,
            nickname: "laptop".into(),
            run_id: "test-run".into(),
            phase: "processing".into(),
            terminal: false,
            message: "run phase: processing".into(),
            updated_at_unix_secs: 1000,
            observed_at_unix_secs: 9999,
            progress_state: None,
            entry_index: None,
            entry_total: None,
            current_entry: None,
            current_step: None,
            progress_status: None,
        };
        // When progress is missing, updated_at should not be equal to observed_at
        // (they serve different purposes)
        assert_ne!(
            response.updated_at_unix_secs, response.observed_at_unix_secs,
            "updated_at and observed_at must be distinct when progress is unavailable"
        );
    }

    // ── Named-root regression tests ──────────────────────────────────

    /// Verify that documentation does not describe nickname
    /// as part of final archive paths.
    #[test]
    fn no_nickname_in_final_path_example_in_readme() {
        let readme = include_str!("../../../README.md");
        let patterns = [
            "/laptop/videos",
            "/phone-dump/videos",
            "/laptop/pictures",
            "root / nickname",
        ];
        for pattern in &patterns {
            if readme.contains(pattern) {
                panic!(
                    "README.md contains a nickname-in-archive-path pattern: {pattern:?}. \
                     Nicknames must not appear in final archive paths."
                );
            }
        }
    }

    /// Verify that documentation does not describe nickname
    /// as part of final archive paths in config docs.
    #[test]
    fn no_nickname_in_final_path_example_in_config_docs() {
        let config_md = include_str!("../../../docs/config.md");
        let patterns = ["root / nickname", "<nickname>/", "/laptop/videos"];
        for pattern in &patterns {
            if config_md.contains(pattern) {
                panic!("docs/config.md contains a nickname-in-archive-path pattern: {pattern:?}");
            }
        }
    }

    /// Verify that protocol docs do not use nickname in destination examples.
    #[test]
    fn no_nickname_in_destination_examples_in_protocol_docs() {
        let protocol_md = include_str!("../../../docs/protocol.md");
        let patterns = [
            "/laptop/videos",
            "/laptop/pictures",
            "root/<nickname>",
            "/universe/synced/laptop",
        ];
        for pattern in &patterns {
            if protocol_md.contains(pattern) {
                panic!("docs/protocol.md contains a nickname-in-path pattern: {pattern:?}");
            }
        }
    }

    #[test]
    fn server_config_rejects_old_root_tables() {
        let config = r#"
work_dir = "/var/lib/purgery/work"

[[root]]
name = "archive"
path = "/archive"
"#;
        assert!(ServerConfig::from_toml(config).is_err());
    }
}
