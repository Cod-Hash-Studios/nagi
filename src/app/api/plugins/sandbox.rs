use std::{
    path::Path,
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};

use wasmtime::{
    component::{Component, Linker, ResourceTable},
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
};
use wasmtime_wasi::{
    p2::{
        bindings::sync::Command,
        pipe::{MemoryInputPipe, MemoryOutputPipe},
    },
    DirPerms, FilePerms, WasiCtx, WasiCtxView, WasiView,
};

const MAX_COMPONENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINEAR_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TABLE_ELEMENTS: usize = 10_000;
const MAX_INSTANCES: usize = 100;
const MAX_MEMORIES: usize = 16;
const MAX_TABLES: usize = 16;
const MAX_HOSTCALL_BYTES: usize = 1024 * 1024;
const DEFAULT_FUEL: u64 = 50_000_000;
pub(super) const SANDBOX_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(crate) struct SandboxInvocation<'a> {
    pub(super) component_path: &'a Path,
    pub(super) plugin_id: &'a str,
    pub(super) action_id: Option<&'a str>,
    pub(super) context_json: &'a str,
    pub(super) stdin: &'a [u8],
    pub(super) workspace: Option<SandboxWorkspaceMount<'a>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SandboxWorkspaceMount<'a> {
    pub(super) host_path: &'a Path,
    pub(super) readable: bool,
    pub(super) writable: bool,
}

#[derive(Debug)]
pub(crate) struct SandboxExecution {
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) error: Option<String>,
}

struct SandboxState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for SandboxState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

pub(crate) fn execute(invocation: SandboxInvocation<'_>, timeout: Duration) -> SandboxExecution {
    match execute_inner(invocation, timeout) {
        Ok(execution) => execution,
        Err(error) => SandboxExecution {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(error),
        },
    }
}

fn execute_inner(
    invocation: SandboxInvocation<'_>,
    timeout: Duration,
) -> Result<SandboxExecution, String> {
    let (engine, component) = load_component(invocation.component_path)?;
    let mut linker = Linker::<SandboxState>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|error| format!("sandbox WASI linker failed: {error}"))?;

    let stdout = MemoryOutputPipe::new(MAX_OUTPUT_BYTES);
    let stderr = MemoryOutputPipe::new(MAX_OUTPUT_BYTES);
    let stdin = MemoryInputPipe::new(invocation.stdin.to_vec());
    let mut wasi = WasiCtx::builder();
    wasi.stdin(stdin)
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .env("NAGI_ENV", "1")
        .env("NAGI_PLUGIN_ID", invocation.plugin_id)
        .env("NAGI_PLUGIN_CONTEXT_JSON", invocation.context_json);
    if let Some(action_id) = invocation.action_id {
        wasi.env("NAGI_PLUGIN_ACTION_ID", action_id);
    }
    if let Some(workspace) = invocation.workspace {
        let (dir_perms, file_perms) = workspace_permissions(workspace.readable, workspace.writable);
        wasi.preopened_dir(workspace.host_path, "/workspace", dir_perms, file_perms)
            .map_err(|error| format!("sandbox workspace mount failed: {error}"))?;
        wasi.env("NAGI_WORKSPACE_DIR", "/workspace");
    }
    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_LINEAR_MEMORY_BYTES)
        .table_elements(MAX_TABLE_ELEMENTS)
        .instances(MAX_INSTANCES)
        .memories(MAX_MEMORIES)
        .tables(MAX_TABLES)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(
        &engine,
        SandboxState {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limits,
        },
    );
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(DEFAULT_FUEL)
        .map_err(|error| format!("sandbox fuel configuration failed: {error}"))?;
    store.set_hostcall_fuel(MAX_HOSTCALL_BYTES);
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();

    let (finished_tx, finished_rx) = mpsc::channel();
    let timeout_engine = engine.clone();
    let watchdog = std::thread::spawn(move || {
        if finished_rx.recv_timeout(timeout).is_err() {
            timeout_engine.increment_epoch();
        }
    });
    let started = Instant::now();
    let result = Command::instantiate(&mut store, &component, &linker)
        .and_then(|command| command.wasi_cli_run().call_run(&mut store));
    let _ = finished_tx.send(());
    let _ = watchdog.join();

    let stdout = String::from_utf8_lossy(&stdout.contents()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr.contents()).into_owned();
    match result {
        Ok(Ok(())) => Ok(SandboxExecution {
            exit_code: Some(0),
            stdout,
            stderr,
            error: None,
        }),
        Ok(Err(())) => Ok(SandboxExecution {
            exit_code: Some(1),
            stdout,
            stderr,
            error: Some("sandbox component returned failure".to_owned()),
        }),
        Err(error) => {
            let reason = if started.elapsed() >= timeout {
                format!(
                    "sandbox component timed out after {}ms",
                    timeout.as_millis()
                )
            } else {
                format!("sandbox component trapped: {error}")
            };
            Ok(SandboxExecution {
                exit_code: None,
                stdout,
                stderr,
                error: Some(reason),
            })
        }
    }
}

fn workspace_permissions(readable: bool, writable: bool) -> (DirPerms, FilePerms) {
    let mut dir_perms = DirPerms::empty();
    let mut file_perms = FilePerms::empty();
    if readable || writable {
        // WASI needs directory traversal rights to resolve a path for creation.
        // File contents remain unreadable unless the read capability is present.
        dir_perms |= DirPerms::READ;
    }
    if readable {
        file_perms |= FilePerms::READ;
    }
    if writable {
        dir_perms |= DirPerms::MUTATE;
        file_perms |= FilePerms::WRITE;
    }
    (dir_perms, file_perms)
}

