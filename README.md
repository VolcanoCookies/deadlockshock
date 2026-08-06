[![](https://gamebanana.com/mods/embeddables/700758?type=large)](https://gamebanana.com/mods/700758)

# DeadlockShock

DeadlockShock is a small UI mod and companion app that can sync local-player deaths, ability uses, and cooldown readiness to external shockers.
Made because my friend complained about OCR missfiring for them.

## !!! Required Deadlock mod !!!

The companion app does not work by itself. Install and enable the [DeadlockShock mod from GameBanana](https://gamebanana.com/mods/700758) in Deadlock before starting the companion. The mod detects gameplay events and writes them to the log that the companion listens to.

## Contents

- `mod/` has the Panorama gameplay-state hook.
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

In **Setup**, pick PiShock or OpenShock, enter your credentials, test the connection, select a device group, and try the test sound.

In **Effects**, configure Death, Ability use, and Cooldown ready independently. Each trigger has its own fixed or random-interval shock profile. Ability-use and cooldown-ready also have independent positional-slot filters that apply across heroes; ability names appear when the addon reports them, with numbered slots as the fallback. Use the explicit Copy control to copy only shock settings between profiles without changing enablement or ability selection. Local-player death is enabled by default, while both ability triggers are opt-in. Cooldown ready covers both a normal cooldown finishing and a charged ability restoring a charge.

In **Game connection**, auto-detect `console.log` or enter its path, start the listener, and review listener, bridge-event, and delivery diagnostics. Deadlock must run with `-condebug` so the log is written.

The companion remembers your setup—including credentials, all three shock profiles, and ability filters—in your OS user config directory. Ability names are runtime diagnostics and are not saved.

## Publishing a release

The Drone repository needs a `github-token` secret with permission to create releases in this GitHub repository. Push a version tag to start the release pipeline:

```sh
git tag v0.1.0
git push origin v0.1.0
```
