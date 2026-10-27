# Hivegrid fork of WezTerm

Fork of `wezterm/wezterm` main for [Hivegrid](https://github.com/leongreiner/hivegrid). Branch
`hivegrid` carries a few commits on top of upstream; `origin` is this fork, `upstream` is
wezterm/wezterm.

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
- A closed pane's room (`mux/src/tab.rs` `remove_pane_if`, `return_fixed_width_room`): it is
  shared out over the whole surviving subtree in proportion, like a tab resize. Upstream left
  every nested split's first side alone and gave all of it to the last column, so closing the
  overview dock (a top-level split) after opening it moved columns from the first panes to the
  last one, a little more each time. A fixed-width pane (a sidebar view) that closes beside
  another fixed-width one (the bar) gives its columns to the grid side of the split above
  instead of to the survivor: upstream made the bar as wide as bar + view until Lua narrowed it
  again, and a Lua that did not run (a failing status handler) left it so.
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
- Web pane (`mux/src/webpane.rs`, `hivegrid-web/`, `wezterm-gui/src/termwindow/render/web.rs`): a
  pane kind that shows a web page. The page lives in an out-of-process headless Chrome
  (`chrome --headless=new --remote-debugging-pipe`, DevTools protocol on fds 3/4, no TCP port;
  one browser process per profile name, one page per pane, sandbox on, the browser dies with the
  GUI). Frames come from `Page.startScreencast` (JPEG), are decoded in Rust and drawn as one
  textured quad in the pane's rectangle after the zindex-0 layer; a pane with no frame yet, or
  whose browser exited, draws its placeholder text grid (URL and title cut to one row each, then
  the state; `Press Enter to restart the browser.` after a crash, followed by the reason and the
  last lines Chrome printed). `WebPane` is an ordinary `mux::Pane`: no new PDU, no `PaneEntry`
  change, no Lua API; split, zoom (chrome-aware), rotate, swap, `--move-pane-id` and `cli list`
  work on it as on any pane. Mouse (always grabbed, so no cell rounding; a wheel notch is meant to
  scroll 100 CSS px, see below), keys, paste and composed text go to the page; Hivegrid's own key bindings
  still run first. Ctrl+C and Ctrl+Insert also ask the page for its selection and put it on the
  clipboard (`MuxNotification::AssignClipboard`); hyperlink rules never apply to the placeholder; Enter in a pane whose browser exited restarts
  it; closing the pane closes the page, and Chrome exits with the last page of its profile.
  - Spawning: the program name `hivegrid:web <url>` (`about:blank` without a URL) handled first
    in `LocalDomain::spawn_pane`. The profile (default `main`) and privacy come from variables
    set on the spawn command itself, `HIVEGRID_WEB_PROFILE=<name>` and `HIVEGRID_WEB_PRIVATE=1`
    (the GUI's own environment does not count), which Lua's `pane:split{ args = { 'hivegrid:web',
    url }, set_environment_variables = { ... } }` and `SpawnCommandInNewTab` forward (checked in
    the GUI for `pane:split`). `HIVEGRID_WEB_FLAGS` (read when a pane spawns) adds Chrome flags,
    e.g. `--no-sandbox` where Chrome cannot create its sandbox (unsafe). CLI sugar: `wezterm cli
    split-pane --url URL [--profile NAME] [--private] [--browser PATH]` and `wezterm cli spawn --url URL ...`;
    `--url` conflicts with `PROG`, `--cwd` and `--move-pane-id`, and both build the command with
    `mux::webpane::spawn_command`. Stock WezTerm would try to run a program called `hivegrid:web`,
    so the config only uses it when `theme.has_web` is true (the build is recognised by the same
    probe as the Click format item, see Deploy). Unsupported: `wezterm-mux-server` and `wezterm
    cli` never call `mux::webpane::enable` (spawning there fails with "web panes need the
    Hivegrid GUI"), mux client domains and SSH domains refuse the sentinel ("web panes are only
    supported in the local domain"). Profiles live in
    `${XDG_DATA_HOME:-~/.local/share}/hivegrid/web/profile-<name>` (a relative `XDG_DATA_HOME` is
    ignored; mode 0700, cookies and logins persist); a private pane gets an ephemeral browser
    context. The directory name is injective (letters, digits, `-`, `_`, `.` stay, every other
    byte becomes `%XX`), so `a/b` and `a_b` never share a directory, and browsers are keyed by it.
    Browser binary, highest first (`mux::webpane::browser_choice`, read when a pane spawns): the
    spawn command's `HIVEGRID_WEB_BROWSER=<absolute path>` (`--browser PATH`, made absolute by the CLI,
    since the GUI's directory is not the caller's), `HIVEGRID_CHROME` (read by the engine; a set
    variable also stops the settings from being read), `web.browser` of `$HIVEGRID_SETTINGS` or
    `~/.config/hivegrid/settings.json` (an absolute path, `"auto"` or anything else is none; the GUI
    reads the file for each spawn itself, so Lua, `pane:split` env, `SpawnCommandInNewTab`, `wezterm
    cli` and the Python hive all agree without carrying the path), then the engine's search
    (`/opt/google/chrome/chrome`, `google-chrome-stable`, `google-chrome`, `chromium`,
    `chromium-browser` on `PATH`). A named binary that is not an executable file is an error shown
    in the pane, never a silent fallback. The browser pool is keyed by profile directory alone, not
    by (profile, binary): two Chromes cannot share one profile directory (the second hands its
    window to the first and exits), so a view joins the browser already running for its profile
    whatever binary it asked for, and a changed choice applies to the next browser process of the
    profile, after the last pane of the profile closed. Chromium-family only (CDP screencast);
    Firefox has none. Snap and Flatpak browsers are not supported (confined: the hidden profile
    directory is out of their reach and their launcher does not reliably pass the fd 3/4 pipe).
    Other knobs: `HIVEGRID_WEB_QUALITY`, `HIVEGRID_WEB_SCALE`,
    `HIVEGRID_WEB_DEVTOOLS` (see Security below, and `hivegrid-web/src/lib.rs`).
  - Security notes for the web pane:
    - Cookies and logins sit in the profile directory under Chrome's *basic* password store
      (`--password-store=basic`): they are obfuscated with a key built into Chrome, not encrypted
      by the OS keyring, so anything running as your user can read them. The directories are 0700
      and the files stay private to the user; use a `--private` pane for a login you do not want
      on disk.
    - `HIVEGRID_WEB_DEVTOOLS=1` is opt-in and starts Chrome with `--remote-debugging-port=0`: an
      unauthenticated DevTools port on 127.0.0.1 that any local process can use to control the
      browser and read its cookies. A warning is logged each time a browser is started with it.
      Leave it unset except while debugging.
    - Chrome is started with a cleared environment plus an allowlist (`HOME`, `PATH`, `XDG_*`,
      `LANG`, `LC_*`, `TZ`, `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`,
      the proxy variables in lower and upper case, `HIVEGRID_WEB_*`); tokens and `WEZTERM_*` of
      the GUI process do not reach the browser or its renderers.
    - A typed, spawned (`--url`, `hivegrid:web`) or `navigate;` location is opened only for the
      schemes `http`, `https`, `file`, `data` and exactly `about:blank`; `javascript:`, `chrome:`,
      `devtools:`, `about:flags`, `view-source:` and the like are refused (a spawn falls back to
      `about:blank`, a navigation is ignored). The Lua and Python normalisers apply the same list.
    - Page-controlled text is cleaned where it enters (`hivegrid_web::sanitize_display`): the title
      and the URL shown lose control characters (`char::is_control`, U+2028/2029, bidi controls) and
      are cut to 512 and 2048 characters; a JS dialog's text is cleaned before it is logged. CDP
      messages above 32 MiB are dropped without being stored (the connection stays up), at most 64
      mouse-move commands wait in the writer queue, and a decoded frame may not exceed a few
      times the view's pixel area.
    - A panic in a view's worker or decoder thread, or a texture that cannot be allocated, leaves
      the pane's placeholder instead of a frozen page or an aborted paint.
  - Wheel: the backends count differently, so `mux::webpane::set_wheel_units` (called from
    `GuiFrontEnd::try_new`) says how: X11 sends 5 "lines" per notch (x 20 CSS px = 100 px), Wayland
    the pointer axis value times the window scale, i.e. device pixels (about 10 per notch of a
    wheel [not observed here], x 10 / scale CSS px). Chrome scrolls a CDP wheel delta 1:1. On
    Wayland a touchpad, whose amounts are finger movement, scrolls proportionally fast: the window
    layer does not tell it from a wheel.
  - Scale: Chrome's screencast frames are CSS-sized unless the browser itself runs at the view's
    scale, so a browser is launched with `--force-device-scale-factor` of the first view that needs
    it (a flag of your own in `HIVEGRID_WEB_FLAGS` wins) and expects frames of that size. A view of
    another scale (a second monitor) in the same browser still gets a frame: smaller than its
    pixels when its scale (times zoom) is above the browser's, which the renderer stretches. A frame
    of an unexpected size is never left unshown: it appears once the size had 500 ms to settle.
    Mouse positions are divided by the page zoom (`zoom;N` of the control channel).
  - Exit: the GUI calls `mux::webpane::shutdown` after `Mux::shutdown`, which sends every browser
    `Browser.close` and waits up to 3 s, so cookies and logins reach the profile; a browser that
    does not quit is killed, and `PR_SET_PDEATHSIG` still kills it if the GUI dies (kill -9).
  - Page policy (`hivegrid-web`): permissions and downloads are denied, `alert` is accepted,
    `confirm` and `prompt` are dismissed, and `window.open`/`target=_blank` load their URL in the
    same pane instead of opening a window, but only `http(s)` URLs (a script cannot steer the pane to
    `data:`, `file:` or `javascript:`). The cursor a page reports is limited to the CSS cursor
    keywords, and a view's events reach the GUI as at most one queued redraw per pane.
  - Control channel: `ESC _ hgweb ; <command> [; <arg>] ESC \` written to the pane (Lua
    `pane:send_text`, `wezterm cli send-text --no-paste`): `navigate;URL`, `back`, `forward`,
    `reload`, `hardreload`, `stop`, `restart`, `copy-url`, `zoom;PERCENT`, `devtools` (only with
    `HIVEGRID_WEB_DEVTOOLS=1`). Output of a program in a terminal pane never reaches it: the
    sequence has to be written to the web pane itself.
  - What Hivegrid reads from it: the user var `hivegrid_web=1` (Lua: `pane:get_user_vars()`);
    in `wezterm cli list` a web pane has `is_web: true` (the field is new in codec 47; `false`
    for every other pane), `tty_name: null`, the page URL as `cwd` (any scheme but `file:`) and
    the page title (the URL until there is one) as `title`. `Mux::resolve_cwd` ignores
    the working directory of a web pane, so a split of a terminal never starts in a page's path. The web pane is a grid pane (not `hivegrid_chrome`, not fixed
    width): numbered, arranged and zoomed with the terminals.
  - Drawing (`render/web.rs`): `paint_pane` hands a web pane that has a frame to
    `TermWindow::paint_web_pane` and skips its cells; the frame goes into a per-pane texture
    (`RenderContext::allocate_texture`, reallocated when the frame size changes, uploaded when
    the frame changed) kept in `RenderState::web` so it dies with the GL/wgpu context. After the
    zindex-0 layer, `call_draw_glium` and `call_draw_webgpu` draw it with the glyph program's
    background-image mode (`has_color = 2.0`, no new shader), so the fancy tab bar and modals
    still cover it, then a 2 px border in the pane palette's `cursor_bg` over the edge of the
    active web pane. Panes painted in a pass are visible; the ones painted in the previous pass
    and not in this one (another tab, an overlay) get `set_visible(false)`, which stops the
    screencast and hides the page (no rendering, no animation frames) until it is painted again,
    and their textures are freed. Not applied to web panes: `inactive_pane_hsb`.
  - Hook points in shared code, for rebases: `mux/src/domain.rs` (`LocalDomain::spawn_pane`, the
    first four lines), `mux/src/lib.rs` (`pub mod webpane`, the web pane filter in `resolve_cwd`),
    `mux/src/ssh.rs` (`RemoteSshDomain::spawn_pane`) and `wezterm-client/src/domain.rs`
    (`ClientDomain::spawn` and `split_pane`), two lines each for the refusals,
    `wezterm-gui/src/main.rs` (`setup_mux`: `mux::webpane::enable`; `main`: `mux::webpane::shutdown`),
    `wezterm-gui/src/frontend.rs` (`try_new`: `set_wheel_units`),
    `wezterm-gui/src/renderstate.rs` (`allocate_texture`, which `allocate_texture_atlas` now
    calls; `RenderLayer::zindex` is `pub`; the `web` field of `RenderState`),
    `termwindow/render/paint.rs` (`paint_pass`: `web.begin_pass()` and `web_end_pass()`),
    `render/pane.rs` (`paint_pane`: one early return after the background), `render/draw.rs` (one
    call after the zindex-0 layer in each of `call_draw_glium` and `call_draw_webgpu`),
    `render/mod.rs` (`pub mod web`), and the CLI `wezterm/src/cli/{split_pane,spawn_command}.rs`.
    Everything else is new files. The renderer hooks (`render/draw.rs` above all) are the
    likeliest conflicts.
  - Tests: `cargo test -p hivegrid-web -p mux` plus `cargo test -p wezterm -p wezterm-gui -- web
    split_pane spawn_command` with the environment `build-hivegrid.sh` sets (`dev/check` runs no
    cargo, so these run locally). `dev/qa/web-browser-smoke` (slot 13) covers the browser choice (`--browser`, `web.browser`) with a wrapper script around Chrome. `dev/qa/web-smoke` (local only: Xvfb, Chrome, a test
    build as `HG_WEZTERM_DIR`) drives the real GUI in the QA sandbox and checks pixels, input,
    zoom/resize/move/arrange, dialogs, crash and restart, freezing of hidden tabs, clipboard and
    that no Chrome outlives a pane or the GUI; `HGQ_FRONT_END=WebGpu` runs it through wgpu and `HGQ_XFT_DPI=144` at a scale of 1.5.
    Hivegrid's side of the contract (`config/web.lua`, `bin/hivegrid_agents.py`) is tested in the
    main repository (`tests/lua/test_web.lua`, `tests/test_agents_web.py`).
- Toast urgency (`wezterm-toast-notification/src/dbus.rs`, `wezterm-gui/src/frontend.rs`): a toast
  with a timeout is sent with Normal urgency, so GNOME Shell, dunst and similar servers honour the
  timeout. Upstream sends every toast as Critical, which those servers never expire. This covers
  `window:toast_notification` and the toasts a pane raises with OSC 9 / OSC 777 (an agent
  finishing), which get a 5 s timeout. Toasts without a timeout (download finished, update
  available, fatal errors) stay Critical.

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
