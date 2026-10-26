---
title: wezterm.format
tags:
 - utility
 - string
---

# `wezterm.format({})`

{{since('20210314-114017-04b7cedd')}}

`wezterm.format` can be used to produce a formatted string
with terminal graphic attributes such as bold, italic and colors.
The resultant string is rendered into a string with wezterm
compatible escape sequences embedded.

`wezterm.format` accepts a single array argument, where each
element is a `FormatItem`.

This example logs the text `Hello`, then the date/time, underlined, in purple
text on a blue background to the stderr of the wezterm process:

```lua
local wezterm = require 'wezterm'

local success, date, stderr = wezterm.run_child_process { 'date' }

wezterm.log_info(wezterm.format {
  { Attribute = { Underline = 'Single' } },
  { Foreground = { AnsiColor = 'Fuchsia' } },
  { Background = { Color = 'blue' } },
  { Text = 'Hello ' .. date .. ' ' },
  'ResetAttributes',
  { Text = 'this text has default attributes' },
})
```

Possible values for the `FormatItem` elements are:

* `{Text="Hello"}` - the text `Hello`. The string can be any string expression,
  including escape sequences that are not supported directly by
  `wezterm.format`.
* `{Attribute={Underline="None"}}` - disable underline
* `{Attribute={Underline="Single"}}` - enable single underline
* `{Attribute={Underline="Double"}}` - enable double underline
* `{Attribute={Underline="Curly"}}` - enable curly underline
* `{Attribute={Underline="Dotted"}}` - enable dotted underline
* `{Attribute={Underline="Dashed"}}` - enable dashed underline
* `{Attribute={Intensity="Normal"}}` - set normal intensity
* `{Attribute={Intensity="Bold"}}` - set bold intensity
* `{Attribute={Intensity="Half"}}` - set half intensity
* `{Attribute={Italic=true}}` - enable italics
* `{Attribute={Italic=false}}` - disable italics
* `{Foreground={AnsiColor="Black"}}` - set foreground color to one of the ansi color palette values (index 0-15) using one of the names `Black`, `Maroon`, `Green`, `Olive`, `Navy`, `Purple`, `Teal`, `Silver`, `Grey`, `Red`, `Lime`, `Yellow`, `Blue`, `Fuchsia`, `Aqua` or `White`.
* `{Foreground={Color="yellow"}}` - set foreground color to a named color or rgb value like `#ffffff`.
* `{Background={AnsiColor="Black"}}` - set the background color to an ansi color as per `Foreground` above.
* `{Background={Color="blue"}}` - set the background color to a named color or rgb value as per `Foreground` above.
* `"ResetAttributes"` - reset all attributes to default. {{since('20220807-113146-c2fee766', inline=True)}}
* `{Click="id"}` - start a clickable region named `id`. This only has an
  effect in the status text set with
  [window:set_right_status](../window/set_right_status.md) and
  [window:set_left_status](../window/set_left_status.md); see
  [status-click](../window-events/status-click.md). The region lasts until
  `"ClickEnd"` or `"ResetAttributes"`. (Hivegrid fork only; stock WezTerm
  rejects the item as an unknown `FormatItem`.)
* `"ClickEnd"` - end the region started by `Click`, leaving colors and other
  attributes unchanged. (Hivegrid fork only.)

This example shows how to use arbitrary escape sequences to change the underline color:

```lua
local wezterm = require 'wezterm'
wezterm.log_info(wezterm.format {
  -- turn on underlines
  { Attribute = { Underline = 'Single' } },
  -- make the underline red
  { Text = '\x1b[58:2::255:0:0m' },
  -- and say hello
  { Text = 'hello' },
})
```
