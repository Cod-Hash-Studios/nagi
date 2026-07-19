use crate::api::schema::{
    EmptyParams, Method, MissionHandoffPreviewParams, MissionHandoffStartParams, MissionProvider,
    MissionTarget, Request,
};

pub(super) fn run_mission_command(args: &[String]) -> std::io::Result<i32> {
    match args {
        [command] if command == "list" => print_method_response(
            "cli:mission:list",
            Method::MissionList(EmptyParams::default()),
        ),
        [command, mission_id] if command == "get" => print_method_response(
            "cli:mission:get",
            Method::MissionGet(MissionTarget {
                mission_id: mission_id.clone(),
            }),
        ),
        [command, mission_id] if command == "proof" => print_method_response(
            "cli:mission:proof",
            Method::MissionProofGet(MissionTarget {
                mission_id: mission_id.clone(),
            }),
        ),
        [command, mission_id] if command == "close" => print_method_response(
            "cli:mission:close",
            Method::MissionClose(MissionTarget {
                mission_id: mission_id.clone(),
            }),
        ),
        [command, mission_id, to_flag, provider, preview]
            if command == "handoff" && to_flag == "--to" && preview == "--preview" =>
        {
            let Some(to) = parse_provider(provider) else {
                eprintln!("invalid provider: expected codex, claude-code, opencode, or acp");
                return Ok(2);
            };
            print_method_response(
                "cli:mission:handoff:preview",
                Method::MissionHandoffPreview(MissionHandoffPreviewParams {
                    mission_id: mission_id.clone(),
                    to,
                }),
            )
        }
        [command, mission_id, to_flag, provider, start, artifact_flag, artifact_sha256, generated_at_flag, generated_at_millis]
            if command == "handoff"
                && to_flag == "--to"
                && start == "--start"
                && artifact_flag == "--artifact-sha256"
                && generated_at_flag == "--generated-at-millis" =>
        {
            let Some(to) = parse_provider(provider) else {
                eprintln!("invalid provider: expected codex, claude-code, opencode, or acp");
                return Ok(2);
            };
            let Some((artifact_sha256, generated_at_millis)) =
                parse_handoff_binding(artifact_sha256, generated_at_millis)
            else {
                eprintln!(
                    "invalid handoff binding: use the 64-character digest and timestamp from the inspected preview"
                );
                return Ok(2);
            };
            print_method_response(
                "cli:mission:handoff:start",
                Method::MissionHandoffStart(MissionHandoffStartParams {
                    mission_id: mission_id.clone(),
                    to,
                    generated_at_millis,
                    artifact_sha256,
                }),
            )
        }
        [command] if matches!(command.as_str(), "help" | "--help" | "-h") => {
            print_help();
            Ok(0)
        }
        _ => {
            print_help();
            Ok(2)
        }
    }
}

fn parse_handoff_binding(digest: &str, generated_at_millis: &str) -> Option<(String, u64)> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let generated_at_millis = generated_at_millis.parse::<u64>().ok()?;
    Some((digest.to_owned(), generated_at_millis))
}

fn parse_provider(provider: &str) -> Option<MissionProvider> {
    match provider {
        "codex" => Some(MissionProvider::Codex),
        "claude-code" => Some(MissionProvider::ClaudeCode),
        "opencode" => Some(MissionProvider::OpenCode),
        "acp" => Some(MissionProvider::Acp),
        _ => None,
    }
}

fn print_method_response(id: &'static str, method: Method) -> std::io::Result<i32> {
    super::print_response(&super::send_request(&Request {
        id: id.into(),
        method,
    })?)
}

fn print_help() {
    eprintln!("nagi mission commands:");
    eprintln!("  nagi mission list");
    eprintln!("  nagi mission get <mission_id>");
    eprintln!("  nagi mission proof <mission_id>");
    eprintln!("  nagi mission close <mission_id>");
    eprintln!(
        "  nagi mission handoff <mission_id> --to <codex|claude-code|opencode|acp> --preview"
    );
    eprintln!(
        "  nagi mission handoff <mission_id> --to <provider> --start --artifact-sha256 <sha256> --generated-at-millis <timestamp>"
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_mission_command_is_usage_error_without_connecting() {
        let args = vec!["proof".to_owned()];
        assert_eq!(super::run_mission_command(&args).unwrap(), 2);
    }

    #[test]
    fn help_does_not_connect() {
        let args = vec!["--help".to_owned()];
        assert_eq!(super::run_mission_command(&args).unwrap(), 0);
    }

    #[test]
    fn handoff_provider_parser_is_explicit() {
        assert_eq!(
            super::parse_provider("claude-code"),
            Some(crate::api::schema::MissionProvider::ClaudeCode)
        );
        assert_eq!(super::parse_provider("claude"), None);
    }

    #[test]
    fn handoff_binding_accepts_only_the_exact_preview_identity() {
        assert_eq!(
            super::parse_handoff_binding(&"a".repeat(64), "42"),
            Some(("a".repeat(64), 42))
        );
        assert!(super::parse_handoff_binding(&"A".repeat(64), "42").is_none());
        assert!(super::parse_handoff_binding(&"a".repeat(63), "42").is_none());
        assert!(super::parse_handoff_binding(&"a".repeat(64), "later").is_none());
    }
}
