use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

use super::{
    evidence::{
        ArtifactEvidence, CheckDeclaration, CommandEvidence, EvidenceError, EvidenceRecord,
        EvidenceStatus,
    },
    evidence_pack::{CheckExecutionLog, EvidencePack, EvidencePackError},
    proof::{ProofError, ProofIdentity},
    verifier::{TrustedCheckError, TrustedCheckRunner, TrustedGit, VerificationError},
};

#[derive(Clone, Debug)]
pub(crate) struct ClosureExecutionRequest {
    pub(crate) mission_id: String,
    pub(crate) run_id: String,
    pub(crate) repository_path: String,
    pub(crate) worktree_path: String,
    pub(crate) base_revision: String,
    pub(crate) declarations: Vec<CheckDeclaration>,
}

/// Runs only the command checks explicitly declared on a mission. It never
/// interprets a shell string, and manual checks remain unresolved for a human.
#[cfg(test)]
pub(crate) fn execute_closure(
    request: ClosureExecutionRequest,
) -> Result<EvidencePack, ClosureExecutionError> {
    let cancelled = AtomicBool::new(false);
    execute_closure_cancellable(request, &cancelled)
}

pub(crate) fn execute_closure_cancellable(
    request: ClosureExecutionRequest,
    cancelled: &AtomicBool,
) -> Result<EvidencePack, ClosureExecutionError> {
    let identity = ProofIdentity::new(
        &request.mission_id,
        &request.run_id,
        &request.repository_path,
        &request.worktree_path,
        &request.base_revision,
    )?;
    let git = TrustedGit::discover()?;
    let runner = TrustedCheckRunner::new();
    let include_ignored = request
        .declarations
        .iter()
        .any(CheckDeclaration::includes_ignored);
    let mut records = BTreeMap::new();
    let mut summaries = BTreeMap::new();
    let mut execution_logs = BTreeMap::new();
    let worktree = Path::new(&request.worktree_path);

    for declaration in &request.declarations {
        if cancelled.load(Ordering::Acquire) {
            return Err(ClosureExecutionError::Cancelled);
        }
        let Some(command) = declaration.command_spec() else {
            continue;
        };
        let before = git.scan(worktree, include_ignored)?;
        let result = match runner.run_with_cancel(worktree, command, cancelled) {
            Ok(result) => result,
            Err(TrustedCheckError::Cancelled) => return Err(ClosureExecutionError::Cancelled),
            Err(error) => {
                summaries.insert(declaration.id().to_owned(), EvidenceStatus::Failed);
                execution_logs.insert(
                    declaration.id().to_owned(),
                    CheckExecutionLog::rejected(error.to_string()),
                );
                continue;
            }
        };
        let after = git.scan(worktree, include_ignored)?;
        let artifacts = declaration
            .required_artifact_paths()
            .filter_map(|path| {
                after
                    .evidence_snapshot()
                    .artifact_hash(path)
                    .map(|hash| ArtifactEvidence::new(path, hash, media_type_for_path(path)))
            })
            .collect::<Vec<_>>();
        let evidence = CommandEvidence::new(
            declaration,
            &identity,
            before.evidence_snapshot(),
            after.evidence_snapshot(),
            result.exit_code().unwrap_or(-1),
            result.started_at_unix_millis(),
            result.finished_at_unix_millis(),
            artifacts,
        );
        match evidence {
            Ok(evidence) => {
                let status = evidence.status_against(after.evidence_snapshot());
                summaries.insert(declaration.id().to_owned(), status);
                records.insert(
                    declaration.id().to_owned(),
                    EvidenceRecord::Command(Box::new(evidence)),
                );
                execution_logs.insert(
                    declaration.id().to_owned(),
                    CheckExecutionLog::completed(&result, None),
                );
            }
            Err(error) => {
                summaries.insert(declaration.id().to_owned(), EvidenceStatus::Failed);
                execution_logs.insert(
                    declaration.id().to_owned(),
                    CheckExecutionLog::completed(&result, Some(error.to_string())),
                );
            }
        }
    }

    if cancelled.load(Ordering::Acquire) {
        return Err(ClosureExecutionError::Cancelled);
    }
    let current = git.scan(worktree, include_ignored)?;
    EvidencePack::new(
        request.mission_id,
        request.run_id,
        identity,
        current.evidence_snapshot().clone(),
        records,
        summaries,
        execution_logs,
        now_millis(),
    )
    .map_err(Into::into)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn media_type_for_path(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => "application/json",
        Some("html") => "text/html",
        Some("txt" | "md" | "log") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Error)]
pub(crate) enum ClosureExecutionError {
    #[error("closure execution was cancelled")]
    Cancelled,
    #[error(transparent)]
    Proof(#[from] ProofError),
    #[error(transparent)]
    Verification(#[from] VerificationError),
    #[error(transparent)]
    Evidence(#[from] EvidenceError),
    #[error(transparent)]
    Pack(#[from] EvidencePackError),
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::mission::evidence::{CommandSpec, PathRule};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[cfg(unix)]
    #[test]
    fn declared_command_produces_exact_persistable_evidence() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        git(directory.path(), &["config", "user.name", "Nagi Test"]);
        git(
            directory.path(),
            &["config", "user.email", "nagi@example.invalid"],
        );
        let script = directory.path().join("check");
        std::fs::write(&script, "#!/bin/sh\nprintf 'verified'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let failing_script = directory.path().join("fail-check");
        std::fs::write(&failing_script, "#!/bin/sh\nprintf 'failed' >&2\nexit 7\n").unwrap();
        std::fs::set_permissions(&failing_script, std::fs::Permissions::from_mode(0o700)).unwrap();
        git(directory.path(), &["add", "check", "fail-check"]);
        git(directory.path(), &["commit", "-qm", "fixture"]);
        let base_revision = git(directory.path(), &["rev-parse", "HEAD"]);
        let canonical = directory.path().canonicalize().unwrap();
        let check = CheckDeclaration::command(
            "unit",
            CommandSpec::new("./check", [] as [&str; 0], "."),
            vec![PathRule::All],
            vec![],
        );

        let pack = execute_closure(ClosureExecutionRequest {
            mission_id: "mission-1".to_owned(),
            run_id: "run-1".to_owned(),
            repository_path: canonical.to_string_lossy().into_owned(),
            worktree_path: canonical.to_string_lossy().into_owned(),
            base_revision,
            declarations: vec![check],
        })
        .unwrap();

        assert_eq!(pack.summaries().get("unit"), Some(&EvidenceStatus::Passed));
        assert!(pack.records().contains_key("unit"));
        assert!(pack.created_at_millis() > 0);

        let failed = execute_closure(ClosureExecutionRequest {
            mission_id: "mission-2".to_owned(),
            run_id: "run-2".to_owned(),
            repository_path: canonical.to_string_lossy().into_owned(),
            worktree_path: canonical.to_string_lossy().into_owned(),
            base_revision: git(directory.path(), &["rev-parse", "HEAD"]),
            declarations: vec![CheckDeclaration::command(
                "failing",
                CommandSpec::new("./fail-check", [] as [&str; 0], "."),
                vec![PathRule::All],
                vec![],
            )],
        })
        .unwrap();
        assert_eq!(
            failed.summaries().get("failing"),
            Some(&EvidenceStatus::Failed)
        );
        assert!(failed.records().contains_key("failing"));
    }
}
