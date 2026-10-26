# Hivegrid fork of WezTerm

Private fork of `wezterm/wezterm` main. Branch `hivegrid` carries a few commits
on top of upstream; `origin` is the private fork, `upstream` is wezterm/wezterm.

## What it changes

- Clickable tab bar status: `wezterm.format` takes `{ Click = 'id' }` and
  `'ClickEnd'`; a click on such a region emits `status-click(window, pane, id,
  button)` instead of dragging the window. See
  `docs/config/lua/window-events/status-click.md`. Implemented through an OSC 8
  hyperlink with the `wezterm-status-click:` scheme, so the id travels through
  the unchanged `set_right_status` string path; stock WezTerm ignores it.
- Fancy tab bar proportions: `tab_bar_height_scale`, `tab_bar_status_scale`,
  `tab_bar_separator_height` and `tab_bar_separator_color` (see
  `docs/config/lua/config/`). Stock WezTerm rejects them, so set them with `pcall`.

## Build without sudo

The extra development libraries live in `~/.local/sysroot` (unpacked .debs).
`./build-hivegrid.sh` sets `PKG_CONFIG_PATH`, `LD_LIBRARY_PATH` and `RUSTFLAGS`
for it and runs `cargo build --release -p wezterm-gui -p wezterm` (add
`WITH_MUX=1` for `wezterm-mux-server`). A cold build takes a few minutes, an
incremental one around 30-80 s.

## Deploy

`./build-hivegrid.sh` then copies the binaries to
`~/.local/opt/hivegrid-wezterm/`, the sysroot libraries they need to its
`lib/`, and writes the wrapper `~/.local/bin/hivegrid`. The wrapper puts that
directory first on `PATH` (so `wezterm cli` in panes is the matching build) and
execs `wezterm-gui`. It never touches `/usr/bin/wezterm`. To use it from the
desktop entry, change `Exec=` to `/home/<user>/.local/bin/hivegrid start --cwd .`.
`SKIP_BUILD=1 ./build-hivegrid.sh` redeploys without building.

Configs written for the stock release stay compatible: guard fork-only items
with `pcall(wezterm.format, { { Click = 'probe' } })`.

## Rebase on upstream

```
git fetch upstream
git rebase upstream/main      # on branch hivegrid
./build-hivegrid.sh
git push --force-with-lease origin hivegrid
```

Conflicts are most likely in `wezterm-gui/src/tabbar.rs`,
`termwindow/mouseevent.rs` and `termwindow/box_model.rs`.
