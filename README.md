[![](https://gamebanana.com/mods/embeddables/700758?type=large)](https://gamebanana.com/mods/700758)

# DeadlockShock

DeadlockShock is a small ui mod + companion app, to better sync in-game deaths to external shockers.
Made because my friend complained about OCR missfiring for them.

## Contents

- `mod/` has the Panorama death hook.
- `companion/` is the desktop app.
- `pishock/` and `openshock/` talk to the two shock providers.

## Preview

[![Preview](./media/showcase.png)](./media/showcase.png)

## Building from source

You will need [Rust](https://rust-lang.org/), [PowerShell](https://learn.microsoft.com/en-us/powershell/), and [Reduced CSDK 12](https://deadlockmodding.pages.dev/modding-tools/csdk-12).

From the repo root:

```powershell
.\build.bat
cargo build --manifest-path companion/Cargo.toml --release
```

That puts the addon at `dist/deadlock_death_hook.vpk`. Install and enable it in Deadlock, then start the companion:

```sh
cargo run --manifest-path companion/Cargo.toml --release
```

Pick PiShock or OpenShock, enter your credentials, choose a device group, and try the test sound. Set the intensity and duration you want, then auto-detect `console.log` and start the listener. From there, every new local-player death shocks the available shockers in that group.

The companion remembers your setup—including credentials—in your OS user config directory.

## Publishing a release

The Drone repository needs a `github-token` secret with permission to create releases in this GitHub repository. Push a version tag to start the release pipeline:

```sh
git tag v0.1.0
git push origin v0.1.0
```
