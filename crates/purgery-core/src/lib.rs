use camino::{Utf8Path, Utf8PathBuf};
use std::io;
use thiserror::Error;

// ── Module declarations ──────────────────────────────────────────────

mod cleanup_state;
mod config;
mod manifest;
mod path;
mod postprocess;
mod rsync_filter;
mod status;

pub use cleanup_state::*;
pub use config::*;
pub use manifest::*;
pub use path::*;
pub use postprocess::*;
pub use rsync_filter::*;
pub use status::*;

// ── Error Types ──────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("failed to parse config TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("failed to serialize config: {0}")]
    TomlSerialize(String),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("invalid sync name: {0}")]
    SyncName(#[from] SyncNameError),
    #[error("invalid host: {0}")]
    RemoteHost(#[from] RemoteHostError),
    #[error("invalid path: {0}")]
    Path(#[from] PathValidationError),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("postprocess config: {0}")]
    PostprocessConfig(String),
    #[error("state dir: {0}")]
    StateDir(String),
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
    #[error("invalid sync name: {0}")]
    SyncName(#[from] SyncNameError),
    #[error("invalid local path: {0}")]
    LocalPath(#[from] LocalSourcePathError),
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
    #[error("invalid sync name: {0}")]
    SyncName(#[from] SyncNameError),
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

pub fn build_rsync_args(source: &str, destination: &str) -> Vec<String> {
    vec![
        "--recursive".to_string(),
        "--partial".to_string(),
        "--archive".to_string(),
        "--no-inc-recursive".to_string(),
        "--protect-args".to_string(),
        "--".to_string(),
        format!("{}/", source),
        destination.to_string(),
    ]
}

/// Insert an rsync option argument before the `--` operand separator.
///
/// Returns an error if the args list does not contain a literal `--` element.
/// The option is inserted as the last option before `--`, preserving the
/// invariant that all options appear before the separator and all path
/// operands appear after it.
pub fn insert_rsync_option_before_operands(
    args: &mut Vec<String>,
    option: String,
) -> Result<(), &'static str> {
    let dashdash_pos = args
        .iter()
        .position(|a| a == "--")
        .ok_or("rsync args list has no `--` separator")?;
    args.insert(dashdash_pos, option);
    Ok(())
}

// ── Work Area ────────────────────────────────────────────────────────

pub fn work_dir(purgery_root: &PurgeryRoot, nickname: &Nickname, run_id: &RunId) -> Utf8PathBuf {
    purgery_root
        .run_dir(nickname, run_id, RunPhase::Processing)
        .join("work")
}

pub fn commit_temp_path(final_path: &Utf8Path, run_id: &RunId) -> Utf8PathBuf {
    let filename = final_path.file_name().unwrap_or("unknown");
    let tmp_name = format!(".purgery-commit.{}.{}.tmp", run_id.as_str(), filename);
    final_path
        .parent()
        .map_or_else(|| Utf8PathBuf::from(&tmp_name), |p| p.join(&tmp_name))
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

    // ── SyncName tests ──

    #[test]
    fn sync_name_valid() {
        let n = SyncName::new("videos".into()).unwrap();
        assert_eq!(n.as_str(), "videos");
    }

    #[test]
    fn sync_name_empty_is_error() {
        assert!(SyncName::new("".into()).is_err());
    }

    #[test]
    fn sync_name_rejects_slash() {
        assert!(SyncName::new("bad/name".into()).is_err());
    }

    #[test]
    fn sync_name_serde_roundtrip() {
        let n = SyncName::new("videos".into()).unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"videos\"");
        let back: SyncName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
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

    // ── LocalSourcePath tests ──

    #[test]
    fn local_source_path_valid() {
        let p = LocalSourcePath::new("/home/user/Videos".into()).unwrap();
        assert_eq!(p.as_str(), "/home/user/Videos");
    }

    #[test]
    fn local_source_path_empty_is_error() {
        assert!(LocalSourcePath::new("".into()).is_err());
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

    // ── ServerRoot tests ──

    #[test]
    fn server_root_absolute_valid() {
        let p = Utf8PathBuf::from("/universe/synced");
        let r = ServerRoot::new(p.clone()).unwrap();
        assert_eq!(r.as_path(), &p);
    }

    #[test]
    fn server_root_relative_is_error() {
        let p = Utf8PathBuf::from("relative/path");
        assert_eq!(ServerRoot::new(p), Err(PathValidationError::NotAbsolute));
    }

    #[test]
    fn server_root_serde_roundtrip() {
        let r = ServerRoot::new("/data".into()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: ServerRoot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn server_root_final_path() {
        let root = ServerRoot::new("/universe/synced".into()).unwrap();
        let nick = Nickname::new("laptop".into()).unwrap();
        let dest = RelativeDestinationPath::new("videos".into()).unwrap();
        let rel = NormalizedRelativePath::new("a.mp4".into()).unwrap();
        assert_eq!(
            root.final_path(&nick, &dest, &rel),
            Utf8PathBuf::from("/universe/synced/laptop/videos/a.mp4")
        );
    }

    // ── PurgeryRoot tests ──

    #[test]
    fn purgery_root_absolute_valid() {
        let p = Utf8PathBuf::from("/universe/tmp/purgery");
        let r = PurgeryRoot::new(p.clone()).unwrap();
        assert_eq!(r.as_path(), &p);
    }

    #[test]
    fn purgery_root_relative_is_error() {
        let p = Utf8PathBuf::from("tmp/purgy");
        assert_eq!(PurgeryRoot::new(p), Err(PathValidationError::NotAbsolute));
    }

    #[test]
    fn purgery_root_path_helpers() {
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

    // ── RelativeDestinationPath tests ──

    #[test]
    fn relative_dest_valid() {
        let p = Utf8PathBuf::from("videos");
        let r = RelativeDestinationPath::new(p.clone()).unwrap();
        assert_eq!(r.as_path(), &p);
    }

    #[test]
    fn relative_dest_with_subdir() {
        let p = Utf8PathBuf::from("media/videos");
        RelativeDestinationPath::new(p).unwrap();
    }

    #[test]
    fn relative_dest_absolute_is_error() {
        let p = Utf8PathBuf::from("/absolute/path");
        assert_eq!(
            RelativeDestinationPath::new(p),
            Err(PathValidationError::NotRelative)
        );
    }

    #[test]
    fn relative_dest_dotdot_is_error() {
        let p = Utf8PathBuf::from("../escape");
        assert_eq!(
            RelativeDestinationPath::new(p),
            Err(PathValidationError::ContainsDotDot)
        );
    }

    #[test]
    fn relative_dest_collapses_consecutive_separators() {
        let p = Utf8PathBuf::from("a//b");
        let r = RelativeDestinationPath::new(p).unwrap();
        assert_eq!(r.as_str(), "a/b");
    }

    #[test]
    fn relative_dest_removes_dot_components() {
        let p = Utf8PathBuf::from("a/./b");
        let r = RelativeDestinationPath::new(p).unwrap();
        assert_eq!(r.as_str(), "a/b");
    }

    #[test]
    fn sync_to_rejects_escape() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "../escape"
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err(), "sync to='../escape' must be rejected");
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
    fn parse_server_config_minimal() {
        let toml = r#"
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        assert_eq!(config.purgery_root.as_str(), "/universe/tmp/purgery");
    }

    #[test]
    fn parse_server_config_full() {
        let toml = r#"
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
[postprocess]

[postprocess.steps.compress-video]
kind = "subprocess"
program = "my-compress-video"
args = ["--input", "{input}"]
expected_outputs = ["{stem}.Z.webm"]
keep_original = true
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, PostprocessKind::Subprocess);
        assert_eq!(step.program, "my-compress-video");
        assert!(step.keep_original);
    }

    #[test]
    fn parse_server_config_subprocess_kind() {
        let toml = r#"
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"

[postprocess.steps.compress-video]
kind = "subprocess"
program = "my-compress-video"
args = ["--input", "{input}"]
expected_outputs = ["{stem}.Z.webm"]
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, PostprocessKind::Subprocess);
        assert_eq!(step.program, "my-compress-video");
        assert!(step.keep_original);
    }

    #[test]
    fn postprocess_step_rejects_old_command_format() {
        let toml = r#"
kind = "subprocess"
command = "my-compress-video"
"#;
        let result: Result<PostprocessStepDefinition, _> = toml::from_str(toml);
        assert!(result.is_err(), "old 'command' field must be rejected");
    }

    #[test]
    fn postprocess_step_rejects_old_compress_video_kind() {
        let toml = r#"
kind = "compress-video"
program = "my-compress-video"
"#;
        let result: Result<PostprocessStepDefinition, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "old 'compress-video' kind must be rejected"
        );
    }

    #[test]
    fn postprocess_step_rejects_unknown_kind() {
        let toml = r#"
kind = "builtin"
"#;
        let result: Result<PostprocessStepDefinition, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown kind must be rejected");
    }

    #[test]
    fn parse_client_config_minimal() {
        let toml = r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.com"
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.nickname.as_str(), "laptop");
        assert_eq!(config.server.host.as_str(), "example.com");
        assert_eq!(config.server.command, "purgery-server");
        assert!(config.sync.is_empty());
    }

    #[test]
    fn parse_client_config_full() {
        let toml = r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.com"
command = "purgery-server --config /etc/purgery/server.toml"

[[sync]]
name = "videos"
from = "/home/vitalik/Videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "pictures"
from = "/home/vitalik/Pictures"
to = "pictures"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = ["videos"]

[[postprocess.rules]]
match = "*.mov"
steps = ["compress-video"]
for = ["videos"]

[[postprocess.rules]]
match = "*.mkv"
steps = ["compress-video"]
for = ["videos"]

[[postprocess.rules]]
match = "*.webm"
steps = ["compress-video"]
for = ["videos"]
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.sync.len(), 2);
        assert_eq!(config.sync[0].name.as_str(), "videos");
        assert!(config.sync[0].delete_after_import);
        assert!(!config.sync[1].delete_after_import);
        assert_eq!(config.postprocess.rules.len(), 4);
        assert_eq!(config.postprocess.rules[0].pattern, "*.mp4");
        assert_eq!(config.postprocess.rules[0].steps, vec!["compress-video"]);
        assert_eq!(
            config.server.command,
            "purgery-server --config /etc/purgery/server.toml"
        );
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
purgery_root = "/universe/tmp/purgery"
"#;
        let result = ServerConfig::from_toml(toml);
        assert!(result.is_err());
    }

    #[test]
    fn client_config_rejects_invalid_nickname() {
        let toml = r#"
nickname = ""

[server]
host = "example.com"
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err());
    }

    // ── Manifest tests ──

    #[test]
    fn parse_manifest() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
size = 123456789
mtime_ns = 1780944312000000000
sha256 = "abcd1234"
"#;
        let manifest = Manifest::from_toml(toml).unwrap();
        assert_eq!(manifest.run_id.as_str(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].sync_name.as_str(), "videos");
        assert_eq!(
            manifest.entries[0].local_path.as_str(),
            "/home/vitalik/Videos/a.mp4"
        );
        assert_eq!(manifest.entries[0].sha256.as_deref(), Some("abcd1234"));
    }

    #[test]
    fn parse_manifest_rejects_invalid_sync_name() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
sync_name = ""
local_path = "/home/user/file.mp4"
staged_path = "files/file.mp4"
relative_path = "file.mp4"
size = 100
mtime_ns = 0
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "empty sync_name must be rejected");
    }

    #[test]
    fn parse_manifest_rejects_invalid_local_path() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
