---
tags:
  - tab_bar
---
# `tab_bar_status_scale`

*Hivegrid fork only.*

Height of the boxes the fancy tab bar draws for the left and right status (the
background of each coloured run of status text), as a multiple of the cell
height of the [window_frame](window_frame.md) font. Defaults to `1.75`. The
boxes are centred vertically in the bar, so a bar taller than this leaves room
above and below them.

```lua
config.tab_bar_height_scale = 2.6
config.tab_bar_status_scale = 1.75
```

Values noticeably below `1.75` currently clip the tops of glyphs.