pub(crate) fn validate_component(path: &Path) -> Result<(), String> {
    load_component(path).map(|_| ())
}

fn load_component(path: &Path) -> Result<(Engine, Component), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("sandbox component is unavailable: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_COMPONENT_BYTES {
        return Err(format!(
            "sandbox component must be a non-empty file no larger than {MAX_COMPONENT_BYTES} bytes"
        ));
    }
    let engine = sandbox_engine()?;
    let component = Component::from_file(&engine, path)
        .map_err(|error| format!("sandbox component validation failed: {error}"))?;
    Ok((engine, component))
}

fn sandbox_engine() -> Result<Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config
                .wasm_component_model(true)
                .consume_fuel(true)
                .epoch_interruption(true)
                .cranelift_nan_canonicalization(true);
            Engine::new(&config)
                .map_err(|error| format!("sandbox engine configuration failed: {error}"))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_component(name: &str, wat: &str) -> tempfile::TempDir {
        let directory = tempfile::Builder::new()
            .prefix(&format!("nagi-sandbox-{name}-"))
            .tempdir()
            .unwrap();
        let bytes = wat::parse_str(wat).unwrap();
        std::fs::write(directory.path().join("plugin.wasm"), bytes).unwrap();
        directory
    }

    fn invocation(path: &Path) -> SandboxInvocation<'_> {
        SandboxInvocation {
            component_path: path,
            plugin_id: "example.sandbox",
            action_id: Some("run"),
            context_json: r#"{"source":"test"}"#,
            stdin: b"fixture input",
            workspace: None,
        }
    }

    #[test]
    fn executes_a_minimal_wasi_command_component() {
        let directory = write_component(
            "success",
            r#"
                (component
                    (core module $module
                        (func (export "run") (result i32)
                            i32.const 0))
                    (core instance $instance (instantiate $module))
                    (type $result (result))
                    (type $run (func (result $result)))
                    (func $run (type $run)
                        (canon lift (core func $instance "run")))
                    (instance $exports
                        (export "run" (func $run)))
                    (export "wasi:cli/run@0.2.0" (instance $exports)))
            "#,
        );
        let result = execute(
            invocation(&directory.path().join("plugin.wasm")),
            Duration::from_secs(2),
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout, "");
        assert_eq!(result.stderr, "");
        assert_eq!(result.error, None);
    }

    #[test]
    fn component_loads_share_the_process_sandbox_engine() {
        let directory = write_component(
            "shared-engine",
            r#"
                (component
                    (core module $module
                        (func (export "run") (result i32)
                            i32.const 0))
                    (core instance $instance (instantiate $module))
                    (type $result (result))
                    (type $run (func (result $result)))
                    (func $run (type $run)
                        (canon lift (core func $instance "run")))
                    (instance $exports
                        (export "run" (func $run)))
                    (export "wasi:cli/run@0.2.0" (instance $exports)))
            "#,
        );
        let path = directory.path().join("plugin.wasm");

        let (first, _) = load_component(&path).unwrap();
        let (second, _) = load_component(&path).unwrap();

        assert!(Engine::same(&first, &second));
    }

    #[test]
    fn rejects_invalid_components_without_panicking() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plugin.wasm");
        std::fs::write(&path, b"not wasm").unwrap();

        let result = execute(invocation(&path), Duration::from_millis(100));

        assert_eq!(result.exit_code, None);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("validation failed")),
            "{result:?}"
        );
    }

    #[test]
    fn fuel_stops_a_non_terminating_component() {
        let directory = write_component(
            "fuel",
            r#"
                (component
                    (core module $module
                        (func (export "run") (result i32)
                            (loop $forever
                                br $forever)
                            i32.const 0))
                    (core instance $instance (instantiate $module))
                    (type $result (result))
                    (type $run (func (result $result)))
                    (func $run (type $run)
                        (canon lift (core func $instance "run")))
                    (instance $exports
                        (export "run" (func $run)))
                    (export "wasi:cli/run@0.2.0" (instance $exports)))
            "#,
        );

        let result = execute(
            invocation(&directory.path().join("plugin.wasm")),
            Duration::from_secs(2),
        );

        assert_eq!(result.exit_code, None);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("trapped")),
            "{result:?}"
        );
    }

    #[test]
    fn rejects_empty_and_oversized_components_before_compilation() {
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("empty.wasm");
        std::fs::write(&empty, []).unwrap();
        let empty_result = execute(invocation(&empty), Duration::from_millis(100));
        assert!(
            empty_result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("non-empty file")),
            "{empty_result:?}"
        );

        let oversized = directory.path().join("oversized.wasm");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_COMPONENT_BYTES + 1).unwrap();
        let oversized_result = execute(invocation(&oversized), Duration::from_millis(100));
        assert!(
            oversized_result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no larger than")),
            "{oversized_result:?}"
        );
    }

    #[test]
    fn write_only_workspace_access_can_resolve_paths_without_reading_files() {
        let (dir_perms, file_perms) = workspace_permissions(false, true);

        assert!(dir_perms.contains(DirPerms::READ));
        assert!(dir_perms.contains(DirPerms::MUTATE));
        assert!(file_perms.contains(FilePerms::WRITE));
        assert!(!file_perms.contains(FilePerms::READ));
    }
}
