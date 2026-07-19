use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

fn write_plugin(root: &std::path::Path, id: &str, wat: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("component.wasm"), wat::parse_str(wat).unwrap()).unwrap();
    fs::write(
        root.join("nagi-plugin.toml"),
        format!(
            "manifest_version = 2\nid = \"{id}\"\nname = \"Isolation fixture\"\nversion = \"0.1.0\"\nmin_nagi_version = \"{}\"\nruntime = \"wasi-component\"\nentrypoint = \"component.wasm\"\ncapabilities = []\n",
            env!("CARGO_PKG_VERSION")
        ),
    )
    .unwrap();
}

fn run_plugin(
    path: &std::path::Path,
    config_home: &std::path::Path,
) -> (std::process::ExitStatus, serde_json::Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_nagi"))
        .args(["plugin", "test"])
        .arg(path)
        .args(["--json"])
        .env("XDG_CONFIG_HOME", config_home)
        .env_remove("NAGI_ENV")
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "plugin test output was not JSON: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status, value)
}

#[test]
fn hostile_component_is_trapped_and_the_next_plugin_still_runs() {
    let temp = tempfile::tempdir().unwrap();
    let hostile = temp.path().join("hostile");
    write_plugin(
        &hostile,
        "test.hostile",
        r#"
            (component
                (core module $module
                    (func (export "run") (result i32)
                        (loop $forever br $forever)
                        i32.const 0))
                (core instance $instance (instantiate $module))
                (type $result (result))
                (type $run (func (result $result)))
                (func $run (type $run) (canon lift (core func $instance "run")))
                (instance $exports (export "run" (func $run)))
                (export "wasi:cli/run@0.2.0" (instance $exports)))
        "#,
    );
    let started = Instant::now();
    let (status, hostile_result) = run_plugin(&hostile, &temp.path().join("config"));
    assert!(!status.success(), "{hostile_result}");
    assert_eq!(hostile_result["passed"], false);
    assert!(
        hostile_result["error"]
            .as_str()
            .is_some_and(|error| error.contains("trapped")),
        "{hostile_result}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    let healthy = temp.path().join("healthy");
    write_plugin(
        &healthy,
        "test.healthy",
        r#"
            (component
                (core module $module (func (export "run") (result i32) i32.const 0))
                (core instance $instance (instantiate $module))
                (type $result (result))
                (type $run (func (result $result)))
                (func $run (type $run) (canon lift (core func $instance "run")))
                (instance $exports (export "run" (func $run)))
                (export "wasi:cli/run@0.2.0" (instance $exports)))
        "#,
    );
    let (status, healthy_result) = run_plugin(&healthy, &temp.path().join("config"));
    assert!(status.success(), "{healthy_result}");
    assert_eq!(healthy_result["passed"], true);
    assert_eq!(healthy_result["exit_code"], 0);
}
