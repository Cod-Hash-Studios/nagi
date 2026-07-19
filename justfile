# nagi task runner

# Run tests
test:
    cargo nextest run --locked --status-level fail --final-status-level fail --failure-output final --success-output never
    python3 -m unittest scripts.test_agent_detection_manifest_check scripts.test_bench_render scripts.test_brand_isolation scripts.test_changelog scripts.test_config_reference_check scripts.test_docs_translation_parity scripts.test_fork_safety scripts.test_preview scripts.test_reference_plugins scripts.test_release_workflows scripts.test_seed_navigator_demo scripts.test_vendor_libghostty_vt scripts.test_vendor_portable_pty scripts.test_verify_release
    just integration-assets-test
    just plugin-marketplace-test

# Run one nextest filter, e.g. `just test-one codex_stale_working`
test-one filter:
    cargo nextest run --locked "{{filter}}" --status-level fail --final-status-level fail --failure-output final --success-output never

# Run fast local lint checks
lint:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings

# Run PR CI checks
ci filter='all()': lint
    cargo nextest run --locked -E "{{filter}}" --status-level fail --final-status-level slow --failure-output final --success-output never
    just integration-assets-test
    just plugin-marketplace-test
    just website-build

# Run Windows target lint from Unix/macOS to catch cfg(windows) compile and clippy failures before CI
windows-lint:
    rustup target add x86_64-pc-windows-msvc
    LIBGHOSTTY_VT_SIMD=false cargo clippy --bin nagi --locked --target x86_64-pc-windows-msvc -- -D warnings

# Check formatting + run unit tests + Windows target lint + maintenance script tests
check: ci windows-lint
    python3 -m unittest scripts.test_agent_detection_manifest_check scripts.test_bench_render scripts.test_brand_isolation scripts.test_changelog scripts.test_config_reference_check scripts.test_docs_translation_parity scripts.test_fork_safety scripts.test_preview scripts.test_reference_plugins scripts.test_release_workflows scripts.test_seed_navigator_demo scripts.test_vendor_libghostty_vt scripts.test_vendor_portable_pty scripts.test_verify_release
    @echo "docs reminder: if this changes user-facing behavior, make sure the relevant release docs are updated or called out before release."

# Install repo-local git hooks
install-hooks:
    git config core.hooksPath .githooks
    chmod +x .githooks/pre-commit
    chmod +x .githooks/commit-msg
    @echo "installed git hooks from .githooks"

# Build release binary
build:
    cargo build --release --locked

# Build the benchmark binary separately so compilation is never timed
bench-build:
    cargo build --release --locked

# Record the full local baseline with human-readable output
bench: bench-build
    sh scripts/bench_startup.sh --binary target/release/nagi --format human

# Record the full local baseline as machine-readable JSON
bench-json: bench-build
    sh scripts/bench_startup.sh --binary target/release/nagi --format json

# Fast, deliberately generous regression gate; override any NAGI_BENCH_* value to probe a ceiling
bench-smoke: bench-build
    @sh scripts/bench_startup.sh \
        --binary target/release/nagi \
        --startup-samples "${NAGI_BENCH_STARTUP_SAMPLES:-3}" \
        --render-samples "${NAGI_BENCH_RENDER_SAMPLES:-5}" \
        --warmups "${NAGI_BENCH_WARMUPS:-1}" \
        --idle-settle-seconds "${NAGI_BENCH_IDLE_SETTLE_SECONDS:-2}" \
        --idle-sample-seconds "${NAGI_BENCH_IDLE_SAMPLE_SECONDS:-2}" \
        --timeout-seconds "${NAGI_BENCH_TIMEOUT_SECONDS:-15}" \
        --cols "${NAGI_BENCH_COLS:-120}" \
        --rows "${NAGI_BENCH_ROWS:-40}" \
        --panes "${NAGI_BENCH_PANES:-1}" \
        --startup-ceiling-ms "${NAGI_BENCH_STARTUP_CEILING_MS:-5000}" \
        --render-ceiling-ms "${NAGI_BENCH_RENDER_CEILING_MS:-1000}" \
        --reattach-ceiling-ms "${NAGI_BENCH_REATTACH_CEILING_MS:-5000}" \
        --idle-cpu-ceiling-percent "${NAGI_BENCH_IDLE_CPU_CEILING_PERCENT:-25}" \
        --rss-ceiling-mib "${NAGI_BENCH_RSS_CEILING_MIB:-1024}" \
        --format human

# Build the website and documentation
website-build:
    cd website && bun install --frozen-lockfile && bun run build

# Test bundled agent integration assets
integration-assets-test:
    bun test src/integration/assets/nagi-agent-state.test.ts

# Run plugin marketplace Worker tests
plugin-marketplace-test:
    cd workers/plugin-marketplace && bun test

# Verify or intentionally regenerate deterministic Ratatui UI goldens
ui-goldens:
    python3 scripts/render_ui_goldens.py

ui-goldens-update:
    python3 scripts/render_ui_goldens.py --update

# Exercise hard crash, provider disconnect, and hostile plugin boundaries
chaos-test:
    python3 scripts/chaos_runtime.py

# Build the vendored libghostty-vt source dist
build-libghostty-vt:
    scripts/build_vendored_libghostty_vt.sh

