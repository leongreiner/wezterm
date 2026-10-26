# `status-click`

*Hivegrid fork only; stock WezTerm never emits this event.*

The `status-click` event is emitted when the user clicks a clickable region of
the tab bar's left or right status. A region is created by wrapping part of the
status text in the `Click` [wezterm.format](../wezterm/format.md) item:

```lua
wezterm.on('update-status', function(window, pane)
  window:set_right_status(wezterm.format {
    { Click = 'cpu' },
    { Background = { Color = '#335577' } },
    { Text = ' CPU ' },
    'ClickEnd',
    { Text = ' ' },
    { Click = 'clock' },
    { Text = wezterm.strftime '%H:%M' },
    'ResetAttributes',
  })
end)

wezterm.on('status-click', function(window, pane, id, button)
  if id == 'cpu' and button == 'Left' then
    window:toast_notification('wezterm', 'CPU clicked', nil, 2000)
  end
end)
```

The event parameters are:

1. a [`window` object](../window/index.md) for the gui window.
2. a [`pane` object](../pane/index.md) for the active pane in the window.
3. the string `id` given to `Click`.
4. the mouse button, one of `"Left"`, `"Right"` or `"Middle"`.

The event fires on button release, and only when the press and the release
both landed on the same region, so pressing on a region and dragging off it
cancels the click. Clicks on a region never start a window drag, and a double
click on a region does not maximize the window; it emits two events. Clicks on
the rest of the tab bar, including status text outside any region, keep their
normal behavior. The mouse cursor becomes a pointer over a region.

Both the fancy and the retro tab bar are supported. The `id` must not contain
control characters; they are dropped. Event handlers have no return value
semantics: nothing happens by default.

## Compatibility

Stock WezTerm fails on `{ Click = ... }` with an unknown `FormatItem` error.
A config that must also run there can probe once and fall back:

```lua
local has_click = pcall(wezterm.format, { { Click = 'probe' } })
```

and leave the `Click` and `ClickEnd` items out when `has_click` is false.
