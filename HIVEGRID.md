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
- Hover for panes without focus: mouse motion goes to the pane under the pointer even when
  another pane has the focus, so the sidebar bar and the overview dock can highlight what is
  under the pointer. Upstream sent motion over another pane to the focused pane, with that
  other pane's coordinates (a focused git view lit a row while the pointer was over the grid).
- `window:get_dimensions()` also returns `tab_bar_height` (pixel height of the tab bar, 0 when
  hidden), `content_top` (pixels from the window top to the first text row: tab bar at the
  top, OS border and `window_padding.top`) and `content_bottom` (where the last text row
  ends). Stock WezTerm lacks them (nil); the sidebar bar uses them to centre its buttons.
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

- Chrome pane user vars (set with `OSC 1337 SetUserVar`): `hivegrid_no_focus` — a click on the
  pane reaches it but does not take the keyboard focus (`termwindow/mouseevent.rs`);
  `hivegrid_no_dim` — `inactive_pane_hsb` is not applied to it (`render/pane.rs`,
  `RenderScreenLineParams::dim`); `hivegrid_fixed_width` — a tab resize or a top-level split
  leaves its columns alone while the rest of the tree can absorb the change (a split whose
  leaves are all fixed, the bar with a sidebar view beside it, is fixed too; when such a split
  has to take a change anyway, its wider side takes it, so the bar keeps its width);
  `hivegrid_chrome` — chrome for zoom only (below). A `hivegrid_fixed_width` pane is chrome as
  well. The sidebar bar sets `no_focus`, `no_dim` and `fixed_width`, the sidebar views and the
  overview dock `no_dim` and `fixed_width`.
- Zoom keeps the chrome (`mux/src/tab.rs` `zoom_area`, `non_chrome_area`): when a pane is
  zoomed (`TogglePaneZoomState`, `SetPaneZoomState`, `wezterm cli zoom-pane`), chrome panes at
  the edges of the tab keep their place and size and the zoomed pane fills the rest (from the
  root, every split with a chrome-only side is stepped over). The other panes are hidden, the
  dividers beside the chrome stay and can be dragged, and a tab resize while zoomed keeps the
  split tree up to date, so unzooming restores the layout at the current size. A chrome pane can
  have the focus while the zoom stays: clicking it, `pane:activate()`, or an
  `ActivatePaneDirection` from the zoomed pane to an adjacent chrome pane (and from a chrome
  pane to what is visible); a move to a hidden pane unzooms (`unzoom_on_switch_pane`), as does
  any split, top-level included (upstream refused a top-level split while zoomed). Zooming a
  chrome pane itself, or a tab without chrome at its edges, is the stock full-tab zoom. Lua sees
  the zoom as `is_zoomed` in `tab:panes_with_info()` and `PaneInformation`; while zoomed the
  active pane may be a chrome pane, so test whether any pane is zoomed. `wezterm cli list`
  reports the zoomed pane with its zoom size and its unzoomed position (as upstream).
- Proportional tab resize (`mux/src/tab.rs` `share_delta`): a size change is shared between the
  two sides of every split in proportion to their sizes and never below a subtree's minimum,
  in one pass. Upstream alternated single cells between the sides (the first column of a
  nested grid took half of every change) and could spin forever on the main thread when a
  side was already at its minimum.
- Top-level splits size the new pane: upstream left a pane inserted by a top-level split of a
  tab with two or more panes at the size it was spawned or moved with, and `split-pane
  --left`/`--top` spawned the new pane with the other side's size (`mux/src/domain.rs`); the bar
  drew for the whole tab width until something else resized it.
- `wezterm cli adjust-pane-size --pane-id N` resizes pane N (`Tab::adjust_pane_size_of`);
  upstream ignored the id and resized the tab's active pane.
- Edge panes with their own default background: a pane that set one with OSC 11 already
  paints the window padding beside it in that colour; a vertical divider next to such a pane
  now also runs through the top and bottom padding (`render/split.rs`), so the bar and dock
  run from the header to the bottom border with their dividers.
- Cells past a pane's width are not drawn (`render/screen_line.rs`): a line keeps the cells it
  had before its pane got narrower, and inline images a program drew while its new top-level
  pane was briefly full width were painted over the neighbouring panes.
- The floating "+" button's rounded corners take its fill colour (they were notched in the
  default border colour), and it sits 0.75 cell from the last tab.

## Build without sudo

The extra development libraries live in `~/.local/sysroot` (unpacked .debs).
`./build-hivegrid.sh` sets `PKG_CONFIG_PATH`, `LD_LIBRARY_PATH` and `RUSTFLAGS`
for it and runs `cargo build --release -p wezterm-gui -p wezterm` (add
`WITH_MUX=1` for `wezterm-mux-server`). A cold build takes a few minutes, an
incremental one around 30-80 s.

## Deploy

`./build-hivegrid.sh` then copies the binaries to
`~/.local/opt/hivegrid-wezterm/` (`OPT=`), the sysroot libraries they need to its
`lib/`, and writes the wrapper `~/.local/bin/hivegrid` (`WRAPPER=`). Binaries and
libraries are written to a temporary name and renamed, so a running Hivegrid keeps its
mapped files (copying over a mapped library crashed it). The wrapper puts that
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
