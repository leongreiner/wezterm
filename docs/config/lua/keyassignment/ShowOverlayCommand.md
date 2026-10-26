# `ShowOverlayCommand`

Runs a program in an overlay that covers the active tab, like the
[debug overlay](ShowDebugOverlay.md) does, but the overlay is a real terminal
running the program you name. When the program exits the overlay goes away and
the tab underneath is restored; the tab's panes keep running meanwhile.

The argument is a [SpawnCommand](../SpawnCommand.md): `args`, `cwd`,
`set_environment_variables` and `domain` behave as for
[SpawnCommandInNewTab](SpawnCommandInNewTab.md). When `args` is omitted the
default program of the domain runs.

```lua
config.keys = {
  {
    key = ',',
    mods = 'LEADER',
    action = wezterm.action.ShowOverlayCommand {
      args = { 'my-settings-tui' },
      cwd = wezterm.home_dir,
    },
  },
}
```

Stock WezTerm does not know this action; probe for it with
`pcall(function() return wezterm.action.ShowOverlayCommand { args = { 'true' } } end)`
and fall back to `SpawnCommandInNewTab`.

The program's own events reach the config like those of any pane: a
[user variable](../../../recipes/passing-data.md) it sets with
`OSC 1337 ; SetUserVar` fires `user-var-changed`.
