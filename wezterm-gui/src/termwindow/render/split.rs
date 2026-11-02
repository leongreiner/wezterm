use crate::termwindow::render::TripleLayerQuadAllocator;
use crate::termwindow::{UIItem, UIItemType};
use mux::pane::Pane;
use mux::tab::{PositionedSplit, SplitDirection};
use std::str::FromStr;
use std::sync::Arc;
use window::color::LinearRgba;
use wezterm_term::color::SrgbaTuple;

/// The user var a program or the config sets on a pane (`OSC 1337 SetUserVar`) to colour the
/// edge of the pane: `#rrggbb`. Hivegrid's `agentstatus.lua` sets it from the agent's state.
pub const EDGE_VAR: &str = "hivegrid_edge";

/// The colour a `hivegrid_edge` value names: exactly `#rrggbb`, anything else is no edge.
pub fn edge_color(value: &str) -> Option<LinearRgba> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let c = SrgbaTuple::from_str(value).ok()?;
    Some(c.to_linear())
}

impl crate::TermWindow {
    /// True for a vertical divider whose left neighbour is a pane that set the user var
    /// `hivegrid_bar` (the Hivegrid sidebar bar).
    fn split_beside_bar(&self, split: &PositionedSplit) -> bool {
        let mux = mux::Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return false;
        };
        tab.iter_panes_ignoring_zoom().iter().any(|p| {
            p.left + p.width == split.left
                && p.top < split.top + split.size
                && split.top < p.top + p.height
                && p.pane.copy_user_vars().contains_key("hivegrid_bar")
        })
    }

    /// True for a vertical divider with a pane on either side whose default background
    /// differs from the window's (set with OSC 11, as the Hivegrid bar and overview dock do).
    /// Such a pane paints the window padding above and below it, so its divider has to run
    /// through the padding as well, or the colour edge would show a notch without a line.
    fn split_beside_painted_pane(
        &self,
        split: &PositionedSplit,
        window_bg: wezterm_term::color::SrgbaTuple,
    ) -> bool {
        let mux = mux::Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return false;
        };
        tab.iter_panes_ignoring_zoom().iter().any(|p| {
            (p.left + p.width == split.left || p.left == split.left + 1)
                && p.top < split.top + split.size
                && split.top < p.top + p.height
                && p.pane.palette().background != window_bg
        })
    }

    pub fn paint_split(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        split: &PositionedSplit,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<()> {
        let palette = pane.palette();
        let foreground = palette.split.to_linear();
        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;

        let border = self.get_os_border();
        let first_row_offset = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height()?
        } else {
            0.
        } + border.top.get() as f32;

        let (padding_left, padding_top) = self.padding_left_top();

        let pos_y = split.top as f32 * cell_height + first_row_offset + padding_top;
        let pos_x = split.left as f32 * cell_width + padding_left + border.left.get() as f32;

        if split.direction == SplitDirection::Horizontal {
            let mut top = pos_y - (cell_height / 2.0);
            let mut bottom = top + (1. + split.size as f32) * cell_height;
            let window_bg = self.palette().background;
            if self.split_beside_painted_pane(split, window_bg) {
                if split.top == 0 {
                    top = first_row_offset;
                }
                if split.top + split.size >= self.terminal_size.rows as usize {
                    let bottom_bar = if self.show_tab_bar && self.config.tab_bar_at_bottom {
                        self.tab_bar_pixel_height()?
                    } else {
                        0.
                    };
                    bottom = self.dimensions.pixel_height as f32
                        - border.bottom.get() as f32
                        - bottom_bar;
                }
            }
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    pos_x + (cell_width / 2.0),
                    top,
                    self.render_metrics.underline_height as f32,
                    bottom - top,
                ),
                foreground,
            )?;
            // The line is drawn, but a divider next to the sidebar bar is not a hit target:
            // the bar has a fixed width, so a resize cursor there would promise too much.
            if !self.split_beside_bar(split) {
                self.ui_items.push(UIItem {
                    x: border.left.get() as usize
                        + padding_left as usize
                        + (split.left * cell_width as usize),
                    width: cell_width as usize,
                    y: padding_top as usize
                        + first_row_offset as usize
                        + split.top * cell_height as usize,
                    height: split.size * cell_height as usize,
                    item_type: UIItemType::Split(split.clone()),
                });
            }
        } else {
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    pos_x - (cell_width / 2.0),
                    pos_y + (cell_height / 2.0),
                    (1.0 + split.size as f32) * cell_width,
                    self.render_metrics.underline_height as f32,
                ),
                foreground,
            )?;
            self.ui_items.push(UIItem {
                x: border.left.get() as usize
                    + padding_left as usize
                    + (split.left * cell_width as usize),
                width: split.size * cell_width as usize,
                y: padding_top as usize
                    + first_row_offset as usize
                    + split.top * cell_height as usize,
                height: cell_height as usize,
                item_type: UIItemType::Split(split.clone()),
            });
        }

        Ok(())
    }
}