# Check that release docs and changelog have been finalized from docs/next before release
release-docs-check:
    python3 scripts/agent_detection_manifest_check.py --require-website
    python3 scripts/config_reference_check.py
    @if ! diff -u website/src/data/config-reference.json docs/next/website/src/data/config-reference.json; then \
        echo "error: stable config reference differs from docs/next; finalize it before releasing"; \
        exit 1; \
    fi
    @for file in README.md CHANGELOG.md; do \
        if ! diff -u "$file" "docs/next/$file"; then \
            echo "error: $file differs from docs/next/$file; finalize release docs before releasing"; \
            exit 1; \
        fi; \
    done
    @for file in CONFIGURATION.md INTEGRATIONS.md SOCKET_API.md; do \
        if [ -e "$file" ]; then \
            echo "error: $file was replaced by website docs; remove the root copy"; \
            exit 1; \
        fi; \
    done
    @test -d docs/next/website/src/content/docs
    @for file in $(find website/src/content/docs -path '*/preview' -prune -o -type f -name '*.mdx' -print); do \
        relative="${file#website/src/content/docs/}"; \
        staged="docs/next/website/src/content/docs/$relative"; \
        if [ ! -f "$staged" ]; then \
            echo "error: $staged is missing; docs/next/website/src/content/docs must mirror website/src/content/docs"; \
            exit 1; \
        fi; \
        if ! diff -u "$file" "$staged"; then \
            echo "error: $file differs from $staged; finalize website docs before releasing"; \
            exit 1; \
        fi; \
    done
    @for file in $(find docs/next/website/src/content/docs -type f -name '*.mdx' -print); do \
        relative="${file#docs/next/website/src/content/docs/}"; \
        released="website/src/content/docs/$relative"; \
        if [ ! -f "$released" ]; then \
            echo "error: $file has no matching released website doc"; \
            exit 1; \
        fi; \
    done
    @for file in website/src/content/docs/*.mdx; do \
        for locale in ja zh-cn; do \
            translated="website/src/content/docs/$locale/$(basename "$file")"; \
            if [ ! -f "$translated" ]; then \
                echo "error: $translated is missing; translate stable docs before releasing"; \
                exit 1; \
            fi; \
        done; \
    done
    @for file in website/src/content/docs/ja/*.mdx website/src/content/docs/zh-cn/*.mdx; do \
        released="website/src/content/docs/$(basename "$file")"; \
        if [ ! -f "$released" ]; then \
            echo "error: $file has no matching english doc; remove the stale translation"; \
            exit 1; \
        fi; \
    done
    python3 scripts/docs_translation_parity.py --docs-root website/src/content/docs

# Prepare the release commit without tagging or pushing (usage: just release-prepare 0.1.1)
release-prepare version:
    @printf '%s\n' '{{version}}' | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { \
        echo "error: version must look like 0.6.6 without a v prefix"; \
        exit 1; \
    }
    @if [ -n "$(git status --porcelain)" ]; then \
        echo "error: commit your changes first"; \
        exit 1; \
    fi
    @git fetch origin main --tags
    @if git rev-parse "v{{version}}" >/dev/null 2>&1; then \
        echo "error: tag v{{version}} already exists"; \
        exit 1; \
    fi
    just release-docs-check
    python3 scripts/changelog.py prepare --version {{version}}
    cp CHANGELOG.md docs/next/CHANGELOG.md
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' Cargo.toml && rm -f Cargo.toml.bak
    cargo update -p nagi --offline
    just check
    git add CHANGELOG.md docs/next/CHANGELOG.md Cargo.toml Cargo.lock
    git diff --cached --quiet || git commit -m "release: v{{version}}"
    @echo "v{{version}} release commit prepared. Review it, then run: just release-publish {{version}}"

# Tag and push an already-prepared release commit (usage: just release-publish 0.1.1)
release-publish version:
    @printf '%s\n' '{{version}}' | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { \
        echo "error: version must look like 0.6.6 without a v prefix"; \
        exit 1; \
    }
    @if [ -n "$(git status --porcelain)" ]; then \
        echo "error: working tree must be clean before publishing"; \
        exit 1; \
    fi
    @branch="$(git branch --show-current)"; \
    if [ "$branch" != "main" ]; then \
        echo "error: release-publish must run from main, got $branch"; \
        exit 1; \
    fi
    @git fetch origin main --tags
    @if git rev-parse "v{{version}}" >/dev/null 2>&1; then \
        echo "error: tag v{{version}} already exists"; \
        exit 1; \
    fi
    @cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"; \
    if [ "$cargo_version" != "{{version}}" ]; then \
        echo "error: Cargo.toml version $cargo_version does not match {{version}}"; \
        exit 1; \
    fi
    just release-docs-check
    python3 scripts/changelog.py extract --version {{version}} --output /tmp/nagi-release-notes-check.md
    rm -f /tmp/nagi-release-notes-check.md
    @local_head="$(git rev-parse HEAD)"; \
    remote_head="$(git rev-parse origin/main)"; \
    if ! git merge-base --is-ancestor "$remote_head" "$local_head"; then \
        echo "error: origin/main is not an ancestor of HEAD; pull or rebase before publishing"; \
        exit 1; \
    fi; \
    if [ "$local_head" != "$remote_head" ]; then \
        echo "pushing release commit to origin/main"; \
        git push origin HEAD:main; \
    fi
    git tag -a v{{version}} -m "v{{version}}"
    git push origin v{{version}}
    @echo "v{{version}} released — GitHub Actions building binaries and updating website/latest.json"

# Prepare, verify, tag, push, and trigger the GitHub Release workflow (usage: just release 0.1.1)
release version:
    just release-prepare {{version}}
    just release-publish {{version}}

# Print default config
default-config:
    cargo run --release --locked -- --default-config
