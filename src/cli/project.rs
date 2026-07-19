use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

#[derive(Serialize)]
struct DetectionReport {
    schema_version: u16,
    repository_path: String,
    recipe_id: String,
    recipe_label: String,
    proof_command: String,
    confidence: &'static str,
    project_contract: ContractStatus,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ContractStatus {
    Missing,
    Valid {
        services: usize,
        checks: usize,
        cleanup_commands: usize,
    },
    Invalid {
        error: String,
    },
}

pub(super) fn run_project_command(args: &[String]) -> std::io::Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(2);
    };
    if matches!(command, "help" | "--help" | "-h") {
        print_help();
        return Ok(0);
    }
    match command {
        "detect" | "validate" => {
            let Some((path, json)) = parse_path_and_json(&args[1..]) else {
                print_help();
                return Ok(2);
            };
            if command == "detect" {
                detect(&path, json)
            } else {
                validate(&path, json)
            }
        }
        "setup" => execute_setup(&args[1..]),
        "check" => execute_checks(&args[1..]),
        "cleanup" => execute_cleanup(&args[1..]),
        "services" => services(&args[1..]),
        "resources" => resources(&args[1..]),
        _ => {
            print_help();
            Ok(2)
        }
    }
}

fn contract(path: &Path) -> Result<crate::project_recipe::ProjectContract, i32> {
    match crate::project_recipe::load_contract(path) {
        Ok(Some(contract)) => Ok(contract),
        Ok(None) => {
            eprintln!("missing .nagi/project.toml under {}", path.display());
            Err(1)
        }
        Err(error) => {
            eprintln!("invalid .nagi/project.toml: {error}");
            Err(1)
        }
    }
}

fn execution_args(args: &[String]) -> Option<(PathBuf, bool, bool, Option<String>)> {
    let mut path = None;
    let mut yes = false;
    let mut json = false;
    let mut id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--yes" if !yes => yes = true,
            "--json" if !json => json = true,
            "--id" if id.is_none() => {
                index += 1;
                id = args.get(index).cloned();
                id.as_ref()?;
            }
            value if !value.starts_with('-') && path.is_none() => path = Some(PathBuf::from(value)),
            _ => return None,
        }
        index += 1;
    }
    Some((path.unwrap_or_else(|| PathBuf::from(".")), yes, json, id))
}

fn execute_setup(args: &[String]) -> std::io::Result<i32> {
    let Some((path, yes, json, id)) = execution_args(args) else {
        return Ok(2);
    };
    if id.is_some() {
        return Ok(2);
    }
    let Ok(contract) = contract(&path) else {
        return Ok(1);
    };
    let Some(setup) = &contract.setup else {
        println!("project recipe has no setup command");
        return Ok(0);
    };
    if !yes {
        eprintln!(
            "setup executes repository-owned code; inspect .nagi/project.toml and pass --yes"
        );
        return Ok(2);
    }
    print_results(vec![crate::project_recipe::run_setup(&path, setup)], json)
}

fn execute_checks(args: &[String]) -> std::io::Result<i32> {
    let Some((path, yes, json, requested_id)) = execution_args(args) else {
        return Ok(2);
    };
    let Ok(contract) = contract(&path) else {
        return Ok(1);
    };
    if !yes {
        eprintln!(
            "checks execute repository-owned code; inspect .nagi/project.toml and pass --yes"
        );
        return Ok(2);
    }
    let selected = contract
        .checks
        .iter()
        .filter(|check| requested_id.as_ref().is_none_or(|id| id == &check.id))
        .collect::<Vec<_>>();
    if let Some(requested_id) = requested_id.filter(|_| selected.is_empty()) {
        eprintln!("project check id not found: {requested_id}");
        return Ok(1);
    }
    print_results(
        selected
            .into_iter()
            .map(|check| crate::project_recipe::run_check(&path, check))
            .collect(),
        json,
    )
}

fn execute_cleanup(args: &[String]) -> std::io::Result<i32> {
    let Some((path, yes, json, id)) = execution_args(args) else {
        return Ok(2);
    };
    if id.is_some() || !yes {
        eprintln!(
            "cleanup executes repository-owned code; inspect .nagi/project.toml and pass --yes"
        );
        return Ok(2);
    }
    let Ok(contract) = contract(&path) else {
        return Ok(1);
    };
    print_results(
        contract
            .cleanup
            .iter()
            .enumerate()
            .map(|(index, cleanup)| crate::project_recipe::run_cleanup(&path, index, cleanup))
            .collect(),
        json,
    )
}

