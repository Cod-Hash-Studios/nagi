# nagi website

The homepage is `index.html`. The documentation source is in `src/content/docs/` and is rendered by Astro Starlight.

```bash
bun install
bun run dev
bun run build
```

The build output is `dist/`. The site targets the GitHub Pages project URL at
`https://cod-hash-studios.github.io/nagi/`, so Astro uses `/nagi` as its base
path. `bun run build` also rejects generated links or assets that escape that
base.

The `Deploy website` workflow builds and publishes `website/dist` after changes
reach `main`; it can also be started manually. The repository's Pages source
must be set to **GitHub Actions** once in GitHub settings.

Do not switch to a root-domain host without updating `site`, the shared
base-path helper, canonical URLs, and the generated-path check together.
