# GitHub Actions builds and releases

## Automatic builds

`.github/workflows/build.yml` runs on branch pushes, pull requests, and manual
**Actions → Build → Run workflow** requests. It also serves as the reusable build
for releases. All jobs use GitHub-hosted `ubuntu-latest` or `windows-latest` runners
and current stable Rust; Node uses the latest LTS.

Before any release-mode compilation or packaging, both test jobs must pass:

- Linux and Windows: `cargo test --locked --workspace --lib --bins`.
- Linux: the complete `cargo test --locked --workspace`, including stock-LuaJIT,
  CLI JSONL, and DAP integration tests. LuaJIT is installed by the workflow.
- Both: the Zed native unit-test target and VS Code JavaScript syntax check.
  Zed currently has no unit tests; this checks that its test target compiles.

Successful builds upload these six files to the run's **Artifacts** section,
retained for 30 days:

| Download | Contents |
| --- | --- |
| `gdebug-linux-x86.tar.gz` | x86 CLI; `gmsv_gdebug_linux.dll`, `gmcl_gdebug_linux.dll` |
| `gdebug-linux-x64.tar.gz` | x64 CLI; `gmsv_gdebug_linux64.dll`, `gmcl_gdebug_linux64.dll` |
| `gdebug-windows-x86.zip` | x86 CLI; `gmsv_gdebug_win32.dll`, `gmcl_gdebug_win32.dll` |
| `gdebug-windows-x64.zip` | x64 CLI; `gmsv_gdebug_win64.dll`, `gmcl_gdebug_win64.dll` |
| `gdebug-vscode.vsix` | Platform-independent VS Code extension |
| `gdebug-zed.tar.gz` | Platform-independent Zed WASI component, manifest, and debug adapter schema |

Native bundles put `gdebug` / `gdebug.exe` at the archive root and modules under
`garrysmod/lua/bin/`. Server and client modules are **compiled separately** with
their respective Cargo features, not just renamed copies. Linux modules are ELF
shared libraries despite GMod's required `.dll` suffix. Tar archives preserve the
Linux CLI's executable bit. Windows builds statically link the MSVC CRT.

Install the VSIX with **Extensions: Install from VSIX**. Both editor extensions
still require the separately downloaded CLI on PATH (or the configured adapter
path); neither bundles a platform-specific CLI. The Zed download is a prebuilt
extension payload, not a marketplace publication: extract it into the `gdebug`
subdirectory of Zed's `extensions/installed` directory and restart Zed. For normal
source development, continue using Zed's **Install Dev Extension** on `editors/zed`.

### Caches and linkers

- `Swatinem/rust-cache` caches Cargo downloads and installed Cargo tools, with
  target-specific native-build keys. Target-directory caching is disabled to avoid
  duplicating the compiler cache.
- `mozilla-actions/sccache-action` caches Rust compilation through GitHub's cache
  backend; incremental compilation is disabled.
- Zig's setup action caches its toolchain and linker cache; npm's download cache
  caches the pinned VSIX packager.
- **mold is used for native Linux tests.** Distributable Linux binaries use
  `cargo zigbuild` targeting glibc 2.17, preserving the project's existing build
  baseline even as `ubuntu-latest` changes. They use Zig's linker, not mold.
  Windows uses MSVC and Zed uses the WASM linker; Linux mold flags never reach them.
- Both Rust workspaces use their checked-in `Cargo.lock` files with `--locked`.

## Version-tag releases

`.github/workflows/release.yml` only triggers on numeric `vMAJOR.MINOR.PATCH` tags,
for example `v0.1.0`. Branch pushes and prerelease tags such as `v0.1.0-rc.1` do not
publish releases. Update the workspace and editor manifest versions before tagging;
the workflow does not rewrite source versions.

```sh
git tag v0.1.0
git push origin v0.1.0
```

The release calls the same reusable build workflow **at the tagged commit**. Its
publisher waits for all tests and builds, downloads only that workflow run's six
artifacts, checks that the set is complete, adds `SHA256SUMS`, and creates a GitHub
Release with generated notes. It does not rebuild in the publish job, select a
"latest successful" build, or depend on a previous branch run's expiring artifacts.
A tag therefore performs its own cached build even if the branch already built
that commit. Release assets remain available independently of Actions retention.

Publishing also verifies that the remote tag still resolves to the build's commit;
a tag moved during the build causes publication to fail.

Only the publishing job receives `contents: write`; builds and PRs are read-only.
The built-in `GITHUB_TOKEN` is sufficient; no publishing secret is required. Enable
GitHub Actions and permit that token to create releases in repository/organization
policy. Existing releases are not overwritten by a rerun.

These are automated build/test artifacts, **not proof of all-platform GMod runtime
qualification or editor UI behavior**. See [QUALIFICATION.md](QUALIFICATION.md).
The repository has no license file yet; VSIX packaging explicitly permits that
omission. Marketplace publication is not configured.