fn print_results(
    results: Vec<
        Result<
            crate::project_recipe::ProjectCommandResult,
            crate::mission::verifier::TrustedCheckError,
        >,
    >,
    json: bool,
) -> std::io::Result<i32> {
    let mut rendered = Vec::new();
    let mut success = true;
    for result in results {
        match result {
            Ok(result) => {
                success &= result.succeeded();
                rendered.push(result);
            }
            Err(error) => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &serde_json::json!({"ok": false, "error": error.to_string()})
                        )
                        .expect("command error is serializable")
                    );
                } else {
                    eprintln!("project command failed: {error}");
                }
                return Ok(1);
            }
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": success,
                "results": rendered,
            }))
            .expect("project results are serializable")
        );
    } else {
        for result in rendered {
            println!(
                "{}: {} ({} ms)",
                result.id,
                if result.succeeded() {
                    "passed"
                } else {
                    "failed"
                },
                result.duration_millis
            );
            if !result.stdout.is_empty() {
                print!("{}", result.stdout);
            }
            if !result.stderr.is_empty() {
                eprint!("{}", result.stderr);
            }
        }
    }
    Ok(if success { 0 } else { 1 })
}

fn services(args: &[String]) -> std::io::Result<i32> {
    let Some(action) = args.first().map(String::as_str) else {
        return Ok(2);
    };
    let Some(options) = service_args(&args[1..]) else {
        return Ok(2);
    };
    let allocator = crate::resources::ports::PortAllocator::open(
        &crate::config::state_dir().join("project-resources"),
    )
    .map_err(std::io::Error::other)?;
    match action {
        "start" => {
            if !options.yes {
                eprintln!("services execute repository-owned code; inspect .nagi/project.toml and pass --yes");
                return Ok(2);
            }
            let Ok(contract) = contract(&options.path) else {
                return Ok(1);
            };
            let set = crate::resources::services::ServiceSet::start(
                allocator,
                &contract,
                &options.path,
                &options.mission,
                &options.run,
                now_millis(),
            )
            .map_err(std::io::Error::other)?;
            let ports = set.detach();
            print_service_ports(&ports, options.json);
            Ok(0)
        }
        "status" => {
            let leases = allocator
                .leases_for_owner(&options.mission, &options.run)
                .map_err(std::io::Error::other)?;
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&leases).expect("leases are serializable")
                );
            } else if leases.is_empty() {
                println!("no services for {}/{}", options.mission, options.run);
            } else {
                for lease in leases {
                    println!(
                        "{}: 127.0.0.1:{} pid={} owner={}",
                        lease.service_id(),
                        lease.port(),
                        lease
                            .service_pid()
                            .map_or_else(|| "-".into(), |pid| pid.to_string()),
                        lease.owner_pid()
                    );
                }
            }
            Ok(0)
        }
        "stop" => {
            if !options.yes {
                eprintln!("stopping services requires --yes");
                return Ok(2);
            }
            let stopped = crate::resources::services::ServiceSet::stop_owner(
                &allocator,
                &options.mission,
                &options.run,
            )
            .map_err(std::io::Error::other)?;
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"stopped": stopped}))
                        .expect("service result is serializable")
                );
            } else {
                println!("stopped: {}", stopped.join(", "));
            }
            Ok(0)
        }
        _ => Ok(2),
    }
}

struct ServiceOptions {
    path: PathBuf,
    mission: String,
    run: String,
    yes: bool,
    json: bool,
}

fn service_args(args: &[String]) -> Option<ServiceOptions> {
    let mut path = None;
    let mut mission = None;
    let mut run = None;
    let mut yes = false;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--mission" if mission.is_none() => {
                index += 1;
                mission = args.get(index).cloned();
            }
            "--run" if run.is_none() => {
                index += 1;
                run = args.get(index).cloned();
            }
            "--yes" if !yes => yes = true,
            "--json" if !json => json = true,
            value if !value.starts_with('-') && path.is_none() => path = Some(PathBuf::from(value)),
            _ => return None,
        }
        index += 1;
    }
    Some(ServiceOptions {
        path: path.unwrap_or_else(|| PathBuf::from(".")),
        mission: mission?,
        run: run?,
        yes,
        json,
    })
}

