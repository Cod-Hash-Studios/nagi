use crate::mission::model::ProviderKind;

use super::{
    AdapterContractVersion, ManagedProviderError, ManagedProviderSupervisor, ProviderCapabilities,
    ProviderRuntimeVersion,
};

const FIRST_PARTY_PROVIDERS: [ProviderKind; 3] = [
    ProviderKind::Codex,
    ProviderKind::ClaudeCode,
    ProviderKind::OpenCode,
];

#[test]
fn registry_exposes_one_versioned_adapter_per_first_party_provider() {
    for provider in FIRST_PARTY_PROVIDERS {
        let descriptor =
            ManagedProviderSupervisor::descriptor(provider, AdapterContractVersion::CURRENT)
                .expect("the current first-party adapter contract must resolve");

        assert_eq!(descriptor.provider, provider);
        assert_eq!(descriptor.contract_version, AdapterContractVersion::CURRENT);
        assert_eq!(
            descriptor.capabilities,
            ProviderCapabilities {
                resume: true,
                turns: true,
                interrupt: true,
                permission_attention: true,
                question_attention: provider != ProviderKind::OpenCode,
                streaming_output: true,
                usage: false,
                diffs: false,
            }
        );
        assert_eq!(
            descriptor.runtime_version,
            match provider {
                ProviderKind::Codex | ProviderKind::ClaudeCode => {
                    ProviderRuntimeVersion::NotPinned
                }
                ProviderKind::OpenCode => ProviderRuntimeVersion::Exact("1.18.3"),
                ProviderKind::Acp => ProviderRuntimeVersion::NotPinned,
            }
        );
    }
}

#[test]
fn registry_rejects_unknown_adapter_contract_versions_without_guessing() {
    let requested = AdapterContractVersion::new(999);

    for provider in FIRST_PARTY_PROVIDERS {
        let error = ManagedProviderSupervisor::descriptor(provider, requested)
            .expect_err("an unknown adapter contract must fail closed");

        assert!(matches!(
            error,
            ManagedProviderError::UnsupportedAdapterContract {
                provider: rejected,
                requested: rejected_version,
                supported: AdapterContractVersion::CURRENT,
            } if rejected == provider && rejected_version == requested
        ));
        assert!(error.to_string().contains("adapter contract 999"));
        assert!(error.to_string().contains("supported contract is 1"));
    }
}

#[cfg(unix)]
mod lifecycle {
    use std::{
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        time::Duration,
    };

    use tokio::sync::mpsc;

    use super::*;
    use crate::managed_provider::{
        AttentionClass, ManagedProviderHandle, ProviderCommand, ProviderEvent, ProviderResponse,
        SandboxAccess, StartOrResume, TransportFailure, TurnOutcome,
    };

    const CODEX_FIXTURE: &str = include_str!("../../tests/fixtures/providers/codex.sh");
    const CLAUDE_FIXTURE: &str = include_str!("../../tests/fixtures/providers/claude.sh");
    const OPEN_CODE_FIXTURE: &str = include_str!("../../tests/fixtures/providers/opencode.py");
    const PROVIDER_EVENT_TIMEOUT: Duration = Duration::from_secs(30);

