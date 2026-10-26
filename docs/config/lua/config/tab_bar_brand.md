---
tags:
  - tab_bar
---
# `tab_bar_brand`

*Hivegrid fork only.*

Draws a honeycomb mark of seven hexagons (one centre cell, six around it,
clockwise from the top) at the left end of the [fancy tab bar](use_fancy_tab_bar.md),
vertically centred in the bar. The mark is a shape drawn by the renderer, not
text, so it scales with the DPI. A click on it emits the
[status-click](../window-events/status-click.md) event with the id `brand`; the rest of
the bar still drags the window. Absent by default.

```lua
config.tab_bar_brand = {
  enabled = true, -- default true once the table is given
  size = 30, -- height and width in pixels at 96 dpi, default 28
  margin_left = 10, -- pixels at 96 dpi, default 10
  margin_right = 8, -- space before the first tab, default 8
  colors = {
    center = '#1e90ff', -- fill of the centre cell
    center_outline = '#96cdff', -- default: a lighter shade of `center`
    glow = '#1e90ff', -- glow behind the centre, default `center`
    -- Fills of the six outer cells, clockwise from the top; a shorter list repeats.
    ring = { '#1ea876', '#70aaee', '#ec6e76', '#70aaee', '#7a8eb8', '#70aaee' },
    -- Outlines in the same order; default: a lighter shade of each fill.
    ring_outline = { '#82f0c3', '#c4e0ff', '#ffbebe', '#c4e0ff', '#cddaf0', '#c4e0ff' },
  },
}
```

Stock WezTerm rejects the option, so set it with `pcall` and keep a text fallback:

```lua
local drawn = pcall(function() config.tab_bar_brand = { size = 30 } end)
```
