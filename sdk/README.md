# Nagi plugin SDKs

The Rust and TypeScript guest SDKs mirror Nagi's manifest v2 invocation and structured inspector document contract. They deliberately expose data types and bounded serialization only: capabilities remain enforced by the host.

- `rust/nagi-plugin-sdk`: dependency-light Rust/WASI helpers.
- `typescript`: runtime-neutral JavaScript helpers with TypeScript declarations.

Both SDKs treat `NAGI_PLUGIN_CONTEXT_JSON` and stdin as untrusted input and emit only host-rendered structured documents. Run their tests with:

```sh
cargo test --manifest-path sdk/rust/nagi-plugin-sdk/Cargo.toml
npm test --prefix sdk/typescript
```