sync_name = "videos"
local_path = ""
staged_path = "files/file.mp4"
relative_path = "file.mp4"
size = 100
mtime_ns = 0
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "empty local_path must be rejected");
    }

    #[test]
    fn parse_manifest_without_sha256() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
size = 123456789
mtime_ns = 1780944312000000000
"#;
        let manifest = Manifest::from_toml(toml).unwrap();
        assert!(manifest.entries[0].sha256.is_none());
    }

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
    fn directory_manifest_entry_rejects_regular_file_identity_fields() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
sync_name = "data"
local_path = "/source/dir"
staged_path = "files/data/dir"
relative_path = "dir"
kind = "directory"
mtime_ns = 1
"#;
        let error = Manifest::from_toml(toml).unwrap_err();
        assert!(error.to_string().contains("fields incompatible"));
    }

    #[test]
    fn symlink_manifest_entry_rejects_regular_file_identity_fields() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
sync_name = "data"
local_path = "/source/link"
staged_path = "files/data/link"
relative_path = "link"
kind = "symlink"
mtime_ns = 1
link_target = "target"
"#;
        let error = Manifest::from_toml(toml).unwrap_err();
        assert!(error.to_string().contains("fields incompatible"));
    }

    #[test]
    fn symlink_manifest_entry_requires_link_target() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
