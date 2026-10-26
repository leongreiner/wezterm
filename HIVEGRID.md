# Hivegrid fork of WezTerm

Private fork of `wezterm/wezterm` main. Branch `hivegrid` carries a few commits
on top of upstream; `origin` is the private fork, `upstream` is wezterm/wezterm.

## What it changes

- Clickable tab bar status: `wezterm.format` takes `{ Click = 'id' }` and
  `'ClickEnd'`; a click on such a region emits `status-click(window, pane, id,
  button)` instead of dragging the window. See
  `docs/config/lua/window-events/status-click.md`. Implemented through an OSC 8
  hyperlink with the `wezterm-status-click:` scheme, so the id travels through
  the unchanged `set_right_status` string path; stock WezTerm ignores it. Holding the
  left button on such a region (brand mark included) and moving more than 4 px drags the
  window instead of clicking, so the header can be grabbed anywhere.
- Mouse pointer shape (OSC 22): a program that grabs the mouse can request a pointer
  shape (`ESC ] 22 ; ew-resize ST`; `=name` sets, `>name` pushes, `<` pops, an empty name
  restores the default; a comma separated list is a fallback list of CSS cursor names). The
  shape applies while the pointer is over that pane and the pane has mouse reporting on.
  Stock WezTerm ignores the sequence. The explorer uses it for the column borders.
- Hover for panes without focus: mouse motion over a pane that reports the mouse goes to
  that pane even when another pane has the focus, so the sidebar bar and the
  overview dock can highlight what is under the pointer.
- Pointer leave report (private mode 8800): a pane learns of the pointer only while it is
  inside it, so a program cannot tell when the pointer moves on. A program that sets
  `CSI ? 8800 h` (reset with `CSI ? 8800 l`, query with DECRQM `CSI ? 8800 $ p`, reset by RIS)
  is told: when the pointer moves to another pane, to a divider, the tab bar or any other
  non-pane item, or out of the window, the GUI writes one SGR motion report without button at
  column 0, row 0 (`ESC [ < 35 ; 0 ; 0 M`; valid positions start at 1, so it is no position)
  and drops the OSC 22 pointer shapes the program had set, so a hand cannot outlive the
  hover; the program sets its shape again with the motion that enters it. The report needs
  mouse reporting on with the SGR (1006) or SGR-pixels (1016) encoding. The pane told is the
  one the last mouse event went to (`TermWindow::hover_pane`, `Pane::mouse_leave`,
  `TerminalState::mouse_leave`); it works for local panes only, not for panes of a mux client
  domain. Stock WezTerm ignores the mode (unknown private modes are only logged). The bar and
  the overview dock use it to end their hover fade.
- `ShowOverlayCommand` key assignment (argument: a `SpawnCommand`): runs a program in a
  pty-backed overlay that covers the active tab until the program exits, then restores the
  tab; its panes keep running underneath. Only the full-tab overlay exists, there is no
  centred modal mode. Programs in the overlay can set user vars (`user-var-changed` fires).
  See `docs/config/lua/keyassignment/ShowOverlayCommand.md`. Stock WezTerm rejects the
  action; guard it with `pcall`.
- Fancy tab bar proportions: `tab_bar_height_scale`, `tab_bar_status_scale`,
  `tab_bar_separator_height` and `tab_bar_separator_color` (see
  `docs/config/lua/config/`). Stock WezTerm rejects them, so set them with `pcall`.
- `tab_bar_brand`: a drawn honeycomb mark (seven coloured hexagons) at the left end of the
  fancy tab bar, clickable as the status-click id `brand`; the polys are static shapes in
  `termwindow/render/brand_mark_shapes.rs`, rendered through `ElementContent::Mark`.
  `tab_bar_floating_tabs` draws tabs and "+" as rounded chips centred in the bar, level
  with the status boxes. Both are rejected by stock WezTerm: set them with `pcall`.

- Wayland window drag: the Wayland backend implements `request_drag_move`
  (`xdg_toplevel.move`), which it lacked, so the integrated tab bar can move the window on
  Wayland (it already could on X11).

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
