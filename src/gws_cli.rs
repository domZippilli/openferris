//! Shared `gws` (Google Workspace CLI) subprocess runner.
//!
//! Consolidates process-spawn/timeout/kill-on-drop/"not installed" logic that
//! used to be duplicated (with drifting behavior) across `gmail.rs`,
//! `email.rs`, `tools/gws.rs`, and a verbatim clone inlined in
//! `GwsTool::execute`. Callers still differ in how they want the result
//! shaped (parsed JSON vs raw bytes) and in their error-message formatting,
//! so this module exposes:
//!
//! - [`run`]: low-level, returns a structured [`GwsError`] with the exit
//!   status/stdout/stderr intact, so callers that need custom error
//!   formatting (or auth-error classification) can build on it.
//! - [`run_gws`]: the convenience wrapper matching `tools/gws.rs`'s
//!   formatting (the most careful of the original copies) for callers that
//!   just want an `anyhow::Result<Output>`.

use std::process::Output;
use std::time::Duration;

/// Timeout for a single `gws` subprocess invocation.
pub const GWS_TIMEOUT: Duration = Duration::from_secs(120);

/// Structured error from invoking the `gws` subprocess. Carries the raw
/// stdout/stderr (when available) so callers can classify failures — e.g.
/// [`GwsError::is_auth_error`] — without re-parsing a formatted string.
#[derive(Debug)]
pub enum GwsError {
    /// The `gws` binary is not on `PATH`.
    NotInstalled,
    /// The subprocess did not complete within [`GWS_TIMEOUT`].
    Timeout,
    /// Failed to spawn/wait on the subprocess for a reason other than "not
    /// installed" (e.g. permission denied).
    Spawn(std::io::Error),
    /// The subprocess ran and exited with a non-zero status.
    NonZeroExit {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
}

impl GwsError {
    /// Whether this looks like an expired/invalid Gmail auth error (as
    /// opposed to a transient or unrelated failure), based on the raw
    /// stdout/stderr content. Used by `gmail.rs` to trigger its auth backoff
    /// instead of a normal error log.
    pub fn is_auth_error(&self) -> bool {
        match self {
            GwsError::NonZeroExit { stdout, stderr, .. } => {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(stdout),
                    String::from_utf8_lossy(stderr)
                );
                combined.contains("401")
                    || combined.contains("authError")
                    || combined.contains("invalid_grant")
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for GwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GwsError::NotInstalled => write!(
                f,
                "gws is not installed. Install with: npm install -g @googleworkspace/cli"
            ),
            GwsError::Timeout => write!(f, "gws timed out after {:?}", GWS_TIMEOUT),
            GwsError::Spawn(e) => write!(f, "Failed to run gws: {}", e),
            GwsError::NonZeroExit {
                status,
                stdout,
                stderr,
            } => {
                let stdout = String::from_utf8_lossy(stdout);
                let stderr = String::from_utf8_lossy(stderr);
                let mut msg = String::new();
                if !stdout.is_empty() {
                    msg.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !msg.is_empty() {
                        msg.push('\n');
                    }
                    msg.push_str(&stderr);
                }
                write!(f, "gws exited with {}: {}", status, msg.trim())
            }
        }
    }
}

impl std::error::Error for GwsError {}

/// Run `gws` with `args`, enforcing the shared timeout and kill-on-drop
/// policy. Returns the raw output on success (status 0) or a structured
/// [`GwsError`] otherwise (including non-zero exit, with the exit status,
/// stdout, and stderr preserved).
pub async fn run(args: &[&str]) -> Result<Output, GwsError> {
    let output = tokio::time::timeout(
        GWS_TIMEOUT,
        tokio::process::Command::new("gws")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| GwsError::Timeout)?
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GwsError::NotInstalled
        } else {
            GwsError::Spawn(e)
        }
    })?;

    if !output.status.success() {
        return Err(GwsError::NonZeroExit {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }

    Ok(output)
}

/// Convenience wrapper over [`run`] for callers that just want an
/// `anyhow::Result<Output>` with `tools/gws.rs`'s original error formatting
/// (stdout then stderr, trimmed).
pub async fn run_gws(args: &[&str]) -> anyhow::Result<Output> {
    run(args).await.map_err(|e| anyhow::anyhow!(e))
}

/// Find the first [`GwsError`] in an `anyhow::Error`'s cause chain, if any.
/// Lets callers that wrap the runner's error with additional `.context(...)`
/// still classify the underlying `gws` failure structurally.
pub fn find_gws_error(e: &anyhow::Error) -> Option<&GwsError> {
    e.chain().find_map(|cause| cause.downcast_ref::<GwsError>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn test_exit_status(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code)
    }

    #[test]
    fn test_is_auth_error_detects_401() {
        let e = GwsError::NonZeroExit {
            status: test_exit_status(1),
            stdout: b"some 401 unauthorized".to_vec(),
            stderr: Vec::new(),
        };
        assert!(e.is_auth_error());
    }

    #[test]
    fn test_is_auth_error_detects_invalid_grant_in_stderr() {
        let e = GwsError::NonZeroExit {
            status: test_exit_status(1),
            stdout: Vec::new(),
            stderr: b"invalid_grant: token expired".to_vec(),
        };
        assert!(e.is_auth_error());
    }

    #[test]
    fn test_is_auth_error_false_for_unrelated_failure() {
        let e = GwsError::NonZeroExit {
            status: test_exit_status(1),
            stdout: b"not found".to_vec(),
            stderr: Vec::new(),
        };
        assert!(!e.is_auth_error());
    }

    #[test]
    fn test_is_auth_error_false_for_not_installed_and_timeout() {
        assert!(!GwsError::NotInstalled.is_auth_error());
        assert!(!GwsError::Timeout.is_auth_error());
    }

    #[test]
    fn test_find_gws_error_through_context() {
        let base: anyhow::Error = GwsError::NonZeroExit {
            status: test_exit_status(1),
            stdout: b"401".to_vec(),
            stderr: Vec::new(),
        }
        .into();
        let wrapped = base.context("Failed to fetch Drive file metadata");
        let found = find_gws_error(&wrapped).expect("GwsError should be in chain");
        assert!(found.is_auth_error());
    }

    #[test]
    fn test_find_gws_error_none_for_plain_error() {
        let e = anyhow::anyhow!("No historyId");
        assert!(find_gws_error(&e).is_none());
    }
}