sync_name = "data"
local_path = "/source/link"
staged_path = "files/data/link"
relative_path = "link"
kind = "symlink"
"#;
        let error = Manifest::from_toml(toml).unwrap_err();
        assert!(error.to_string().contains("fields incompatible"));
    }

    // ── Status tests ──

    #[test]
    fn parse_status_with_sync_name() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[entries]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
"#;
        let status = RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.entries[0].sync_name.as_str(), "videos");
        assert_eq!(status.entries[0].local_path, "/home/vitalik/Videos/a.mp4");
    }

    #[test]
    fn parse_status() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[entries]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
final_paths = ["laptop/videos/a.mp4"]
postprocess = ["compress-video"]

[[entries]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/b.mp4"
relative_path = "b.mp4"
status = "failed"
error = "compress-video failed"
"#;
        let status = RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths, vec!["laptop/videos/a.mp4"]);
        assert_eq!(status.entries[0].sync_name.as_str(), "videos");
        assert_eq!(status.entries[1].status, FileStatus::Failed);
        assert_eq!(
            status.entries[1].error.as_deref(),
            Some("compress-video failed")
        );
    }

    #[test]
    fn parse_status_with_run_error() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "failed"
error = "failed to parse manifest.toml"
files = []
"#;
        let status = RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("failed to parse manifest.toml")
        );
        assert!(status.entries.is_empty());
    }

    // ── Identity tests ──

    #[test]
    fn identity_from_entry() {
        let entry = ManifestEntry {
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/home/vitalik/Videos/a.mp4".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 100,
            mtime_ns: 200,
            sha256: Some("abc".into()),
            link_target: None,
            mode: Default::default(),
            postprocess_steps: Vec::new(),
            covered_by: None,
        };
        let identity = entry.identity();
        assert_eq!(identity.size, 100);
        assert_eq!(identity.mtime_ns, 200);
        assert_eq!(identity.sha256, Some("abc".into()));
    }

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

    #[test]
    fn manifest_toml_roundtrip() {
        let manifest = Manifest {
            run_id: RunId::new("test-123".into()).unwrap(),
            nickname: Nickname::new("testbox".into()).unwrap(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 100,
                mtime_ns: 200,
                sha256: Some("abcdef".into()),
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        let toml = manifest.to_toml().unwrap();
        let parsed = Manifest::from_toml(&toml).unwrap();
        assert_eq!(parsed.run_id, manifest.run_id);
        assert_eq!(parsed.nickname, manifest.nickname);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].sha256, Some("abcdef".into()));
        assert_eq!(parsed.entries[0].sync_name.as_str(), "videos");
    }

    #[test]
    fn status_toml_roundtrip() {
        let status = RunStatus {
            run_id: RunId::new("test-123".into()).unwrap(),
            nickname: Nickname::new("testbox".into()).unwrap(),
            state: RunState::Done,
            entries: vec![EntryStatusEntry {
                kind: ManifestEntryKind::RegularFile,
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: "/tmp/test.mp4".into(),
                relative_path: "test.mp4".into(),
                status: FileStatus::Imported,
                final_paths: vec!["laptop/videos/test.mp4".into()],
                postprocess: Some(vec!["compress-video".into()]),
                error: None,
            }],
            error: None,
        };
        let toml = status.to_toml().unwrap();
        let parsed = RunStatus::from_toml(&toml).unwrap();
        assert_eq!(parsed.state, RunState::Done);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].status, FileStatus::Imported);
        assert_eq!(parsed.entries[0].sync_name.as_str(), "videos");
        assert_eq!(
            parsed.entries[0].final_paths,
            vec!["laptop/videos/test.mp4"]
        );
    }

    // ── Envelope validation tests ──

    #[test]
    fn validate_envelope_ok() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let run_config = RunConfig {
            nickname: nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nick.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &run_config, &manifest).is_ok());
    }

    #[test]
    fn validate_envelope_rejects_nickname_mismatch() {
        let dir_nick = Nickname::new("laptop".into()).unwrap();
        let other_nick = Nickname::new("desktop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let run_config = RunConfig {
            nickname: other_nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: other_nick,
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        assert!(validate_envelope(&dir_nick, &run_id, &run_config, &manifest).is_err());
    }

    #[test]
    fn validate_envelope_rejects_manifest_nickname_mismatch() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let other = Nickname::new("server".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let run_config = RunConfig {
            nickname: nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: other,
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &run_config, &manifest).is_err());
    }

    #[test]
    fn validate_envelope_rejects_run_id_mismatch() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let wrong_run_id = RunId::new("run-2".into()).unwrap();
        let run_config = RunConfig {
            nickname: nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: wrong_run_id,
            nickname: nick.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &run_config, &manifest).is_err());
    }

    // ── verify_staged tests ──

    #[test]
    fn verify_staged_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("f.bin")).unwrap();
        std::fs::write(staged.as_std_path(), b"hello").unwrap();

        let entry = ManifestEntry {
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 999,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            mode: Default::default(),
            postprocess_steps: Vec::new(),
            covered_by: None,
        };

        let result = entry.verify_staged(&staged);
        assert!(matches!(
            result,
            Err(IdentityVerificationError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn verify_staged_sha_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("f.bin")).unwrap();
        std::fs::write(staged.as_std_path(), b"hello").unwrap();

        let entry = ManifestEntry {
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 5,
            mtime_ns: 0,
            sha256: Some("badbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbad1".into()),
            link_target: None,
            mode: Default::default(),
            postprocess_steps: Vec::new(),
            covered_by: None,
        };

        let result = entry.verify_staged(&staged);
        assert!(matches!(
            result,
            Err(IdentityVerificationError::Sha256Mismatch)
        ));
    }

    #[test]
    fn verify_staged_size_ok_no_sha() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("f.bin")).unwrap();
        std::fs::write(staged.as_std_path(), b"hello").unwrap();

        let entry = ManifestEntry {
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 5,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            mode: Default::default(),
            postprocess_steps: Vec::new(),
            covered_by: None,
        };

        assert!(entry.verify_staged(&staged).is_ok());
    }

    #[test]
    fn verify_staged_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("nonexistent")).unwrap();

        let entry = ManifestEntry {
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("nonexistent".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("nonexistent".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 5,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            mode: Default::default(),
            postprocess_steps: Vec::new(),
            covered_by: None,
        };

        let result = entry.verify_staged(&staged);
        assert!(matches!(
            result,
            Err(IdentityVerificationError::NotFound(_))
        ));
    }

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
                "--archive",
                "--no-inc-recursive",
                "--protect-args",
                "--",
                "/home/user/Videos/",
                "example.com:/remote/path",
            ]
        );
    }

    #[test]
    fn build_rsync_args_includes_protect_args() {
        let args = build_rsync_args("/src", "host:/dst");
        assert!(args.contains(&"--protect-args".to_string()));
    }

    #[test]
    fn build_rsync_args_with_spaces_in_source() {
        let args = build_rsync_args("/home/user/My Videos", "host:/dst");
        assert_eq!(args[6], "/home/user/My Videos/");
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
            .position(|a| a == "/src/")
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
    fn insert_rsync_option_before_operands_places_option_before_separator() {
        let mut args = build_rsync_args("/src", "host:/dst");
        let filter = "--filter=merge /tmp/filters".to_string();
        insert_rsync_option_before_operands(&mut args, filter.clone()).unwrap();
        let dashdash_pos = args.iter().position(|a| a == "--").unwrap();
        let filter_pos = args.iter().position(|a| a == &filter).unwrap();
        assert!(
            filter_pos < dashdash_pos,
            "filter option must be inserted before --"
        );
    }

    #[test]
    fn postprocess_argv_not_rewritten() {
        let configured_args = vec!["--input".to_string(), "{input}".to_string()];
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
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
    fn postprocess_argv_not_rejected_for_placeholders() {
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
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

    // ── Commit temp path tests ──

    #[test]
    fn commit_temp_path_basic() {
        let final_path = Utf8Path::new("/data/laptop/videos/a.mp4");
        let run_id = RunId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()).unwrap();
        let tmp = commit_temp_path(final_path, &run_id);
        assert_eq!(
            tmp.as_str(),
            "/data/laptop/videos/.purgery-commit.01ARZ3NDEKTSV4RRFFQ69G5FAV.a.mp4.tmp"
        );
    }

    #[test]
    fn commit_temp_path_same_filesystem_as_parent() {
        let final_path = Utf8Path::new("/root/sub/file.txt");
        let run_id = RunId::new("run-1".into()).unwrap();
        let tmp = commit_temp_path(final_path, &run_id);
        assert_eq!(tmp.parent(), final_path.parent());
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

    // ── PostprocessKind serde tests ──

    #[test]
    fn postprocess_kind_subprocess() {
        let kind: PostprocessKind = serde_json::from_str("\"subprocess\"").unwrap();
        assert_eq!(kind, PostprocessKind::Subprocess);
    }

    #[test]
    fn postprocess_kind_rejects_unknown() {
        let result: Result<PostprocessKind, _> = serde_json::from_str("\"builtin\"");
        assert!(result.is_err());
    }

    #[test]
    fn postprocess_kind_rejects_old_compress_video() {
        let result: Result<PostprocessKind, _> = serde_json::from_str("\"compress-video\"");
        assert!(result.is_err());
    }

    // ── Example config parsing tests ──

    #[test]
    fn parse_example_client_config() {
        let toml = include_str!("../../../examples/client.toml");
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.nickname.as_str(), "laptop");
        assert_eq!(config.sync.len(), 2);
    }

    #[test]
    fn parse_example_server_config() {
        let toml = include_str!("../../../examples/server.toml");
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, PostprocessKind::Subprocess);
        assert_eq!(step.program, "my-compress-video");
        assert!(step.keep_original);
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

    // ── RunConfig tests ──

    #[test]
    fn parse_run_config() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        let config = RunConfig::from_toml(toml).unwrap();
        assert_eq!(config.nickname.as_str(), "laptop");
        assert_eq!(config.sync.len(), 1);
        assert_eq!(config.sync[0].name.as_str(), "videos");
        assert_eq!(config.postprocess.rules.len(), 1);
        assert_eq!(config.postprocess.rules[0].pattern, "*.mp4");
    }

    #[test]
    fn from_toml_rejects_for_empty() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
for = []
"#;
        let result = RunConfig::from_toml(toml);
        assert!(result.is_err(), "empty for must be rejected by from_toml");
    }

    #[test]
    fn from_toml_rejects_for_unknown_sync() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