fn print_service_ports(ports: &BTreeMap<String, u16>, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(ports).expect("ports are serializable")
        );
    } else if ports.is_empty() {
        println!("project recipe has no services");
    } else {
        for (service, port) in ports {
            println!("{service}: http://127.0.0.1:{port}");
        }
    }
}

fn resources(args: &[String]) -> std::io::Result<i32> {
    let Some(action) = args.first().map(String::as_str) else {
        return Ok(2);
    };
    let Some((json, yes, inspected_digest)) = resource_args(&args[1..]) else {
        return Ok(2);
    };
    let allocator = crate::resources::ports::PortAllocator::open(
        &crate::config::state_dir().join("project-resources"),
    )
    .map_err(std::io::Error::other)?;
    let preview = crate::resources::cleanup::preview(&allocator).map_err(std::io::Error::other)?;
    match action {
        "preview" => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&preview).expect("preview is serializable")
                );
            } else {
                println!("cleanup digest: {}", preview.digest);
                println!("resource registry: {} bytes", preview.registry_bytes);
                println!("orphaned ports: {}", preview.orphaned_ports.len());
                for lease in preview.orphaned_ports {
                    println!(
                        "{} {}/{} {} port {}",
                        lease.lease_id,
                        lease.mission_id,
                        lease.run_id,
                        lease.service_id,
                        lease.port
                    );
                }
            }
            Ok(0)
        }
        "apply" if yes && inspected_digest.as_deref() == Some(preview.digest.as_str()) => {
            let removed = crate::resources::cleanup::apply(&allocator, &preview)
                .map_err(std::io::Error::other)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"removed": removed}))
                        .expect("cleanup result is serializable")
                );
            } else {
                println!("removed {} orphaned port leases", removed.len());
            }
            Ok(0)
        }
        "apply" if inspected_digest.is_none() => {
            eprintln!("resource cleanup requires --digest <preview-digest> and --yes");
            Ok(2)
        }
        "apply" if !yes => {
            eprintln!("resource cleanup requires --yes after reviewing the preview");
            Ok(2)
        }
        "apply" => {
            eprintln!("resource cleanup preview changed; run preview again before applying");
            Ok(2)
        }
        _ => Ok(2),
    }
}

fn resource_args(args: &[String]) -> Option<(bool, bool, Option<String>)> {
    let mut json = false;
    let mut yes = false;
    let mut digest = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" if !json => json = true,
            "--yes" if !yes => yes = true,
            "--digest" if digest.is_none() => {
                index += 1;
                let value = args.get(index)?;
                if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                digest = Some(value.to_ascii_lowercase());
            }
            _ => return None,
        }
        index += 1;
    }
    Some((json, yes, digest))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn parse_path_and_json(args: &[String]) -> Option<(PathBuf, bool)> {
    let mut path = None;
    let mut json = false;
    for argument in args {
        if argument == "--json" {
            if json {
                return None;
            }
            json = true;
        } else if argument.starts_with('-') || path.is_some() {
            return None;
        } else {
            path = Some(PathBuf::from(argument));
        }
    }
    Some((path.unwrap_or_else(|| PathBuf::from(".")), json))
}

fn detect(path: &Path, json: bool) -> std::io::Result<i32> {
    let repository = match std::fs::canonicalize(path) {
        Ok(repository) if repository.is_dir() => repository,
        Ok(_) => {
            eprintln!("project path is not a directory: {}", path.display());
            return Ok(1);
        }
        Err(error) => {
            eprintln!("project path is unavailable at {}: {error}", path.display());
            return Ok(1);
        }
    };
    let recipe = crate::project_recipe::detect(&repository);
    let project_contract = match crate::project_recipe::load_contract(&repository) {
        Ok(None) => ContractStatus::Missing,
        Ok(Some(contract)) => ContractStatus::Valid {
            services: contract.services.len(),
            checks: contract.checks.len(),
            cleanup_commands: contract.cleanup.len(),
        },
        Err(error) => ContractStatus::Invalid {
            error: error.to_string(),
        },
    };
    let report = DetectionReport {
        schema_version: crate::project_recipe::PROJECT_SCHEMA_V1,
        repository_path: repository.to_string_lossy().into_owned(),
        recipe_id: recipe.id.to_owned(),
        recipe_label: recipe.label.to_owned(),
        proof_command: recipe.command_line,
        confidence: match recipe.confidence {
            crate::project_recipe::RecipeConfidence::ProjectTest => "project_test",
            crate::project_recipe::RecipeConfidence::BaselineOnly => "baseline_only",
        },
        project_contract,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("detection report is serializable")
        );
    } else {
        println!("Project: {}", report.repository_path);
        println!("Detected: {} ({})", report.recipe_label, report.confidence);
        println!("Suggested proof: {}", report.proof_command);
        match &report.project_contract {
            ContractStatus::Missing => println!("Contract: missing (.nagi/project.toml)"),
            ContractStatus::Valid {
                services,
                checks,
                cleanup_commands,
            } => println!(
                "Contract: valid ({services} services, {checks} checks, {cleanup_commands} cleanup commands)"
            ),
            ContractStatus::Invalid { error } => println!("Contract: invalid ({error})"),
        }
    }
    Ok(0)
}