    fn provider_name(provider: ProviderKind) -> &'static str {
        match provider {
            ProviderKind::Codex => "codex",
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::OpenCode => "opencode",
            ProviderKind::Acp => "acp",
        }
    }

    fn install_fixture(directory: &Path, provider: ProviderKind, scenario: &str) -> PathBuf {
        let executable = directory.join(format!("{}-{scenario}", provider_name(provider)));
        let source = match provider {
            ProviderKind::Codex => CODEX_FIXTURE,
            ProviderKind::ClaudeCode => CLAUDE_FIXTURE,
            ProviderKind::OpenCode => OPEN_CODE_FIXTURE,
            ProviderKind::Acp => unreachable!("ACP uses its protocol-specific fixture suite"),
        };
        std::fs::write(&executable, source).expect("write provider fixture");
        let mut permissions = std::fs::metadata(&executable)
            .expect("read provider fixture metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions)
            .expect("make provider fixture executable");
        executable
    }

    fn start(
        provider: ProviderKind,
        executable: PathBuf,
        run_id: &str,
        resume_session_id: Option<&str>,
        initial_input: &str,
        cwd: &Path,
    ) -> (ManagedProviderHandle, mpsc::Receiver<ProviderEvent>) {
        let (event_tx, event_rx) = mpsc::channel(32);
        let handle = ManagedProviderSupervisor::spawn(provider, Some(executable), event_tx)
            .expect("registered provider adapter should spawn");
        handle
            .try_send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: run_id.to_owned(),
                cwd: cwd.to_path_buf(),
                resume_session_id: resume_session_id.map(str::to_owned),
                initial_input: initial_input.to_owned(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .expect("start command should enter the provider queue");
        (handle, event_rx)
    }

    async fn next_event(events: &mut mpsc::Receiver<ProviderEvent>) -> ProviderEvent {
        tokio::time::timeout(PROVIDER_EVENT_TIMEOUT, events.recv())
            .await
            .expect("provider event timeout")
            .expect("provider event channel closed")
    }

    #[tokio::test]
    async fn first_party_adapters_share_the_supported_managed_lifecycle() {
        for provider in FIRST_PARTY_PROVIDERS {
            let directory = tempfile::tempdir().expect("provider fixture directory");
            let executable = install_fixture(directory.path(), provider, "conformance");
            let run_id = format!("run-{}", provider_name(provider));
            let (handle, mut events) = start(
                provider,
                executable,
                &run_id,
                None,
                "first turn",
                directory.path(),
            );

            assert!(matches!(
                next_event(&mut events).await,
                ProviderEvent::Ready {
                    run_id: ref event_run,
                    ref session_id,
                } if event_run == &run_id && session_id == "session-live"
            ));
            let working = next_event(&mut events).await;
            assert!(
                matches!(
                    working,
                    ProviderEvent::Working {
                        run_id: ref event_run,
                        ..
                    } if event_run == &run_id
                ),
                "{provider:?} emitted {working:?} instead of Working"
            );

            let attention = match next_event(&mut events).await {
                ProviderEvent::AttentionRequested {
                    run_id: event_run,
                    attention,
                } => {
                    assert_eq!(event_run, run_id);
                    assert!(matches!(
                        attention.class,
                        AttentionClass::CommandApproval | AttentionClass::PermissionApproval
                    ));
                    assert!(!attention.requested_action.contains("must-not-escape"));
                    attention
                }
                other => panic!("{provider:?} emitted unexpected attention event: {other:?}"),
            };
            let response_request_id = attention.token.request_id().to_owned();
            handle
                .try_send(ProviderCommand::Respond {
                    token: attention.token,
                    response: ProviderResponse::Approve,
                })
                .expect("provider response should enter the command queue");
            assert_eq!(
                next_event(&mut events).await,
                ProviderEvent::ResponseResolved {
                    run_id: run_id.clone(),
                    request_id: response_request_id,
                }
            );
            assert!(matches!(
                next_event(&mut events).await,
                ProviderEvent::OutputDelta {
                    run_id: ref event_run,
                    ref text,
                    ..
                } if event_run == &run_id && text == "done"
            ));
            assert!(matches!(
                next_event(&mut events).await,
                ProviderEvent::TurnCompleted {
                    run_id: ref event_run,
                    outcome: TurnOutcome::Completed,
                    ..
                } if event_run == &run_id
            ));

            handle
                .try_send(ProviderCommand::SendTurn {
                    input: "second turn".to_owned(),
                })
                .expect("follow-up turn should enter the command queue");
            assert!(matches!(
                next_event(&mut events).await,
                ProviderEvent::Working {
                    run_id: ref event_run,
                    ..
                } if event_run == &run_id
            ));
            handle
                .try_send(ProviderCommand::Interrupt)
                .expect("interrupt should enter the command queue");
            assert!(matches!(
                next_event(&mut events).await,
                ProviderEvent::TurnCompleted {
                    run_id: ref event_run,
                    outcome: TurnOutcome::Interrupted,
                    ..
                } if event_run == &run_id
            ));

            handle
                .try_send(ProviderCommand::Shutdown)
                .expect("shutdown should enter the command queue");
            assert_eq!(
                next_event(&mut events).await,
                ProviderEvent::Stopped { run_id }
            );
        }
    }

    #[tokio::test]
    async fn first_party_adapters_resume_without_replaying_a_turn() {
        for provider in FIRST_PARTY_PROVIDERS {
            let directory = tempfile::tempdir().expect("provider fixture directory");
            let executable = install_fixture(directory.path(), provider, "resume");
            let run_id = format!("resume-{}", provider_name(provider));
            let (handle, mut events) = start(
                provider,
                executable,
                &run_id,
                Some("session-resumed"),
                "",
                directory.path(),
            );

            assert_eq!(
                next_event(&mut events).await,
                ProviderEvent::Ready {
                    run_id: run_id.clone(),
                    session_id: "session-resumed".to_owned(),
                }
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(200), events.recv())
                    .await
                    .is_err(),
                "{provider:?} replayed work after a resume with empty input"
            );
            handle
                .try_send(ProviderCommand::Shutdown)
                .expect("shutdown should enter the command queue");
            assert_eq!(
                next_event(&mut events).await,
                ProviderEvent::Stopped { run_id }
            );
        }
    }

    #[tokio::test]
    async fn first_party_adapters_report_disconnect_without_false_completion() {
        for provider in FIRST_PARTY_PROVIDERS {
            let directory = tempfile::tempdir().expect("provider fixture directory");
            let executable = install_fixture(directory.path(), provider, "disconnect");
            let run_id = format!("disconnect-{}", provider_name(provider));
            let (_handle, mut events) = start(
                provider,
                executable,
                &run_id,
                None,
                "disconnect now",
                directory.path(),
            );
            let mut completed = false;

            loop {
                let event = tokio::time::timeout(PROVIDER_EVENT_TIMEOUT, events.recv())
                    .await
                    .unwrap_or_else(|_| panic!("{provider:?} provider event timed out"))
                    .unwrap_or_else(|| panic!("{provider:?} provider event channel closed"));
                match event {
                    ProviderEvent::TurnCompleted {
                        outcome: TurnOutcome::Completed,
                        ..
                    } => completed = true,
                    ProviderEvent::TransportFailed {
                        run_id: event_run,
                        reason,
                    } if matches!(
                        reason,
                        TransportFailure::Disconnected | TransportFailure::DeliveryUnknown
                    ) =>
                    {
                        assert_eq!(event_run, run_id);
                        assert_eq!(
                            reason,
                            if provider == ProviderKind::OpenCode {
                                TransportFailure::DeliveryUnknown
                            } else {
                                TransportFailure::Disconnected
                            },
                            "a provider with an acknowledged in-flight HTTP turn must preserve delivery uncertainty"
                        );
                        break;
                    }
                    _ => {}
                }
            }
            assert!(
                !completed,
                "{provider:?} claimed completion after disconnect"
            );
        }
    }
}