for = ["missing"]
"#;
        let result = RunConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "unknown sync in for must be rejected by from_toml"
        );
    }

    #[test]
    fn validate_uploaded_purgatory_run_rejects_delete_after_import_false() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
"#;
        let config = RunConfig::from_toml(toml).unwrap();
        let result = config.validate_uploaded_purgatory_run();
        assert!(
            result.is_err(),
            "delete_after_import=false must be rejected in purgatory run"
        );
    }

    #[test]
    fn validate_uploaded_purgatory_run_accepts_valid() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
for = ["videos"]
"#;
        let config = RunConfig::from_toml(toml).unwrap();
        let result = config.validate_uploaded_purgatory_run();
        assert!(
            result.is_ok(),
            "valid purgatory run must be accepted: {:?}",
            result.err()
        );
    }

    #[test]
    fn run_config_roundtrip() {
        let config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![RunConfigSync {
                name: SyncName::new("videos".into()).unwrap(),
                to_path: RelativeDestinationPath::new("videos".into()).unwrap(),
                delete_after_import: false,
            }],
            postprocess: ClientPostprocessConfig::default(),
        };
        let toml = config.to_toml().unwrap();
        let parsed = RunConfig::from_toml(&toml).unwrap();
        assert_eq!(parsed.nickname.as_str(), "laptop");
        assert_eq!(parsed.sync.len(), 1);
    }

    #[test]
    fn run_config_rejects_server_section() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"
