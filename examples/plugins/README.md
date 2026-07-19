# Reference plugins

These plugins exercise the capabilities Nagi actually binds today. They do not bypass the sandbox or pretend unavailable network/process capabilities exist.

- `github-lifecycle`: summarizes existing mission checks and evidence for PR/CI review.
- `dev-services`: reads `.nagi/project.toml` and shows declared services without starting them.
- `evidence-exporter`: previews mission proof and writes Markdown/JSON only after the explicit export action.

Install the pinned WASI target once, then build each plugin:

```sh
rustup target add wasm32-wasip2 --toolchain 1.96.1
cargo build --target wasm32-wasip2 --release --manifest-path examples/plugins/github-lifecycle/Cargo.toml
cargo build --target wasm32-wasip2 --release --manifest-path examples/plugins/dev-services/Cargo.toml
cargo build --target wasm32-wasip2 --release --manifest-path examples/plugins/evidence-exporter/Cargo.toml
```

Exercise the exact host boundary before sharing a component:

```sh
nagi plugin validate examples/plugins/github-lifecycle
nagi plugin test examples/plugins/github-lifecycle
nagi plugin test examples/plugins/dev-services --workspace /path/to/worktree
nagi plugin test examples/plugins/evidence-exporter --action export --workspace /path/to/worktree
nagi plugin pack examples/plugins/github-lifecycle --out /tmp/github-lifecycle.nagi-plugin
```

`plugin pack` rewrites the package manifest to the self-contained
`component.wasm` entrypoint. The resulting directory can be validated and
tested without the original Cargo target directory.
