use crate::termwindow::render::TripleLayerQuadAllocator;
use crate::termwindow::{UIItem, UIItemType};
use mux::pane::Pane;
use mux::tab::{PositionedSplit, SplitDirection};
use std::sync::Arc;

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
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    pos_x + (cell_width / 2.0),
                    pos_y - (cell_height / 2.0),
                    self.render_metrics.underline_height as f32,
                    (1. + split.size as f32) * cell_height,
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