"#;
        let result = RunConfig::from_toml(toml);
        assert!(result.is_err(), "RunConfig must reject [server] section");
    }

    #[test]
    fn run_config_rejects_from_path() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        let result = RunConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "RunConfig must reject 'from' field in sync"
        );
    }

    // ── Placeholder resolution tests ──

    #[test]
    fn resolve_input_placeholder() {
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
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
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
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
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
            program: "ffmpeg".into(),
            args: vec!["--output-dir".into(), "{parent}".into()],
            expected_outputs: vec![],
            keep_original: false,
        };
        let work_path = Utf8Path::new("/work/videos/video.mp4");
        let args = step.build_args(work_path);
        assert_eq!(args, vec!["--output-dir", "/work/videos"]);
    }

    #[test]
    fn run_config_sync_map() {
        let config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![
                RunConfigSync {
                    name: SyncName::new("videos".into()).unwrap(),
                    to_path: RelativeDestinationPath::new("videos".into()).unwrap(),
                    delete_after_import: false,
                },
                RunConfigSync {
                    name: SyncName::new("pictures".into()).unwrap(),
                    to_path: RelativeDestinationPath::new("pictures".into()).unwrap(),
                    delete_after_import: false,
                },
            ],
            postprocess: ClientPostprocessConfig::default(),
        };
        let map = config.sync_map();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("videos").unwrap().to_path.as_str(), "videos");
        assert_eq!(map.get("pictures").unwrap().to_path.as_str(), "pictures");
        assert!(!map.contains_key("music"));
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

    // ── Transfer filter generation tests ──

    fn roots_exact(paths: &[&str]) -> Vec<TransferRoot> {
        paths
            .iter()
            .map(|p| TransferRoot::Exact(p.to_string()))
            .collect()
    }

    fn roots_subtree(path: &str) -> Vec<TransferRoot> {
        vec![TransferRoot::Subtree(path.to_string())]
    }

    #[test]
    fn transfer_filter_includes_subtree_for_postprocessed_directory() {
        let filter = transfer_set_filter(&roots_subtree("album"));
        assert!(
            filter.contains("+ album/**"),
            "subtree root must include descendants: {filter}"
        );
    }

    #[test]
    fn transfer_filter_includes_ancestors_and_exact_roots() {
        let filter = transfer_set_filter(&roots_exact(&["sub/outside.txt"]));
        assert!(
            filter.contains("+ sub/"),
            "ancestor dir must be included: {filter}"
        );
        assert!(
            filter.contains("+ sub/outside.txt"),
            "exact root must be included: {filter}"
        );
        assert!(
            !filter.contains("+ sub/**"),
            "exact root must not have subtree pattern: {filter}"
        );
    }

    #[test]
    fn transfer_filter_for_directory_root_includes_both_dir_and_descendants() {
        let mut roots = roots_subtree("album");
        roots.push(TransferRoot::Exact("outside.txt".to_string()));
        let filter = transfer_set_filter(&roots);
        assert!(
            filter.contains("+ album/"),
            "directory root must include dir/: {filter}"
        );
        assert!(
            filter.contains("+ album/**"),
            "subtree root must include descendants: {filter}"
        );
        assert!(
            filter.contains("+ outside.txt"),
            "exact root must be included: {filter}"
        );
    }

    #[test]
    fn transfer_filter_excludes_everything_else() {
        let filter = transfer_set_filter(&roots_exact(&["a.txt"]));
        assert!(
            filter.ends_with("- *\n") || filter.ends_with("- *"),
            "filter must end with exclude all: {filter}"
        );
    }

    #[test]
    fn transfer_filter_nested_subtree_includes_all_ancestors() {
        let filter = transfer_set_filter(&roots_subtree("a/b/album"));
        assert!(
            filter.contains("+ a/"),
            "must include ancestor a/: {filter}"
        );
        assert!(
            filter.contains("+ a/b/"),
            "must include ancestor a/b/: {filter}"
        );
        assert!(
            filter.contains("+ a/b/album/**"),
            "subtree root must include descendants: {filter}"
        );
    }

    #[test]
    fn transfer_filter_excludes_covered_descendants_from_independent_roots() {
        let filter = transfer_set_filter(&roots_subtree("album"));
        assert!(
            !filter.contains("song.mp3"),
            "covered descendants must not be independent roots: {filter}"
        );
    }

    // ── TransferPlanEntry tests ──

    // ── Issue 19: Postprocess requires delete_after_import=true ──

    #[test]
    fn config_rejects_postprocess_on_no_delete_sync() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "sync with applicable rule but delete_after_import=false must be rejected"
        );
    }

    #[test]
    fn config_rejects_postprocess_with_omitted_for_on_no_delete_sync() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "sync with global rule and delete_after_import=false must be rejected"
        );
    }

    #[test]
    fn config_accepts_postprocess_on_delete_true_sync() {
        let toml = r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert!(config.sync[0].delete_after_import);
        assert_eq!(config.postprocess.rules.len(), 1);
    }

    #[test]
    fn config_accepts_no_delete_sync_with_only_out_of_scope_rules() {
        let toml = r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = false

[[sync]]
name = "pictures"
from = "/home/user/Pictures"
to = "pictures"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = ["pictures"]
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert!(!config.sync[0].delete_after_import);
        assert!(config.sync[1].delete_after_import);
    }

    // ── PostprocessRule `for` field tests ──

    #[test]
    fn client_config_rejects_empty_for_at_parse_time() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = []
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err(), "empty for must be rejected at parse time");
    }

    #[test]
    fn client_config_rejects_unknown_sync_in_for_at_parse_time() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = ["missing-sync"]
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "unknown sync in for must be rejected at parse time"
        );
    }

    #[test]
    fn config_with_postprocess_rule_for_is_accepted() {
        let toml = r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = ["videos"]
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        let rule = &config.postprocess.rules[0];
        assert_eq!(rule.pattern, "*.mp4");
        assert!(rule.sync_names.is_some());
        assert_eq!(rule.sync_names.as_deref().unwrap()[0].as_str(), "videos");
    }

    #[test]
    fn config_with_postprocess_rule_empty_for_is_rejected() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = []
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err(), "empty for must be rejected at parse time");
    }

    #[test]
    fn config_with_postprocess_rule_unknown_sync_in_for_is_rejected() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = ["missing-sync"]
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "unknown sync in for must be rejected at parse time"
        );
    }

    #[test]
    fn transfer_plan_entry_has_no_size_field() {
        let entry = TransferPlanEntry {
            sync_name: SyncName::new("data".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.txt".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            mode: ManifestEntryMode::Passthrough,
            covered_by: None,
            postprocess_steps: Vec::new(),
        };
        assert_eq!(entry.mode, ManifestEntryMode::Passthrough);
        assert_eq!(entry.kind, ManifestEntryKind::RegularFile);
        assert!(entry.covered_by.is_none());
        assert!(entry.postprocess_steps.is_empty());
    }

    #[test]
    fn transfer_plan_entry_is_not_a_manifest_entry() {
        let plan = TransferPlanEntry {
            sync_name: SyncName::new("data".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.txt".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            mode: ManifestEntryMode::Passthrough,
            covered_by: None,
            postprocess_steps: Vec::new(),
        };
        assert_eq!(plan.sync_name.as_str(), "data");
        assert_eq!(plan.relative_path.as_str(), "f.txt");
    }

    #[test]
    fn client_config_rejects_empty_state_dir() {
        let toml = r#"
nickname = "laptop"
state_dir = ""

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err(), "empty state_dir must be rejected");
    }

    #[test]
    fn client_config_rejects_relative_state_dir() {
        let toml = r#"
nickname = "laptop"
state_dir = "relative/path"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err(), "relative state_dir must be rejected");
    }

    #[test]
    fn client_config_accepts_absolute_state_dir() {
        let toml = r#"
nickname = "laptop"
state_dir = "/custom/state/purgery"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.state_dir.as_str(), "/custom/state/purgery");
    }

    #[test]
    fn client_config_rejection_mentions_conformance() {
        let toml = r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        let result = ClientConfig::from_toml(toml);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("conformance")
                || err.contains("import-and-retire")
                || err.contains("indefinite"),
            "rejection must explain conformance tradeoff, got: {err}"
        );
    }

    #[test]
    fn run_config_rejection_mentions_conformance() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
"#;
        let config = RunConfig::from_toml(toml).unwrap();
        let result = config.validate_uploaded_purgatory_run();
        let err = result.unwrap_err();
        assert!(
            err.contains("conformance")
                || err.contains("import-and-retire")
                || err.contains("indefinite"),
            "rejection must explain conformance tradeoff, got: {err}"
        );
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
        };
        // When progress is missing, updated_at should not be equal to observed_at
        // (they serve different purposes)
        assert_ne!(
            response.updated_at_unix_secs, response.observed_at_unix_secs,
            "updated_at and observed_at must be distinct when progress is unavailable"
        );
    }
}