fn validate(path: &Path, json: bool) -> std::io::Result<i32> {
    match crate::project_recipe::load_contract(path) {
        Ok(Some(contract)) => {
            if json {
                let value = serde_json::json!({
                    "valid": true,
                    "schema_version": contract.schema,
                    "services": contract.services.len(),
                    "checks": contract.checks.len(),
                    "cleanup_commands": contract.cleanup.len(),
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value)
                        .expect("validation report is serializable")
                );
            } else {
                println!(
                    "valid .nagi/project.toml: schema {}, {} services, {} checks, {} cleanup commands",
                    contract.schema,
                    contract.services.len(),
                    contract.checks.len(),
                    contract.cleanup.len()
                );
            }
            Ok(0)
        }
        Ok(None) => {
            eprintln!("missing .nagi/project.toml under {}", path.display());
            Ok(1)
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "valid": false,
                        "error": error.to_string(),
                    }))
                    .expect("validation error is serializable")
                );
            } else {
                eprintln!("invalid .nagi/project.toml: {error}");
            }
            Ok(1)
        }
    }
}

fn print_help() {
    eprintln!("nagi project commands:");
    eprintln!("  nagi project detect [PATH] [--json]");
    eprintln!("  nagi project validate [PATH] [--json]");
    eprintln!("  nagi project setup [PATH] --yes [--json]");
    eprintln!("  nagi project check [PATH] [--id ID] --yes [--json]");
    eprintln!("  nagi project cleanup [PATH] --yes [--json]");
    eprintln!(
        "  nagi project services <start|status|stop> [PATH] --mission ID --run ID [--yes] [--json]"
    );
    eprintln!("  nagi project resources preview [--json]");
    eprintln!("  nagi project resources apply --digest DIGEST --yes [--json]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_and_json_parser_is_order_independent_and_bounded() {
        assert_eq!(
            parse_path_and_json(&["repo".into(), "--json".into()]),
            Some((PathBuf::from("repo"), true))
        );
        assert_eq!(
            parse_path_and_json(&["--json".into(), "repo".into()]),
            Some((PathBuf::from("repo"), true))
        );
        assert!(parse_path_and_json(&["a".into(), "b".into()]).is_none());
        assert!(parse_path_and_json(&["--wat".into()]).is_none());
    }

    #[test]
    fn cleanup_digest_parser_requires_one_exact_sha256() {
        let digest = "a".repeat(64);
        assert_eq!(
            resource_args(&["--digest".into(), digest.clone(), "--yes".into()]),
            Some((false, true, Some(digest)))
        );
        assert!(resource_args(&["--digest".into(), "short".into()]).is_none());
        assert!(resource_args(&["--yes".into(), "--yes".into()]).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn setup_cli_requires_consent_and_executes_the_validated_contract() {
        use std::os::unix::fs::PermissionsExt as _;

        let repository = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repository.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(repository.path().join(".nagi")).unwrap();
        std::fs::write(
            repository.path().join(".nagi/project.toml"),
            "schema = 1\n[setup]\ncommand = [\"./setup\"]\ntimeout_seconds = 2\n",
        )
        .unwrap();
        let script = repository.path().join("setup");
        std::fs::write(&script, "#!/bin/sh\nprintf done > setup-result\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = repository.path().to_string_lossy().into_owned();

        assert_eq!(
            run_project_command(&["setup".into(), path.clone()]).unwrap(),
            2
        );
        assert!(!repository.path().join("setup-result").exists());
        assert_eq!(
            run_project_command(&["setup".into(), path, "--yes".into()]).unwrap(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(repository.path().join("setup-result")).unwrap(),
            "done"
        );
    }
}
