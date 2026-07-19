use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub struct HeadlessHarness {
    root: tempfile::TempDir,
    config_home: PathBuf,
    state_home: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
    path_prefix: Option<PathBuf>,
    child: Option<Child>,
}

impl HeadlessHarness {
    pub fn start(path_prefix: Option<&Path>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("nagi-headless-chaos-")
            .tempdir()
            .unwrap();
        let config_home = root.path().join("config");
        let state_home = root.path().join("state");
        let runtime_dir = root.path().join("runtime");
        let socket = runtime_dir.join("nagi.sock");
        fs::create_dir_all(config_home.join("nagi")).unwrap();
        fs::create_dir_all(&state_home).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(config_home.join("nagi/config.toml"), "onboarding = false\n").unwrap();
        let mut harness = Self {
            root,
            config_home,
            state_home,
            runtime_dir,
            socket,
            path_prefix: path_prefix.map(Path::to_path_buf),
            child: None,
        };
        harness.spawn();
        harness
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn config_home(&self) -> &Path {
        &self.config_home
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn diagnostics(&self) -> String {
        ["server.stdout.log", "server.stderr.log"]
            .into_iter()
            .map(|name| {
                let contents = fs::read_to_string(self.root.path().join(name)).unwrap_or_default();
                format!("{name}:\n{contents}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn request(&self, request: serde_json::Value) -> serde_json::Value {
        let mut stream = UnixStream::connect(&self.socket).expect("connect to Nagi API socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        serde_json::from_str(response.trim()).expect("Nagi API response must be JSON")
    }

    pub fn kill_hard(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn restart(&mut self) {
        self.kill_hard();
        self.spawn();
    }

    fn spawn(&mut self) {
        let stdout = fs::File::create(self.root.path().join("server.stdout.log")).unwrap();
        let stderr = fs::File::create(self.root.path().join("server.stderr.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_nagi"));
        command
            .arg("server")
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("NAGI_SOCKET_PATH", &self.socket)
            .env("SHELL", "/bin/sh")
            .env_remove("NAGI_CLIENT_SOCKET_PATH")
            .env_remove("NAGI_ENV")
            .env_remove("NAGI_SESSION")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(prefix) = &self.path_prefix {
            let mut paths = vec![prefix.clone()];
            if let Some(current) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&current));
            }
            let joined: OsString = std::env::join_paths(paths).unwrap();
            command.env("PATH", joined);
        }
        self.child = Some(command.spawn().expect("spawn headless Nagi server"));
        wait_until(Duration::from_secs(10), || {
            self.socket.exists() && UnixStream::connect(&self.socket).is_ok()
        });
    }
}

impl Drop for HeadlessHarness {
    fn drop(&mut self) {
        self.kill_hard();
    }
}

pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        condition(),
        "condition did not become true within {timeout:?}"
    );
}

pub fn initialize_git_repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    for arguments in [
        vec!["init", "-q"],
        vec!["config", "user.name", "Nagi Test"],
        vec!["config", "user.email", "nagi@example.invalid"],
    ] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success());
    }
    fs::write(path.join("README.md"), "fixture\n").unwrap();
    for arguments in [vec!["add", "README.md"], vec!["commit", "-qm", "fixture"]] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
