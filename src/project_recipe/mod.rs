use std::path::Path;

mod copy;
mod execute;
mod model;
mod validate;

pub(crate) use copy::copy_declared_ignored_files;
pub(crate) use execute::{run_check, run_cleanup, run_setup, ProjectCommandResult};
pub(crate) use model::{
    CheckContract, CommandContract, ProjectContract, ServiceContract, PROJECT_SCHEMA_V1,
};
pub(crate) use validate::load_contract;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectRecipe {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) command_line: String,
    pub(crate) confidence: RecipeConfidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecipeConfidence {
    ProjectTest,
    BaselineOnly,
}

pub(crate) fn detect(repository: &Path) -> ProjectRecipe {
    if repository.join("Cargo.toml").is_file() {
        return recipe("rust", "Rust", "cargo test", RecipeConfidence::ProjectTest);
    }
    if repository.join("go.mod").is_file() {
        return recipe("go", "Go", "go test ./...", RecipeConfidence::ProjectTest);
    }
    if repository.join("build.zig").is_file() {
        return recipe(
            "zig",
            "Zig",
            "zig build test",
            RecipeConfidence::ProjectTest,
        );
    }
    if repository.join("pyproject.toml").is_file()
        || repository.join("pytest.ini").is_file()
        || repository.join("setup.cfg").is_file()
    {
        return recipe(
            "python",
            "Python",
            "python -m pytest",
            RecipeConfidence::ProjectTest,
        );
    }
    if repository.join("package.json").is_file() && package_has_test_script(repository) {
        let (id, label, command) =
            if repository.join("bun.lock").is_file() || repository.join("bun.lockb").is_file() {
                ("bun", "Bun", "bun test")
            } else if repository.join("pnpm-lock.yaml").is_file() {
                ("pnpm", "pnpm", "pnpm test")
            } else if repository.join("yarn.lock").is_file() {
                ("yarn", "Yarn", "yarn test")
            } else {
                ("npm", "npm", "npm test")
            };
        return recipe(id, label, command, RecipeConfidence::ProjectTest);
    }

    recipe(
        "git-baseline",
        "Git baseline",
        "git diff --check",
        RecipeConfidence::BaselineOnly,
    )
}

fn recipe(
    id: &'static str,
    label: &'static str,
    command_line: &str,
    confidence: RecipeConfidence,
) -> ProjectRecipe {
    ProjectRecipe {
        id,
        label,
        command_line: command_line.to_owned(),
        confidence,
    }
}

fn package_has_test_script(repository: &Path) -> bool {
    let Ok(bytes) = std::fs::read(repository.join("package.json")) else {
        return false;
    };
    let Ok(package) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    package
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|script| {
            let normalized = script.trim();
            !normalized.is_empty()
                && !normalized.contains("Error: no test specified")
                && normalized != "exit 1"
        })
}

pub(crate) fn parse_command_line(input: &str) -> Result<(String, Vec<String>), &'static str> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in input.trim().chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                word.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            character if character.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(character),
        }
    }
    if escaped || quote.is_some() {
        return Err("proof command has an unfinished quote or escape");
    }
    if !word.is_empty() {
        words.push(word);
    }
    if words.is_empty() {
        return Err("proof command cannot be empty");
    }
    if words.iter().any(|word| {
        matches!(
            word.as_str(),
            "|" | "||" | "&&" | ";" | ">" | ">>" | "<" | "2>" | "2>&1"
        )
    }) {
        return Err("proof commands use argv only; shell operators are not allowed");
    }
    let program = words.remove(0);
    Ok((program, words))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_recipe_is_detected_without_running_project_code() {
        let repository = tempfile::tempdir().unwrap();
        std::fs::write(
            repository.path().join("Cargo.toml"),
            "[package]\nname='x'\n",
        )
        .unwrap();
        let detected = detect(repository.path());
        assert_eq!(detected.id, "rust");
        assert_eq!(detected.command_line, "cargo test");
        assert_eq!(detected.confidence, RecipeConfidence::ProjectTest);
    }

    #[test]
    fn node_recipe_requires_a_real_test_script() {
        let repository = tempfile::tempdir().unwrap();
        std::fs::write(
            repository.path().join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(
            repository.path().join("pnpm-lock.yaml"),
            "lockfileVersion: 9",
        )
        .unwrap();
        assert_eq!(detect(repository.path()).command_line, "pnpm test");

        std::fs::write(
            repository.path().join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        assert_eq!(detect(repository.path()).id, "git-baseline");
    }

    #[test]
    fn command_parser_preserves_quoted_argv_without_a_shell() {
        assert_eq!(
            parse_command_line("cargo test --package 'nagi core'").unwrap(),
            (
                "cargo".to_owned(),
                vec![
                    "test".to_owned(),
                    "--package".to_owned(),
                    "nagi core".to_owned()
                ]
            )
        );
        assert!(parse_command_line("cargo test && touch nope").is_err());
        assert!(parse_command_line("cargo test '").is_err());
    }
}