impl crate::TermWindow {
    /// A coloured frame over the edge of every pane that has the user var `hivegrid_edge`
    /// (`#rrggbb`), drawn after the dividers so the two halves of a divider keep the colours of
    /// the two panes beside it. The strips lie inside the pane's background rectangle, which
    /// reaches half a divider cell into each divider and the window edge on an outer side, and
    /// are not dimmed with the pane (`inactive_pane_hsb`).
    pub fn paint_pane_edges(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        let panes = self.get_panes_to_render();
        if !panes
            .iter()
            .any(|p| p.pane.copy_user_vars().contains_key(EDGE_VAR))
        {
            return Ok(());
        }
        let cell_width = self.render_metrics.cell_size.width as f32;
        let border = self.get_os_border();
        let tab_bar = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height()?
        } else {
            0.
        };
        let (padding_left, padding_top) = self.padding_left_top();
        let top_pixel_y = tab_bar + border.top.get() as f32 + padding_top;
        for pos in &panes {
            let vars = pos.pane.copy_user_vars();
            let Some(color) = vars.get(EDGE_VAR).and_then(|v| edge_color(v)) else {
                continue;
            };
            let r = self.pane_background_rect(
                pos,
                padding_left,
                padding_top,
                top_pixel_y,
                border.left.get() as f32,
            );
            let t = (cell_width / 4.0)
                .round()
                .max(2.0)
                .min(r.size.width / 2.0)
                .min(r.size.height / 2.0);
            for strip in [
                euclid::rect(r.min_x(), r.min_y(), r.size.width, t),
                euclid::rect(r.min_x(), r.max_y() - t, r.size.width, t),
                euclid::rect(r.min_x(), r.min_y() + t, t, r.size.height - 2.0 * t),
                euclid::rect(r.max_x() - t, r.min_y() + t, t, r.size.height - 2.0 * t),
            ] {
                self.filled_rectangle(layers, 2, strip, color)?;
            }
        }
        Ok(())
    }

    /// While a pane is dragged (config `pane_drag_modifiers`): the dragged pane is veiled
    /// and the drop target is outlined in the cursor colour, filled all over for a swap
    /// (centre) or only over the half the pane would take for an edge drop.
    pub fn paint_pane_drag(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        use crate::termwindow::DropZone;
        let Some(drag) = self.pane_drag.clone().filter(|d| d.active) else {
            return Ok(());
        };
        let panes = self.get_panes_to_render();
        let Some(active) = panes.iter().find(|p| p.is_active) else {
            return Ok(());
        };
        let accent = active.pane.palette().cursor_bg.to_linear();
        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let border = self.get_os_border();
        let first_row_offset = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height()?
        } else {
            0.
        } + border.top.get() as f32;
        let (padding_left, padding_top) = self.padding_left_top();
        let origin_x = padding_left + border.left.get() as f32;
        let origin_y = padding_top + first_row_offset;
        let rect_of = |pos: &mux::tab::PositionedPane| {
            (
                origin_x + pos.left as f32 * cell_width,
                origin_y + pos.top as f32 * cell_height,
                pos.width as f32 * cell_width,
                pos.height as f32 * cell_height,
            )
        };

        if let Some(pos) = panes.iter().find(|p| p.pane.pane_id() == drag.source) {
            let (x, y, w, h) = rect_of(pos);
            self.filled_rectangle(layers, 2, euclid::rect(x, y, w, h), accent.mul_alpha(0.12))?;
        }
        if let Some((id, zone)) = drag.target {
            if let Some(pos) = panes.iter().find(|p| p.pane.pane_id() == id) {
                let (x, y, w, h) = rect_of(pos);
                let (fx, fy, fw, fh) = match zone {
                    DropZone::Center => (x, y, w, h),
                    DropZone::Left => (x, y, w / 2.0, h),
                    DropZone::Right => (x + w / 2.0, y, w / 2.0, h),
                    DropZone::Top => (x, y, w, h / 2.0),
                    DropZone::Bottom => (x, y + h / 2.0, w, h / 2.0),
                };
                self.filled_rectangle(
                    layers,
                    2,
                    euclid::rect(fx, fy, fw, fh),
                    accent.mul_alpha(0.3),
                )?;
                let t = (self.render_metrics.underline_height as f32).max(2.0);
                for r in [
                    euclid::rect(x, y, w, t),
                    euclid::rect(x, y + h - t, w, t),
                    euclid::rect(x, y, t, h),
                    euclid::rect(x + w - t, y, t, h),
                ] {
                    self.filled_rectangle(layers, 2, r, accent)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::edge_color;

    #[test]
    fn edge_value_is_exactly_a_hex_colour() {
        assert!(edge_color("#4ade80").is_some());
        assert!(edge_color("#A78BFA").is_some());
        for bad in ["", "4ade80", "#4ade8", "#4ade800", "#4adg80", "green", "rgb(1,2,3)"] {
            assert!(edge_color(bad).is_none(), "{bad:?} must not be an edge colour");
        }
    }
}
