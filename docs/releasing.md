# Releasing

Rift publishes seven public npm packages from the project-owned `@jparklev` scope:

- `@jparklev/rift`
- `@jparklev/rift-<platform>-<arch>` for each supported native tuple

The tag-triggered [release workflow](../.github/workflows/release.yml) publishes native packages before the public launcher. It requests a GitHub OIDC ID token and publishes with provenance and public access.

## One-time npm bootstrap

npm trusted publishing is configured per package, and a package must exist before it can trust a workflow. Before the first release:

1. Ensure the npm account or organization that owns the `@jparklev` scope can publish public packages.
2. Add a short-lived granular publishing token with only that scope's write access and bypass-2FA enabled as the GitHub repository secret `NPM_PUBLISH_TOKEN`.
3. Push the first version tag. The workflow publishes every package with `--access public` and provenance.
4. In npm package settings, configure `jparklev/rift` and `.github/workflows/release.yml` as the GitHub Actions trusted publisher for every one of the seven packages, allowing `npm publish`.
5. Delete `NPM_PUBLISH_TOKEN` and restrict token-based publishing. Subsequent releases authenticate through GitHub OIDC and retain provenance without a long-lived credential.

Do not reuse the unrelated unscoped `rift` or `rift-snapshot` package names: they are owned by other projects.
