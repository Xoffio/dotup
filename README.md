# dotup

I use [chezmoi](https://www.chezmoi.io/) for my dotfiles, and part of that setup installs very specific versions of binaries, so no matter where I go, I can work the same. The dotfiles live [here](https://github.com/Xoffio/dotfiles).

The thing that bugged me at the beginning: whenever a tool got a new release, I had to manually look for the latest version, get all the hashes for the platforms I use, and update my dotfiles.

So I wrote dotup. It checks what versions have been updated and what exactly has changed, and if I want to update, it updates my dotfiles with all the hashes I need. Super easy.

## How it works

dotup drives two files in my chezmoi data (`~/.local/share/chezmoi/.chezmoidata/`):

- **`tools.toml`** — one entry per tool: the version I have, the GitHub repo to watch, and a regex per platform saying which release asset I install
- **`hashes.toml`** — the sha256 of each of those assets

```toml
[tools.opencode]
version = "2.2.6"
repo = "anomalyco/opencode"

[tools.opencode.dotup.assets]
linux_amd64 = "^opencode-linux-x64\\.tar\\.gz$"
linux_arm64 = "^opencode-linux-arm64\\.tar\\.gz$"
darwin_arm64 = "^opencode-darwin-arm64\\.zip$"
```

Each regex has to match exactly one asset in the release — if it matches none or several, dotup warns me instead of guessing. Tools without a `dotup` section are version-checked only.

## Usage

```sh
cargo build --release
./target/release/dotup
```

| Key | Action |
| --- | --- |
| `r` | re-check (only the visible tools when a filter is active) |
| `m` | toggle **latest** ↔ **highest** semver. Some repos (like opencode) flag an older line as GitHub's "Latest" release |
| `/` | filter tools by name, repo, version or status |
| `d` | dry-run an update: preview version + hash changes, write nothing |
| `u` | update: write the new version to `tools.toml` and the new hashes to `hashes.toml` |
| `↑/↓ j/k` | navigate · `←/→` switch pane · `q` quit |

Two things worth knowing: without a GitHub token you get 60 API requests per hour, so I export `GITHUB_TOKEN` (5,000/hour); and check results are cached for an hour in `~/.cache/dotup/cache.json`.
