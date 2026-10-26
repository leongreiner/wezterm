use crate::domain::DomainId;
use crate::pane::*;
use crate::renderable::StableCursorPosition;
use crate::{Mux, MuxNotification, WindowId};
use bintree::PathBranch;
use config::configuration;
use config::keyassignment::PaneDirection;
use parking_lot::Mutex;
use rangeset::intersects_range;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryInto;
use std::sync::Arc;
use url::Url;
use wezterm_term::{StableRowIndex, TerminalSize};

pub type Tree = bintree::Tree<Arc<dyn Pane>, SplitDirectionAndSize>;
pub type Cursor = bintree::Cursor<Arc<dyn Pane>, SplitDirectionAndSize>;

static TAB_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type TabId = usize;

#[derive(Default)]
struct Recency {
    count: usize,
    by_idx: HashMap<usize, usize>,
}

impl Recency {
    fn tag(&mut self, idx: usize) {
        self.by_idx.insert(idx, self.count);
        self.count += 1;
    }

    fn score(&self, idx: usize) -> usize {
        self.by_idx.get(&idx).copied().unwrap_or(0)
    }
}

struct TabInner {
    id: TabId,
    pane: Option<Tree>,
    size: TerminalSize,
    active: usize,
    zoomed: Option<Arc<dyn Pane>>,
    title: String,
    recency: Recency,
}

/// A Tab is a container of Panes
pub struct Tab {
    inner: Mutex<TabInner>,
    tab_id: TabId,
}

#[derive(Clone)]
pub struct PositionedPane {
    /// The topological pane index that can be used to reference this pane
    pub index: usize,
    /// true if this is the active pane at the time the position was computed
    pub is_active: bool,
    /// true if this pane is zoomed
    pub is_zoomed: bool,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub top: usize,
    /// The width of this pane in cells
    pub width: usize,
    pub pixel_width: usize,
    /// The height of this pane in cells
    pub height: usize,
    pub pixel_height: usize,
    /// The pane instance
    pub pane: Arc<dyn Pane>,
}

impl std::fmt::Debug for PositionedPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedPane")
            .field("index", &self.index)
            .field("is_active", &self.is_active)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pane_id", &self.pane.pane_id())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// The size is of the (first, second) child of the split
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitSize {
    Cells(usize),
    Percent(u8),
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::Percent(50)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitRequest {
    pub direction: SplitDirection,
    /// Whether the newly created item will be in the second part
    /// of the split (right/bottom)
    pub target_is_second: bool,
    /// Split across the top of the tab rather than the active pane
    pub top_level: bool,
    /// The size of the new item
    pub size: SplitSize,
}

impl Default for SplitRequest {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            top_level: false,
            size: SplitSize::default(),
        }
    }
}

impl SplitDirectionAndSize {
    fn top_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => 0,
            SplitDirection::Vertical => self.first.rows as usize + 1,
        }
    }

    fn left_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self.first.cols as usize + 1,
            SplitDirection::Vertical => 0,
        }
    }

    pub fn width(&self) -> usize {
        if self.direction == SplitDirection::Horizontal {
            self.first.cols + self.second.cols + 1
        } else {
            self.first.cols
        }
    }

    pub fn height(&self) -> usize {
        if self.direction == SplitDirection::Vertical {
            self.first.rows + self.second.rows + 1
        } else {
            self.first.rows
        }
    }

    pub fn size(&self) -> TerminalSize {
        let cell_width = self.first.pixel_width / self.first.cols;
        let cell_height = self.first.pixel_height / self.first.rows;

        let rows = self.height();
        let cols = self.width();

        TerminalSize {
            rows,
            cols,
            pixel_height: cell_height * rows,
            pixel_width: cell_width * cols,
            dpi: self.first.dpi,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PositionedSplit {
    /// The topological node index that can be used to reference this split
    pub index: usize,
    pub direction: SplitDirection,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub top: usize,
    /// For Horizontal splits, how tall the split should be, for Vertical
    /// splits how wide it should be
    pub size: usize,
}

fn is_pane(pane: &Arc<dyn Pane>, other: &Option<&Arc<dyn Pane>>) -> bool {
    if let Some(other) = other {
        other.pane_id() == pane.pane_id()
    } else {
        false
    }
}

fn pane_tree(
    tree: &Tree,
    tab_id: TabId,
    window_id: WindowId,
    active: Option<&Arc<dyn Pane>>,
    zoomed: Option<&Arc<dyn Pane>>,
    workspace: &str,
    left_col: usize,
    top_row: usize,
) -> PaneNode {
    match tree {
        Tree::Empty => PaneNode::Empty,
        Tree::Node { left, right, data } => {
            let data = data.unwrap();
            PaneNode::Split {
                left: Box::new(pane_tree(
                    &*left, tab_id, window_id, active, zoomed, workspace, left_col, top_row,
                )),
                right: Box::new(pane_tree(
                    &*right,
                    tab_id,
                    window_id,
                    active,
                    zoomed,
                    workspace,
                    if data.direction == SplitDirection::Vertical {
                        left_col
                    } else {
                        left_col + data.left_of_second()
                    },
                    if data.direction == SplitDirection::Horizontal {
                        top_row
                    } else {
                        top_row + data.top_of_second()
                    },
                )),
                node: data,
            }
        }
        Tree::Leaf(pane) => {
            let dims = pane.get_dimensions();
            let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
            let cursor_pos = pane.get_cursor_position();

            PaneNode::Leaf(PaneEntry {
                window_id,
                tab_id,
                pane_id: pane.pane_id(),
                title: pane.get_title(),
                is_active_pane: is_pane(pane, &active),
                is_zoomed_pane: is_pane(pane, &zoomed),
                size: TerminalSize {
                    cols: dims.cols,
                    rows: dims.viewport_rows,
                    pixel_height: dims.pixel_height,
                    pixel_width: dims.pixel_width,
                    dpi: dims.dpi,
                },
                working_dir: working_dir.map(Into::into),
                workspace: workspace.to_string(),
                cursor_pos,
                physical_top: dims.physical_top,
                left_col,
                top_row,
                tty_name: pane.tty_name(),
            })
        }
    }
}

fn build_from_pane_tree<F>(
    tree: bintree::Tree<PaneEntry, SplitDirectionAndSize>,
    active: &mut Option<Arc<dyn Pane>>,
    zoomed: &mut Option<Arc<dyn Pane>>,
    make_pane: &mut F,
) -> Tree
where
    F: FnMut(PaneEntry) -> Arc<dyn Pane>,
{
    match tree {
        bintree::Tree::Empty => Tree::Empty,
        bintree::Tree::Node { left, right, data } => Tree::Node {
            left: Box::new(build_from_pane_tree(*left, active, zoomed, make_pane)),
            right: Box::new(build_from_pane_tree(*right, active, zoomed, make_pane)),
            data,
        },
        bintree::Tree::Leaf(entry) => {
            let is_zoomed_pane = entry.is_zoomed_pane;
            let is_active_pane = entry.is_active_pane;
            let pane = make_pane(entry);
            if is_zoomed_pane {
                zoomed.replace(Arc::clone(&pane));
            }
            if is_active_pane {
                active.replace(Arc::clone(&pane));
            }
            Tree::Leaf(pane)
        }
    }
}

/// Computes the minimum (x, y) size based on the panes in this portion
/// of the tree.
fn compute_min_size(tree: &Tree) -> (usize, usize) {
    match tree {
        Tree::Node { data: None, .. } | Tree::Empty => (1, 1),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (left_x, left_y) = compute_min_size(left);
            let (right_x, right_y) = compute_min_size(right);
            match data.direction {
                SplitDirection::Vertical => (left_x.max(right_x), left_y + right_y + 1),
                SplitDirection::Horizontal => (left_x + right_x + 1, left_y.max(right_y)),
            }
        }
        Tree::Leaf(_) => (1, 1),
    }
}

/// True for a leaf whose pane asked to keep its width when the tab is resized: the user var
/// `hivegrid_fixed_width` (the Hivegrid sidebar bar, sidebar views and overview dock), and for
/// a split whose leaves all did (the bar with a sidebar view beside it).
fn is_fixed_width(tree: &Tree) -> bool {
    match tree {
        Tree::Leaf(pane) => pane.copy_user_vars().contains_key("hivegrid_fixed_width"),
        Tree::Node {
            left,
            right,
            data: Some(_),
        } => is_fixed_width(left) && is_fixed_width(right),
        _ => false,
    }
}

/// True for a Hivegrid chrome pane: one that set the user var `hivegrid_chrome` or
/// `hivegrid_fixed_width`. Chrome stays visible while another pane is zoomed.
fn is_chrome_pane(pane: &Arc<dyn Pane>) -> bool {
    let vars = pane.copy_user_vars();
    vars.contains_key("hivegrid_chrome") || vars.contains_key("hivegrid_fixed_width")
}

/// True for a subtree made of chrome panes only.
fn is_all_chrome(tree: &Tree) -> bool {
    match tree {
        Tree::Leaf(pane) => is_chrome_pane(pane),
        Tree::Node {
            left,
            right,
            data: Some(_),
        } => is_all_chrome(left) && is_all_chrome(right),
        _ => false,
    }
}

/// A rectangle of cells within a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellRect {
    left: usize,
    top: usize,
    cols: usize,
    rows: usize,
}

impl CellRect {
    fn contains(&self, left: usize, top: usize) -> bool {
        left >= self.left
            && left < self.left + self.cols
            && top >= self.top
            && top < self.top + self.rows
    }
}

/// The part of a tab of `size` that is not chrome: from the root, every split with a
/// chrome-only subtree on one side is stepped over to the other side. None when the root is
/// not such a split (no chrome at the edges of the tab).
fn non_chrome_area(tree: &Tree, size: &TerminalSize) -> Option<CellRect> {
    let mut area = CellRect {
        left: 0,
        top: 0,
        cols: size.cols,
        rows: size.rows,
    };
    let mut node = tree;
    let mut stepped = false;
    while let Tree::Node {
        left,
        right,
        data: Some(data),
    } = node
    {
        match (is_all_chrome(left), is_all_chrome(right)) {
            (true, false) => {
                area.left += data.left_of_second();
                area.top += data.top_of_second();
                area.cols = data.second.cols;
                area.rows = data.second.rows;
                node = right;
            }
            (false, true) => {
                area.cols = data.first.cols;
                area.rows = data.first.rows;
                node = left;
            }
            _ => break,
        }
        stepped = true;
    }
    stepped.then_some(area)
}

/// The number of panes side by side along `direction` (columns for Horizontal, rows for
/// Vertical) in the widest/tallest part of a subtree.
fn spans(tree: &Tree, direction: SplitDirection) -> usize {
    match tree {
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (l, r) = (spans(left, direction), spans(right, direction));
            if data.direction == direction {
                l + r
            } else {
                l.max(r)
            }
        }
        _ => 1,
    }
}

/// Splits a size change of `delta` cells between the two sides of a split, each given as
/// (current size, minimum size, fixed, panes side by side). The sides change in proportion
/// to their sizes, so the columns of a grid stay equal however the tree is nested (upstream
/// alternated single cells between the sides, which left the first column of a nested grid
/// with half of the change); the cell the proportion leaves over goes where the panes stay
/// most even (a shrink to the side with the wider panes on average), so a resize in steps of
/// one cell does not all land on the last column. A fixed side takes no share while the
/// other side can absorb the change. Neither side goes below its minimum. Returns the
/// (first, second) deltas.
fn share_delta(
    delta: isize,
    (first, first_min, first_fixed, first_n): (usize, usize, bool, usize),
    (second, second_min, second_fixed, second_n): (usize, usize, bool, usize),
) -> (isize, isize) {
    let (first, second) = (first as isize, second as isize);
    let (first_floor, second_floor) = (first_min as isize - first, second_min as isize - second);
    let mut a = match (first_fixed, second_fixed) {
        (true, false) => 0,
        (false, true) => delta,
        // Both fixed (a chrome-only subtree that has to take a change, e.g. a dragged divider
        // between a sidebar view and the grid): the wider side takes it, the narrow bar keeps
        // its width.
        (true, true) if first >= second => delta,
        (true, true) => 0,
        _ if first + second > 0 => {
            let a = delta * first / (first + second);
            let b = delta * second / (first + second);
            let rest = delta - a - b;
            // average pane size of each side after the proportional share (dividers between
            // its panes excluded), cross-multiplied
            let (first_n, second_n) = (first_n as isize, second_n as isize);
            let (avg_a, avg_b) = (
                (first + a - (first_n - 1)) * second_n,
                (second + b - (second_n - 1)) * first_n,
            );
            if (rest < 0 && avg_a > avg_b) || (rest > 0 && avg_a < avg_b) {
                a + rest
            } else {
                a
            }
        }
        _ => delta / 2,
    }
    .max(first_floor);
    let mut b = delta - a;
    if b < second_floor {
        b = second_floor;
        a = (delta - b).max(first_floor);
    }
    (a, b)
}

fn adjust_x_size(tree: &mut Tree, mut x_adjust: isize, cell_dimensions: &TerminalSize) {
    let (min_x, _) = compute_min_size(tree);
    while x_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Vertical => {
                        let new_cols = (data.first.cols as isize)
                            .saturating_add(x_adjust)
                            .max(min_x as isize);
                        x_adjust = new_cols.saturating_sub(data.first.cols as isize);

                        if x_adjust != 0 {
                            adjust_x_size(&mut *left, x_adjust, cell_dimensions);
                            data.first.cols = new_cols.try_into().unwrap();
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);

                            adjust_x_size(&mut *right, x_adjust, cell_dimensions);
                            data.second.cols = data.first.cols;
                            data.second.pixel_width = data.first.pixel_width;
                        }
                        return;
                    }
                    SplitDirection::Horizontal => {
                        let (first_min, _) = compute_min_size(left);
                        let (second_min, _) = compute_min_size(right);
                        let (first_delta, second_delta) = share_delta(
                            x_adjust,
                            (
                                data.first.cols,
                                first_min,
                                is_fixed_width(left),
                                spans(left, SplitDirection::Horizontal),
                            ),
                            (
                                data.second.cols,
                                second_min,
                                is_fixed_width(right),
                                spans(right, SplitDirection::Horizontal),
                            ),
                        );
                        if first_delta != 0 {
                            adjust_x_size(&mut *left, first_delta, cell_dimensions);
                            data.first.cols = (data.first.cols as isize + first_delta) as usize;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                        }
                        if second_delta != 0 {
                            adjust_x_size(&mut *right, second_delta, cell_dimensions);
                            data.second.cols = (data.second.cols as isize + second_delta) as usize;
                            data.second.pixel_width =
                                data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                        }
                        return;
                    }
                }
            }
        }
    }
}

fn adjust_y_size(tree: &mut Tree, mut y_adjust: isize, cell_dimensions: &TerminalSize) {
    let (_, min_y) = compute_min_size(tree);
    while y_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Horizontal => {
                        let new_rows = (data.first.rows as isize)
                            .saturating_add(y_adjust)
                            .max(min_y as isize);
                        y_adjust = new_rows.saturating_sub(data.first.rows as isize);

                        if y_adjust != 0 {
                            adjust_y_size(&mut *left, y_adjust, cell_dimensions);
                            data.first.rows = new_rows.try_into().unwrap();
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);

                            adjust_y_size(&mut *right, y_adjust, cell_dimensions);
                            data.second.rows = data.first.rows;
                            data.second.pixel_height = data.first.pixel_height;
                        }
                        return;
                    }
                    SplitDirection::Vertical => {
                        let (_, first_min) = compute_min_size(left);
                        let (_, second_min) = compute_min_size(right);
                        let (first_delta, second_delta) = share_delta(
                            y_adjust,
                            (
                                data.first.rows,
                                first_min,
                                false,
                                spans(left, SplitDirection::Vertical),
                            ),
                            (
                                data.second.rows,
                                second_min,
                                false,
                                spans(right, SplitDirection::Vertical),
                            ),
                        );
                        if first_delta != 0 {
                            adjust_y_size(&mut *left, first_delta, cell_dimensions);
                            data.first.rows = (data.first.rows as isize + first_delta) as usize;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                        }
                        if second_delta != 0 {
                            adjust_y_size(&mut *right, second_delta, cell_dimensions);
                            data.second.rows = (data.second.rows as isize + second_delta) as usize;
                            data.second.pixel_height =
                                data.second.rows.saturating_mul(cell_dimensions.pixel_height);
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Resizes every pane to its size in the tree, except `skip` (the zoomed pane, sized apart).
fn apply_sizes_from_splits(tree: &Tree, size: &TerminalSize, skip: Option<PaneId>) {
    match tree {
        Tree::Empty => return,
        Tree::Node { data: None, .. } => return,
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            apply_sizes_from_splits(&*left, &data.first, skip);
            apply_sizes_from_splits(&*right, &data.second, skip);
        }
        Tree::Leaf(pane) => {
            if Some(pane.pane_id()) != skip {
                resize_if_changed(pane, *size);
            }
        }
    }
}

/// Resizes a pane unless it already has that size: a terminal resize rewraps the whole
/// scrollback, and the split tree is applied to every pane after any divider move.
fn resize_if_changed(pane: &Arc<dyn Pane>, size: TerminalSize) {
    let dims = pane.get_dimensions();
    if dims.cols != size.cols
        || dims.viewport_rows != size.rows
        || dims.pixel_width != size.pixel_width
        || dims.pixel_height != size.pixel_height
        || dims.dpi != size.dpi
    {
        pane.resize(size).ok();
    }
}

/// Makes every split fit the size its parent gives it, after a divider moved: the change is
/// shared out with `adjust_x_size`/`adjust_y_size` (in proportion, fixed-width chrome kept,
/// never below a subtree's minimum). Upstream kept the first side of each split and squeezed
/// the second, so a side that could not shrink (a 3-column grid next to the dock) overflowed
/// the tab and pushed the panes after it out of the window.
fn fit_subtrees(tree: &mut Tree, cell_dimensions: &TerminalSize) {
    if let Tree::Node {
        left,
        right,
        data: Some(data),
    } = tree
    {
        for (child, want) in [(&mut **left, data.first), (&mut **right, data.second)] {
            if let Tree::Node {
                data: Some(have), ..
            } = child
            {
                let (cols, rows) = (have.width(), have.height());
                adjust_x_size(child, want.cols as isize - cols as isize, cell_dimensions);
                adjust_y_size(child, want.rows as isize - rows as isize, cell_dimensions);
            }
            fit_subtrees(child, cell_dimensions);
        }
    }
}

fn cell_dimensions(size: &TerminalSize) -> TerminalSize {
    TerminalSize {
        rows: 1,
        cols: 1,
        pixel_width: size.pixel_width / size.cols,
        pixel_height: size.pixel_height / size.rows,
        dpi: size.dpi,
    }
}

impl Tab {
    pub fn new(size: &TerminalSize) -> Self {
        let inner = TabInner::new(size);
        let tab_id = inner.id;
        Self {
            inner: Mutex::new(inner),
            tab_id,
        }
    }

    pub fn get_title(&self) -> String {
        self.inner.lock().title.clone()
    }

    pub fn set_title(&self, title: &str) {
        let mut inner = self.inner.lock();
        if inner.title != title {
            inner.title = title.to_string();
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::TabTitleChanged {
                    tab_id: inner.id,
                    title: title.to_string(),
                })
            });
        }
    }

    /// Called by the multiplexer client when building a local tab to
    /// mirror a remote tab.  The supplied `root` is the information
    /// about our counterpart in the remote server.
    /// This method builds a local tree based on the remote tree which
    /// then replaces the local tree structure.
    ///
    /// The `make_pane` function is provided by the caller, and its purpose
    /// is to lookup an existing Pane that corresponds to the provided
    /// PaneEntry, or to create a new Pane from that entry.
    /// make_pane is expected to add the pane to the mux if it creates
    /// a new pane, otherwise the pane won't poll/update in the GUI.
    pub fn sync_with_pane_tree<F>(&self, size: TerminalSize, root: PaneNode, make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        self.inner.lock().sync_with_pane_tree(size, root, make_pane)
    }

    pub fn codec_pane_tree(&self) -> PaneNode {
        self.inner.lock().codec_pane_tree()
    }

    /// Returns a count of how many panes are in this tab
    pub fn count_panes(&self) -> Option<usize> {
        self.inner.try_lock().map(|mut inner| inner.count_panes())
    }

    /// Sets the zoom state, returns the prior state
    pub fn set_zoomed(&self, zoomed: bool) -> bool {
        self.inner.lock().set_zoomed(zoomed)
    }

    pub fn toggle_zoom(&self) {
        self.inner.lock().toggle_zoom()
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        self.inner.lock().contains_pane(pane)
    }

    pub fn iter_panes(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes()
    }

    pub fn iter_panes_ignoring_zoom(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes_ignoring_zoom()
    }

    pub fn rotate_counter_clockwise(&self) {
        self.inner.lock().rotate_counter_clockwise()
    }

    pub fn rotate_clockwise(&self) {
        self.inner.lock().rotate_clockwise()
    }

    pub fn iter_splits(&self) -> Vec<PositionedSplit> {
        self.inner.lock().iter_splits()
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn get_size(&self) -> TerminalSize {
        self.inner.lock().get_size()
    }

    /// Apply the new size of the tab to the panes contained within.
    /// The delta between the current and the new size is computed,
    /// and is distributed between the splits.  For small resizes
    /// this algorithm biases towards adjusting the left/top nodes
    /// first.  For large resizes this tends to proportionally adjust
    /// the relative sizes of the elements in a split.
    pub fn resize(&self, size: TerminalSize) {
        self.inner.lock().resize(size)
    }

    /// Called when running in the mux server after an individual pane
    /// has been resized.
    /// Because the split manipulation happened on the GUI we "lost"
    /// the information that would have allowed us to call resize_split_by()
    /// and instead need to back-infer the split size information.
    /// We rely on the client to have resized (or be in the process
    /// of resizing) affected panes consistently with its own Tab
    /// tree model.
    /// This method does a simple tree walk to the leaves to back-propagate
    /// the size of the panes up to their containing node split data.
    /// Without this step, disconnecting and reconnecting would cause
    /// the GUI to use stale size information for the window it spawns
    /// to attach this tab.
    pub fn rebuild_splits_sizes_from_contained_panes(&self) {
        self.inner
            .lock()
            .rebuild_splits_sizes_from_contained_panes()
    }

    /// Given split_index, the topological index of a split returned by
    /// iter_splits() as PositionedSplit::index, revised the split position
    /// by the provided delta; positive values move the split to the right/bottom,
    /// and negative values to the left/top.
    /// The adjusted size is propagated downwards to contained children and
    /// their panes are resized accordingly.
    pub fn resize_split_by(&self, split_index: usize, delta: isize) {
        self.inner.lock().resize_split_by(split_index, delta)
    }

    /// Adjusts the size of the active pane in the specified direction
    /// by the specified amount.
    pub fn adjust_pane_size(&self, direction: PaneDirection, amount: usize) {
        self.inner.lock().adjust_pane_size(direction, amount)
    }

    /// Like adjust_pane_size, for the pane `pane_id` instead of the active one (`wezterm cli
    /// adjust-pane-size --pane-id` resized the active pane whatever id it was given).
    pub fn adjust_pane_size_of(&self, pane_id: PaneId, direction: PaneDirection, amount: usize) {
        self.inner.lock().adjust_pane_size_of(pane_id, direction, amount)
    }

    /// Activate an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn activate_pane_direction(&self, direction: PaneDirection) {
        self.inner.lock().activate_pane_direction(direction)
    }

    /// Returns an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn get_pane_direction(&self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        self.inner.lock().get_pane_direction(direction, ignore_zoom)
    }

    pub fn prune_dead_panes(&self) -> bool {
        self.inner.lock().prune_dead_panes()
    }

    pub fn kill_pane(&self, pane_id: PaneId) -> bool {
        self.inner.lock().kill_pane(pane_id)
    }

    pub fn kill_panes_in_domain(&self, domain: DomainId) -> bool {
        self.inner.lock().kill_panes_in_domain(domain)
    }

    /// Remove pane from tab.
    /// The pane is still live in the mux; the intent is for the pane to
    /// be added to a different tab.
    pub fn remove_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.inner.lock().remove_pane(pane_id)
    }

    pub fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        self.inner.lock().can_close_without_prompting(reason)
    }

    pub fn is_dead(&self) -> bool {
        self.inner.lock().is_dead()
    }

    pub fn get_active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_active_pane()
    }

    #[allow(unused)]
    pub fn get_active_idx(&self) -> usize {
        self.inner.lock().get_active_idx()
    }

    pub fn set_active_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().set_active_pane(pane)
    }

    pub fn set_active_idx(&self, pane_index: usize) {
        self.inner.lock().set_active_idx(pane_index)
    }

    /// Assigns the root pane.
    /// This is suitable when creating a new tab and then assigning
    /// the initial pane
    pub fn assign_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().assign_pane(pane)
    }

    /// Swap the active pane with the specified pane_index
    pub fn swap_active_with_index(&self, pane_index: usize, keep_focus: bool) -> Option<()> {
        self.inner
            .lock()
            .swap_active_with_index(pane_index, keep_focus)
    }

    /// Computes the size of the pane that would result if the specified
    /// pane was split in a particular direction.
    /// The intent is to call this prior to spawning the new pane so that
    /// you can create it with the correct size.
    /// May return None if the specified pane_index is invalid.
    pub fn compute_split_size(
        &self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        self.inner.lock().compute_split_size(pane_index, request)
    }

    /// Split the pane that has pane_index in the given direction and assign
    /// the right/bottom pane of the newly created split to the provided Pane
    /// instance.  Returns the resultant index of the newly inserted pane.
    /// Both the split and the inserted pane will be resized.
    pub fn split_and_insert(
        &self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        self.inner
            .lock()
            .split_and_insert(pane_index, request, pane)
    }

    pub fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_zoomed_pane()
    }
}

impl TabInner {
    fn new(size: &TerminalSize) -> Self {
        Self {
            id: TAB_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            pane: Some(Tree::new()),
            size: *size,
            active: 0,
            zoomed: None,
            title: String::new(),
            recency: Recency::default(),
        }
    }

    fn sync_with_pane_tree<F>(&mut self, size: TerminalSize, root: PaneNode, mut make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        let mut active = None;
        let mut zoomed = None;

        log::debug!("sync_with_pane_tree with size {:?}", size);

        let t = build_from_pane_tree(root.into_tree(), &mut active, &mut zoomed, &mut make_pane);
        let mut cursor = t.cursor();

        self.active = 0;
        if let Some(active) = active {
            // Resolve the active pane to its index
            let mut index = 0;
            loop {
                if let Some(pane) = cursor.leaf_mut() {
                    if active.pane_id() == pane.pane_id() {
                        // Found it
                        self.active = index;
                        self.recency.tag(index);
                        break;
                    }
                    index += 1;
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        // Didn't find it
                        cursor = c;
                        break;
                    }
                }
            }
        }
        self.pane.replace(cursor.tree());
        self.zoomed = zoomed;
        self.size = size;

        self.resize(size);

        log::debug!(
            "sync tab: {:#?} zoomed: {} {:#?}",
            size,
            self.zoomed.is_some(),
            self.iter_panes()
        );
        assert!(self.pane.is_some());
    }

    fn codec_pane_tree(&mut self) -> PaneNode {
        let mux = Mux::get();
        let tab_id = self.id;
        let window_id = match mux.window_containing_tab(tab_id) {
            Some(w) => w,
            None => {
                log::error!("no window contains tab {}", tab_id);
                return PaneNode::Empty;
            }
        };

        let workspace = match mux
            .get_window(window_id)
            .map(|w| w.get_workspace().to_string())
        {
            Some(ws) => ws,
            None => {
                log::error!("window id {} doesn't have a window!?", window_id);
                return PaneNode::Empty;
            }
        };

        let active = self.get_active_pane();
        let zoomed = self.zoomed.as_ref();
        if let Some(root) = self.pane.as_ref() {
            pane_tree(
                root,
                tab_id,
                window_id,
                active.as_ref(),
                zoomed,
                &workspace,
                0,
                0,
            )
        } else {
            PaneNode::Empty
        }
    }

    /// Returns a count of how many panes are in this tab
    fn count_panes(&mut self) -> usize {
        let mut count = 0;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                count += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return count;
                }
            }
        }
    }

    /// Sets the zoom state, returns the prior state
    fn set_zoomed(&mut self, zoomed: bool) -> bool {
        if self.zoomed.is_some() == zoomed {
            // Current zoom state matches intended zoom state,
            // so we have nothing to do.
            return zoomed;
        }
        self.toggle_zoom();
        !zoomed
    }

    fn toggle_zoom(&mut self) {
        if let Some(pane) = self.zoomed.take() {
            // We were zoomed, but now we are not. The split tree kept the layout (and
            // followed tab resizes) while zoomed, so only the zoomed pane needs its place back.
            pane.set_zoomed(false);
            if let Some(pos) = self
                .iter_panes_ignoring_zoom()
                .into_iter()
                .find(|p| p.pane.pane_id() == pane.pane_id())
            {
                pane.resize(TerminalSize {
                    cols: pos.width,
                    rows: pos.height,
                    pixel_width: pos.pixel_width,
                    pixel_height: pos.pixel_height,
                    dpi: self.size.dpi,
                })
                .ok();
            }
        } else if let Some(pane) = self.get_active_pane() {
            // We weren't zoomed, but now we want to zoom the active pane.
            pane.set_zoomed(true);
            self.zoomed.replace(pane);
            self.apply_zoom_size();
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    /// Where the zoomed pane goes when the tab has Hivegrid chrome (see `is_chrome_pane`)
    /// at its edges: the rest of the tab, so the chrome keeps its place and size beside the
    /// zoomed pane. None for the whole tab (upstream zoom): not zoomed, no chrome at the
    /// edges, or the zoomed pane is chrome itself.
    fn zoom_area(&self) -> Option<CellRect> {
        let zoomed = self.zoomed.as_ref()?;
        if is_chrome_pane(zoomed) {
            return None;
        }
        non_chrome_area(self.pane.as_ref()?, &self.size)
    }

    /// The size of the zoomed pane: its zoom area, or the whole tab.
    fn zoomed_size(&self, area: Option<CellRect>) -> TerminalSize {
        match area {
            Some(area) => {
                let cell = self.cell_dimensions();
                TerminalSize {
                    cols: area.cols,
                    rows: area.rows,
                    pixel_width: area.cols * cell.pixel_width,
                    pixel_height: area.rows * cell.pixel_height,
                    dpi: cell.dpi,
                }
            }
            None => self.size,
        }
    }

    /// Gives the zoomed pane its size after a change of the tab size or of the tree, and
    /// keeps the active pane among the visible ones.
    fn apply_zoom_size(&mut self) {
        let Some(zoomed) = self.zoomed.clone() else {
            return;
        };
        let area = self.zoom_area();
        zoomed.resize(self.zoomed_size(area)).ok();
        if area.is_some() {
            // With chrome beside the zoomed pane the active pane may be a chrome pane; when
            // that one is gone or no longer visible, the zoomed pane is active again.
            let panes = self.iter_panes();
            if !panes.iter().any(|p| p.is_active) {
                if let Some(p) = panes.iter().find(|p| p.is_zoomed) {
                    self.active = p.index;
                }
            }
        }
    }

    /// Resizes the panes to their sizes in the split tree, the zoomed pane to its zoom size.
    fn apply_sizes(&mut self) {
        let zoomed = self.zoomed.as_ref().map(|p| p.pane_id());
        let size = self.size;
        if let Some(root) = self.pane.as_ref() {
            apply_sizes_from_splits(root, &size, zoomed);
        }
        self.apply_zoom_size();
    }

    fn contains_pane(&self, pane: PaneId) -> bool {
        fn contains(tree: &Tree, pane: PaneId) -> bool {
            match tree {
                Tree::Empty => false,
                Tree::Node { left, right, .. } => contains(left, pane) || contains(right, pane),
                Tree::Leaf(p) => p.pane_id() == pane,
            }
        }
        match &self.pane {
            Some(root) => contains(root, pane),
            None => false,
        }
    }

    /// Walks the pane tree to produce the topologically ordered flattened
    /// list of PositionedPane instances along with their positioning information.
    fn iter_panes(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(true)
    }

    /// Like iter_panes, except that it will include all panes, regardless of
    /// whether one of them is currently zoomed.
    fn iter_panes_ignoring_zoom(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(false)
    }

    fn rotate_counter_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .first()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.postorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    self.apply_sizes();
                    break;
                }
            }
        }
    }

    fn rotate_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .last()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    self.apply_sizes();
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn iter_panes_impl(&mut self, respect_zoom_state: bool) -> Vec<PositionedPane> {
        let mut panes = vec![];

        if respect_zoom_state && self.zoomed.is_some() {
            let zoomed_id = self.zoomed.as_ref().map(|p| p.pane_id());
            let area = self.zoom_area();
            let size = self.zoomed_size(area);
            let Some(area) = area else {
                panes.push(PositionedPane {
                    index: 0,
                    is_active: true,
                    is_zoomed: true,
                    left: 0,
                    top: 0,
                    width: size.cols.into(),
                    pixel_width: size.pixel_width.into(),
                    height: size.rows.into(),
                    pixel_height: size.pixel_height.into(),
                    pane: Arc::clone(self.zoomed.as_ref().unwrap()),
                });
                return panes;
            };
            // Chrome keeps its place; the zoomed pane fills the rest, the other panes are
            // hidden. Indices stay those of the whole tree.
            let mut panes = self.iter_panes_impl(false);
            panes.retain(|p| Some(p.pane.pane_id()) == zoomed_id || !area.contains(p.left, p.top));
            for p in panes.iter_mut().filter(|p| p.is_zoomed) {
                p.left = area.left;
                p.top = area.top;
                p.width = size.cols;
                p.height = size.rows;
                p.pixel_width = size.pixel_width;
                p.pixel_height = size.pixel_height;
            }
            return panes;
        }

        let active_idx = self.active;
        let zoomed_id = self.zoomed.as_ref().map(|p| p.pane_id());
        let root_size = self.size;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let index = panes.len();
                let mut left = 0usize;
                let mut top = 0usize;
                let mut parent_size = None;
                for (branch, node) in cursor.path_to_root() {
                    if let Some(node) = node {
                        if parent_size.is_none() {
                            parent_size.replace(if branch == PathBranch::IsRight {
                                node.second
                            } else {
                                node.first
                            });
                        }
                        if branch == PathBranch::IsRight {
                            top += node.top_of_second();
                            left += node.left_of_second();
                        }
                    }
                }

                let pane = Arc::clone(cursor.leaf_mut().unwrap());
                let dims = parent_size.unwrap_or_else(|| root_size);

                panes.push(PositionedPane {
                    index,
                    is_active: index == active_idx,
                    is_zoomed: zoomed_id == Some(pane.pane_id()),
                    left,
                    top,
                    width: dims.cols as _,
                    height: dims.rows as _,
                    pixel_width: dims.pixel_width as _,
                    pixel_height: dims.pixel_height as _,
                    pane,
                });
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        panes
    }

    fn iter_splits(&mut self) -> Vec<PositionedSplit> {
        let mut dividers = vec![];
        // While zoomed, only the dividers beside chrome (outside the zoom area) are visible.
        let hidden = match (self.zoomed.is_some(), self.zoom_area()) {
            (false, _) => None,
            (true, None) => return dividers,
            (true, Some(area)) => Some(area),
        };

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        loop {
            if !cursor.is_leaf() {
                let mut left = 0usize;
                let mut top = 0usize;
                for (branch, p) in cursor.path_to_root() {
                    if let Some(p) = p {
                        if branch == PathBranch::IsRight {
                            left += p.left_of_second();
                            top += p.top_of_second();
                        }
                    }
                }
                if let Ok(Some(node)) = cursor.node_mut() {
                    match node.direction {
                        SplitDirection::Horizontal => left += node.first.cols as usize,
                        SplitDirection::Vertical => top += node.first.rows as usize,
                    }

                    dividers.push(PositionedSplit {
                        index,
                        direction: node.direction,
                        left,
                        top,
                        size: if node.direction == SplitDirection::Horizontal {
                            node.height() as usize
                        } else {
                            node.width() as usize
                        },
                    })
                }
                index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        if let Some(area) = hidden {
            dividers.retain(|s| !area.contains(s.left, s.top));
        }
        dividers
    }

    fn get_size(&self) -> TerminalSize {
        self.size
    }

    fn resize(&mut self, size: TerminalSize) {
        if size.rows == 0 || size.cols == 0 {
            // Ignore "impossible" resize requests
            return;
        }

        let dims = cell_dimensions(&size);
        let (min_x, min_y) = compute_min_size(self.pane.as_mut().unwrap());
        let current_size = self.size;

        // Constrain the new size to the minimum possible dimensions
        let cols = size.cols.max(min_x);
        let rows = size.rows.max(min_y);
        let size = TerminalSize {
            rows,
            cols,
            pixel_width: cols * dims.pixel_width,
            pixel_height: rows * dims.pixel_height,
            dpi: dims.dpi,
        };

        // Update the split nodes with adjusted sizes; the tree follows the tab size while a
        // pane is zoomed too, so chrome beside the zoomed pane and the layout restored on
        // unzoom are right.
        adjust_x_size(
            self.pane.as_mut().unwrap(),
            cols as isize - current_size.cols as isize,
            &dims,
        );
        adjust_y_size(
            self.pane.as_mut().unwrap(),
            rows as isize - current_size.rows as isize,
            &dims,
        );

        self.size = size;

        // And then resize the individual panes to match
        self.apply_sizes();

        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn apply_pane_size(&mut self, pane_size: TerminalSize, cursor: &mut Cursor) {
        let cell_width = pane_size
            .pixel_width
            .checked_div(pane_size.cols)
            .unwrap_or(1);
        let cell_height = pane_size
            .pixel_height
            .checked_div(pane_size.rows)
            .unwrap_or(1);
        if let Ok(Some(node)) = cursor.node_mut() {
            // Adjust the size of the node; we preserve the size of the first
            // child and adjust the second, so if we are split down the middle
            // and the window is made wider, the right column will grow in
            // size, leaving the left at its current width.
            if node.direction == SplitDirection::Horizontal {
                node.first.rows = pane_size.rows;
                node.second.rows = pane_size.rows;

                node.second.cols = pane_size.cols.saturating_sub(1 + node.first.cols);
            } else {
                node.first.cols = pane_size.cols;
                node.second.cols = pane_size.cols;

                node.second.rows = pane_size.rows.saturating_sub(1 + node.first.rows);
            }
            node.first.pixel_width = node.first.cols * cell_width;
            node.first.pixel_height = node.first.rows * cell_height;

            node.second.pixel_width = node.second.cols * cell_width;
            node.second.pixel_height = node.second.rows * cell_height;
        }
    }

    fn rebuild_splits_sizes_from_contained_panes(&mut self) {
        if self.zoomed.is_some() {
            return;
        }

        fn compute_size(node: &mut Tree) -> Option<TerminalSize> {
            match node {
                Tree::Empty => None,
                Tree::Leaf(pane) => {
                    let dims = pane.get_dimensions();
                    let size = TerminalSize {
                        cols: dims.cols,
                        rows: dims.viewport_rows,
                        pixel_height: dims.pixel_height,
                        pixel_width: dims.pixel_width,
                        dpi: dims.dpi,
                    };
                    Some(size)
                }
                Tree::Node { left, right, data } => {
                    if let Some(data) = data {
                        if let Some(first) = compute_size(left) {
                            data.first = first;
                        }
                        if let Some(second) = compute_size(right) {
                            data.second = second;
                        }
                        Some(data.size())
                    } else {
                        None
                    }
                }
            }
        }

        if let Some(root) = self.pane.as_mut() {
            if let Some(size) = compute_size(root) {
                self.size = size;
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn resize_split_by(&mut self, split_index: usize, delta: isize) {
        // While zoomed, only a divider beside chrome can be moved.
        if self.zoomed.is_some() && !self.iter_splits().iter().any(|s| s.index == split_index) {
            return;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the specified split
        loop {
            if !cursor.is_leaf() {
                if index == split_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }

        // Now cursor is looking at the split
        self.adjust_node_at_cursor(&mut cursor, delta);
        self.cascade_size_from_cursor(cursor);
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_node_at_cursor(&mut self, cursor: &mut Cursor, delta: isize) {
        let cell_dimensions = self.cell_dimensions();
        // Neither side goes below the minimum size of its subtree (upstream only kept one
        // cell, so a nested side could be asked to be smaller than it can be).
        let ((first_min_x, first_min_y), (second_min_x, second_min_y)) = match cursor.subtree() {
            Tree::Node { left, right, .. } => (compute_min_size(left), compute_min_size(right)),
            _ => ((1, 1), (1, 1)),
        };
        if let Ok(Some(node)) = cursor.node_mut() {
            match node.direction {
                SplitDirection::Horizontal => {
                    let width = node.width();

                    let mut cols = node.first.cols as isize;
                    cols = cols
                        .saturating_add(delta)
                        .min((width as isize).saturating_sub(1 + second_min_x as isize))
                        .max(first_min_x as isize);
                    node.first.cols = cols as usize;
                    node.first.pixel_width =
                        node.first.cols.saturating_mul(cell_dimensions.pixel_width);

                    node.second.cols = width.saturating_sub(node.first.cols.saturating_add(1));
                    node.second.pixel_width =
                        node.second.cols.saturating_mul(cell_dimensions.pixel_width);
                }
                SplitDirection::Vertical => {
                    let height = node.height();

                    let mut rows = node.first.rows as isize;
                    rows = rows
                        .saturating_add(delta)
                        .min((height as isize).saturating_sub(1 + second_min_y as isize))
                        .max(first_min_y as isize);
                    node.first.rows = rows as usize;
                    node.first.pixel_height =
                        node.first.rows.saturating_mul(cell_dimensions.pixel_height);

                    node.second.rows = height.saturating_sub(node.first.rows.saturating_add(1));
                    node.second.pixel_height = node
                        .second
                        .rows
                        .saturating_mul(cell_dimensions.pixel_height);
                }
            }
        }
    }

    fn cascade_size_from_cursor(&mut self, cursor: Cursor) {
        // Now the subtrees below the moved divider have to fit their new sizes
        let mut root = cursor.tree();
        fit_subtrees(&mut root, &self.cell_dimensions());
        self.pane.replace(root);
        self.apply_sizes();
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_pane_size(&mut self, direction: PaneDirection, amount: usize) {
        self.adjust_pane_size_at(self.active, direction, amount)
    }

    fn adjust_pane_size_of(&mut self, pane_id: PaneId, direction: PaneDirection, amount: usize) {
        let index = self
            .iter_panes_ignoring_zoom()
            .iter()
            .position(|p| p.pane.pane_id() == pane_id);
        if let Some(index) = index {
            self.adjust_pane_size_at(index, direction, amount)
        }
    }

    fn adjust_pane_size_at(&mut self, active_index: usize, direction: PaneDirection, amount: usize) {
        // While zoomed, only a chrome pane beside the zoomed pane can be resized.
        if self.zoomed.is_some()
            && !self
                .iter_panes()
                .iter()
                .any(|p| p.index == active_index && !p.is_zoomed)
        {
            return;
        }
        // Now we go up from the leaf until we find the parent node that is
        // aligned with the desired direction.
        let split_direction = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        let delta = match direction {
            PaneDirection::Down | PaneDirection::Right => amount as isize,
            PaneDirection::Up | PaneDirection::Left => -(amount as isize),
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        // A fixed-width pane (the Hivegrid bar, a sidebar view) resized Left/Right moves the
        // divider on its right edge: the nearest horizontal split it is the first side of. In
        // bar | view | grid the nearest split of the view is the bar's divider, so its own
        // width could not be set. Without such a split, the nearest one as for any pane.
        let mut right_edge_only = split_direction == SplitDirection::Horizontal
            && self
                .iter_panes_ignoring_zoom()
                .get(active_index)
                .map_or(false, |p| {
                    p.pane
                        .copy_user_vars()
                        .contains_key("hivegrid_fixed_width")
                });
        loop {
            let mut cursor = match self.pane.take().unwrap().cursor().go_to_nth_leaf(active_index)
            {
                Ok(c) => c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return;
                }
            };
            loop {
                let from_first = cursor.is_left();
                match cursor.go_up() {
                    Ok(mut c) => {
                        if let Ok(Some(node)) = c.node_mut() {
                            if node.direction == split_direction && (from_first || !right_edge_only)
                            {
                                self.adjust_node_at_cursor(&mut c, delta);
                                self.cascade_size_from_cursor(c);
                                return;
                            }
                        }
                        cursor = c;
                    }
                    Err(c) => {
                        self.pane.replace(c.tree());
                        break;
                    }
                }
            }
            if !right_edge_only {
                return;
            }
            right_edge_only = false;
        }
    }

    fn activate_pane_direction(&mut self, direction: PaneDirection) {
        if let Some(target) = self.chrome_zoom_direction(direction) {
            if let Some(panel_idx) = target {
                self.set_active_idx(panel_idx);
            }
        } else {
            if self.zoomed.is_some() {
                if !configuration().unzoom_on_switch_pane {
                    return;
                }
                self.toggle_zoom();
            }
            if let Some(panel_idx) = self.get_pane_direction(direction, false) {
                self.set_active_idx(panel_idx);
            }
        }
        let mux = Mux::get();
        if let Some(window_id) = mux.window_containing_tab(self.id) {
            mux.notify(MuxNotification::WindowInvalidated(window_id));
        }
    }

    /// A focus move that keeps the zoom while chrome is visible beside the zoomed pane:
    /// from the zoomed pane to a chrome pane next to it in the unzoomed layout, or from a
    /// chrome pane to what is visible in that direction (Some(None): nothing there). None
    /// when the move is an ordinary one (not zoomed beside chrome, or the zoomed pane's
    /// neighbour is a hidden pane).
    fn chrome_zoom_direction(&mut self, direction: PaneDirection) -> Option<Option<usize>> {
        self.zoom_area()?;
        let visible = self.iter_panes();
        let from_zoomed = visible.iter().any(|p| p.is_active && p.is_zoomed);
        let target = self.get_pane_direction(direction, from_zoomed);
        match target {
            Some(idx) if visible.iter().any(|p| p.index == idx) => Some(Some(idx)),
            None if !from_zoomed => Some(None),
            _ => None,
        }
    }

    fn get_pane_direction(&mut self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        let panes = if ignore_zoom {
            self.iter_panes_ignoring_zoom()
        } else {
            self.iter_panes()
        };

        let active = match panes.iter().find(|pane| pane.is_active) {
            Some(p) => p,
            None => {
                // No active pane somehow...
                return Some(0);
            }
        };

        if matches!(direction, PaneDirection::Next | PaneDirection::Prev) {
            let max_pane_id = panes.iter().map(|p| p.index).max().unwrap_or(active.index);

            return Some(if direction == PaneDirection::Next {
                if active.index == max_pane_id {
                    0
                } else {
                    active.index + 1
                }
            } else {
                if active.index == 0 {
                    max_pane_id
                } else {
                    active.index - 1
                }
            });
        }

        let mut best = None;

        let recency = &self.recency;

        fn edge_intersects(
            active_start: usize,
            active_size: usize,
            current_start: usize,
            current_size: usize,
        ) -> bool {
            intersects_range(
                &(active_start..active_start + active_size),
                &(current_start..current_start + current_size),
            )
        }

        for pane in &panes {
            let score = match direction {
                PaneDirection::Right => {
                    if pane.left == active.left + active.width + 1
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Left => {
                    if pane.left + pane.width + 1 == active.left
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Up => {
                    if pane.top + pane.height + 1 == active.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Down => {
                    if active.top + active.height + 1 == pane.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Next | PaneDirection::Prev => unreachable!(),
            };

            if score > 0 {
                let target = match best.take() {
                    Some((best_score, best_pane)) if best_score > score => (best_score, best_pane),
                    _ => (score, pane),
                };
                best.replace(target);
            }
        }

        if let Some((_, target)) = best.take() {
            return Some(target.index);
        }
        None
    }

    fn prune_dead_panes(&mut self) -> bool {
        let mux = Mux::get();
        !self
            .remove_pane_if(
                |_, pane| {
                    // If the pane is no longer known to the mux, then its liveness
                    // state isn't guaranteed to be monitored or updated, so let's
                    // consider the pane effectively dead if it isn't in the mux.
                    // <https://github.com/wezterm/wezterm/issues/4030>
                    let in_mux = mux.get_pane(pane.pane_id()).is_some();
                    let dead = pane.is_dead();
                    log::trace!(
                        "prune_dead_panes: pane_id={} dead={} in_mux={}",
                        pane.pane_id(),
                        dead,
                        in_mux
                    );
                    dead || !in_mux
                },
                true,
            )
            .is_empty()
    }

    fn kill_pane(&mut self, pane_id: PaneId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.pane_id() == pane_id, true)
            .is_empty()
    }

    fn kill_panes_in_domain(&mut self, domain: DomainId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.domain_id() == domain, true)
            .is_empty()
    }

    fn remove_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let panes = self.remove_pane_if(|_, pane| pane.pane_id() == pane_id, false);
        for pane in panes {
            return Some(pane);
        }
        None
    }

    fn remove_pane_if<F>(&mut self, f: F, kill: bool) -> Vec<Arc<dyn Pane>>
    where
        F: Fn(usize, &Arc<dyn Pane>) -> bool,
    {
        let mut dead_panes = vec![];
        let zoomed_pane = self.zoomed.as_ref().map(|p| p.pane_id());

        {
            let root_size = self.size;
            let mut cursor = self.pane.take().unwrap().cursor();
            let mut pane_index = 0;
            let mut removed_indices = vec![];
            let cell_dims = self.cell_dimensions();

            loop {
                // Figure out the available size by looking at our immediate parent node.
                // If we are the root, look at the tab size
                let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                    if branch == PathBranch::IsRight {
                        parent.second
                    } else {
                        parent.first
                    }
                } else {
                    root_size
                };

                if cursor.is_leaf() {
                    let pane = Arc::clone(cursor.leaf_mut().unwrap());
                    if f(pane_index, &pane) {
                        removed_indices.push(pane_index);
                        if Some(pane.pane_id()) == zoomed_pane {
                            // If we removed the zoomed pane, un-zoom our state!
                            self.zoomed.take();
                        }
                        let parent;
                        match cursor.unsplit_leaf() {
                            Ok((c, dead, p)) => {
                                dead_panes.push(dead);
                                parent = p.unwrap();
                                cursor = c;
                            }
                            Err(c) => {
                                // We might be the root, for example
                                if c.is_top() && c.is_leaf() {
                                    self.pane.replace(Tree::Empty);
                                    dead_panes.push(pane);
                                } else {
                                    self.pane.replace(c.tree());
                                }
                                break;
                            }
                        };

                        // Now we need to increase the size of the current node
                        // and propagate the revised size to its children.
                        let size = TerminalSize {
                            rows: parent.height(),
                            cols: parent.width(),
                            pixel_width: cell_dims.pixel_width * parent.width(),
                            pixel_height: cell_dims.pixel_height * parent.height(),
                            dpi: cell_dims.dpi,
                        };

                        if let Some(unsplit) = cursor.leaf_mut() {
                            if Some(unsplit.pane_id()) != zoomed_pane {
                                unsplit.resize(size).ok();
                            }
                        } else {
                            self.apply_pane_size(size, &mut cursor);
                        }
                    } else if !dead_panes.is_empty() && Some(pane.pane_id()) != zoomed_pane {
                        // Apply our revised size to the tty (the zoomed pane is sized below)
                        pane.resize(pane_size).ok();
                    }

                    pane_index += 1;
                } else if !dead_panes.is_empty() {
                    self.apply_pane_size(pane_size, &mut cursor);
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        self.pane.replace(c.tree());
                        break;
                    }
                }
            }

            // Figure out which pane should now be active.
            // If panes earlier than the active pane were closed, then we
            // need to shift the active pane down
            let active_idx = self.active;
            removed_indices.retain(|&idx| idx <= active_idx);
            self.active = active_idx.saturating_sub(removed_indices.len());
        }

        if !dead_panes.is_empty() {
            // The zoom area may have changed (a chrome pane or a hidden one closed)
            self.apply_zoom_size();
        }

        if !dead_panes.is_empty() && kill {
            let to_kill: Vec<_> = dead_panes.iter().map(|p| p.pane_id()).collect();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                for pane_id in to_kill.into_iter() {
                    mux.remove_pane(pane_id);
                }
            })
            .detach();
        }
        dead_panes
    }

    fn can_close_without_prompting(&mut self, reason: CloseReason) -> bool {
        let panes = self.iter_panes_ignoring_zoom();
        for pos in &panes {
            if !pos.pane.can_close_without_prompting(reason) {
                return false;
            }
        }
        true
    }

    fn is_dead(&mut self) -> bool {
        // Make sure we account for all panes, so that we don't
        // kill the whole tab if the zoomed pane is dead!
        let panes = self.iter_panes_ignoring_zoom();
        let mut dead_count = 0;
        for pos in &panes {
            if pos.pane.is_dead() {
                dead_count += 1;
            }
        }
        dead_count == panes.len()
    }

    fn get_active_pane(&mut self) -> Option<Arc<dyn Pane>> {
        if let Some(zoomed) = self.zoomed.clone() {
            // Beside chrome, a chrome pane can have the focus while another pane is zoomed.
            if self.zoom_area().is_some() {
                if let Some(pos) = self.iter_panes().into_iter().find(|p| p.is_active) {
                    return Some(pos.pane);
                }
            }
            return Some(zoomed);
        }

        self.iter_panes_ignoring_zoom()
            .iter()
            .nth(self.active)
            .map(|p| Arc::clone(&p.pane))
    }

    fn get_active_idx(&self) -> usize {
        self.active
    }

    fn set_active_pane(&mut self, pane: &Arc<dyn Pane>) {
        let prior = self.get_active_pane();

        if is_pane(pane, &prior.as_ref()) {
            return;
        }

        if self.zoomed.is_some() {
            // Focusing a pane that is visible beside the zoomed one (chrome) keeps the zoom.
            let visible = self.zoom_area().is_some()
                && self
                    .iter_panes()
                    .iter()
                    .any(|p| p.pane.pane_id() == pane.pane_id());
            if !visible {
                if !configuration().unzoom_on_switch_pane {
                    return;
                }
                self.toggle_zoom();
            }
        }

        if let Some(item) = self
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane.pane_id())
        {
            self.active = item.index;
            self.recency.tag(item.index);
            self.advise_focus_change(prior);
        }
    }

    fn advise_focus_change(&mut self, prior: Option<Arc<dyn Pane>>) {
        let mux = Mux::get();
        let current = self.get_active_pane();
        match (prior, current) {
            (Some(prior), Some(current)) if prior.pane_id() != current.pane_id() => {
                prior.focus_changed(false);
                current.focus_changed(true);
                mux.notify(MuxNotification::PaneFocused(current.pane_id()));
            }
            (None, Some(current)) => {
                current.focus_changed(true);
                mux.notify(MuxNotification::PaneFocused(current.pane_id()));
            }
            (Some(prior), None) => {
                prior.focus_changed(false);
            }
            (Some(_), Some(_)) | (None, None) => {
                // no change
            }
        }
    }

    fn set_active_idx(&mut self, pane_index: usize) {
        let prior = self.get_active_pane();
        self.active = pane_index;
        self.recency.tag(pane_index);
        self.advise_focus_change(prior);
    }

    fn assign_pane(&mut self, pane: &Arc<dyn Pane>) {
        match Tree::new().cursor().assign_top(Arc::clone(pane)) {
            Ok(c) => self.pane = Some(c.tree()),
            Err(_) => panic!("tried to assign root pane to non-empty tree"),
        }
    }

    fn cell_dimensions(&self) -> TerminalSize {
        cell_dimensions(&self.size)
    }

    fn swap_active_with_index(&mut self, pane_index: usize, keep_focus: bool) -> Option<()> {
        let active_idx = self.get_active_idx();
        let mut pane = self.get_active_pane()?;
        log::trace!(
            "swap_active_with_index: pane_index {} active {}",
            pane_index,
            active_idx
        );

        {
            let mut cursor = self.pane.take().unwrap().cursor();

            // locate the requested index
            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    log::trace!("didn't find pane {pane_index}");
                    self.pane.replace(c.tree());
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());

            // re-position to the root
            cursor = cursor.tree().cursor();

            // and now go and update the active idx
            match cursor.go_to_nth_leaf(active_idx) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    log::trace!("didn't find active {active_idx}");
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());
            self.pane.replace(cursor.tree());

            // Advise the panes of their new sizes
            self.apply_sizes();
        }

        // And update focus
        if keep_focus {
            self.set_active_idx(pane_index);
        } else {
            self.advise_focus_change(Some(pane));
        }
        None
    }

    fn compute_split_size(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        let cell_dims = self.cell_dimensions();

        fn split_dimension(dim: usize, request: SplitRequest) -> (usize, usize) {
            let target_size = match request.size {
                SplitSize::Cells(n) => n,
                SplitSize::Percent(n) => (dim * (n as usize)) / 100,
            }
            .max(1);

            let remain = dim.saturating_sub(target_size + 1);

            if request.target_is_second {
                (remain, target_size)
            } else {
                (target_size, remain)
            }
        }

        // Ensure that we're not zoomed, otherwise we'll end up in
        // a bogus split state (https://github.com/wezterm/wezterm/issues/723);
        // a top-level split (the Hivegrid dock or bar opening) unzooms too.
        self.set_zoomed(false);

        if request.top_level {
            let size = self.size;

            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimension(size.cols as usize, request),
                    (size.rows as usize, size.rows as usize),
                ),
                SplitDirection::Vertical => (
                    (size.cols as usize, size.cols as usize),
                    split_dimension(size.rows as usize, request),
                ),
            };

            return Some(SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            });
        }

        self.iter_panes().iter().nth(pane_index).map(|pos| {
            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimension(pos.width, request),
                    (pos.height, pos.height),
                ),
                SplitDirection::Vertical => {
                    ((pos.width, pos.width), split_dimension(pos.height, request))
                }
            };

            SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            }
        })
    }

    fn split_and_insert(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        if self.zoomed.is_some() {
            anyhow::bail!("cannot split while zoomed");
        }

        {
            let split_info = self
                .compute_split_size(pane_index, request)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid pane_index {}; cannot split!", pane_index)
                })?;

            let tab_size = self.size;
            if split_info.first.rows == 0
                || split_info.first.cols == 0
                || split_info.second.rows == 0
                || split_info.second.cols == 0
                || split_info.top_of_second() + split_info.second.rows > tab_size.rows
                || split_info.left_of_second() + split_info.second.cols > tab_size.cols
            {
                log::error!(
                    "No space for split!!! {:#?} height={} width={} top_of_second={} left_of_second={} tab_size={:?}",
                    split_info,
                    split_info.height(),
                    split_info.width(),
                    split_info.top_of_second(),
                    split_info.left_of_second(),
                    tab_size
                );
                anyhow::bail!("No space for split!");
            }

            let needs_resize = if request.top_level {
                self.pane.as_ref().unwrap().num_leaves() > 1
            } else {
                false
            };

            // `resize` below also overwrites `self.size`; the tab is still as big as before
            // once the new pane is in, so the size is put back after the split.
            let full_size = self.size;
            if needs_resize {
                // Pre-emptively resize the tab contents down to
                // match the target size; it's easier to reuse
                // existing resize logic that way
                if request.target_is_second {
                    self.resize(split_info.first.clone());
                } else {
                    self.resize(split_info.second.clone());
                }
            }

            let mut cursor = self.pane.take().unwrap().cursor();

            if request.top_level && !cursor.is_leaf() {
                let result = if request.target_is_second {
                    cursor.split_node_and_insert_right(Arc::clone(&pane))
                } else {
                    cursor.split_node_and_insert_left(Arc::clone(&pane))
                };
                cursor = match result {
                    Ok(c) => {
                        cursor = match c.assign_node(Some(split_info)) {
                            Err(c) | Ok(c) => c,
                        };
                        // The new pane gets its slot's size (it was spawned or moved with
                        // another one, and nothing else resizes it).
                        pane.resize(if request.target_is_second {
                            split_info.second
                        } else {
                            split_info.first
                        })
                        .ok();

                        self.pane.replace(cursor.tree());
                        self.size = full_size;

                        let pane_index = if request.target_is_second {
                            self.pane.as_ref().unwrap().num_leaves().saturating_sub(1)
                        } else {
                            0
                        };

                        self.active = pane_index;
                        self.recency.tag(pane_index);
                        return Ok(pane_index);
                    }
                    Err(cursor) => cursor,
                };
            }

            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            let existing_pane = Arc::clone(cursor.leaf_mut().unwrap());

            let (pane1, pane2) = if request.target_is_second {
                (existing_pane, pane)
            } else {
                (pane, existing_pane)
            };

            pane1.resize(split_info.first)?;
            pane2.resize(split_info.second.clone())?;

            *cursor.leaf_mut().unwrap() = pane1;

            match cursor.split_leaf_and_insert_right(pane2) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            // cursor now points to the newly created split node;
            // we need to populate its split information
            match cursor.assign_node(Some(split_info)) {
                Err(c) | Ok(c) => self.pane.replace(c.tree()),
            };

            if request.target_is_second {
                self.active = pane_index + 1;
                self.recency.tag(pane_index + 1);
            }
        }

        log::debug!("split info after split: {:#?}", self.iter_splits());
        log::debug!("pane info after split: {:#?}", self.iter_panes());

        Ok(if request.target_is_second {
            pane_index + 1
        } else {
            pane_index
        })
    }

    fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.zoomed.clone()
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub enum PaneNode {
    Empty,
    Split {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
}

impl PaneNode {
    pub fn into_tree(self) -> bintree::Tree<PaneEntry, SplitDirectionAndSize> {
        match self {
            PaneNode::Empty => bintree::Tree::Empty,
            PaneNode::Split { left, right, node } => bintree::Tree::Node {
                left: Box::new((*left).into_tree()),
                right: Box::new((*right).into_tree()),
                data: Some(node),
            },
            PaneNode::Leaf(e) => bintree::Tree::Leaf(e),
        }
    }

    pub fn root_size(&self) -> Option<TerminalSize> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { node, .. } => Some(node.size()),
            PaneNode::Leaf(entry) => Some(entry.size),
        }
    }

    pub fn window_and_tab_ids(&self) -> Option<(WindowId, TabId)> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { left, right, .. } => match left.window_and_tab_ids() {
                Some(res) => Some(res),
                None => right.window_and_tab_ids(),
            },
            PaneNode::Leaf(entry) => Some((entry.window_id, entry.tab_id)),
        }
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneEntry {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    pub workspace: String,
    pub cursor_pos: StableCursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub tty_name: Option<String>,
}

#[derive(Deserialize, Clone, Serialize, PartialEq, Debug)]
#[serde(try_from = "String", into = "String")]
pub struct SerdeUrl {
    pub url: Url,
}

impl std::convert::TryFrom<String> for SerdeUrl {
    type Error = url::ParseError;
    fn try_from(s: String) -> Result<SerdeUrl, url::ParseError> {
        let url = Url::parse(&s)?;
        Ok(SerdeUrl { url })
    }
}

impl From<Url> for SerdeUrl {
    fn from(url: Url) -> SerdeUrl {
        SerdeUrl { url }
    }
}

impl Into<Url> for SerdeUrl {
    fn into(self) -> Url {
        self.url
    }
}

impl Into<String> for SerdeUrl {
    fn into(self) -> String {
        self.url.as_str().into()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderable::*;
    use parking_lot::{MappedMutexGuard, Mutex};
    use rangeset::RangeSet;
    use std::ops::Range;
    use termwiz::surface::SequenceNo;
    use url::Url;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        vars: HashMap<String, String>,
    }

    impl FakePane {
        fn new(id: PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                vars: HashMap::new(),
            })
        }

        /// A pane whose size the test can read; `chrome`: a Hivegrid chrome pane (bar, dock)
        /// with the user var `hivegrid_fixed_width`.
        fn typed(id: PaneId, size: TerminalSize, chrome: bool) -> Arc<Self> {
            let mut vars = HashMap::new();
            if chrome {
                vars.insert("hivegrid_fixed_width".to_string(), "1".to_string());
            }
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                vars,
            })
        }

        fn cols(&self) -> usize {
            self.size.lock().cols
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            unimplemented!();
        }

        fn get_current_seqno(&self) -> SequenceNo {
            unimplemented!();
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            unimplemented!();
        }

        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
            unimplemented!();
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            unimplemented!();
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            unimplemented!();
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            unimplemented!();
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                dpi: size.dpi,
                ..Default::default()
            }
        }

        fn get_title(&self) -> String {
            unimplemented!()
        }
        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            unimplemented!()
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn is_dead(&self) -> bool {
            false
        }
        fn palette(&self) -> ColorPalette {
            unimplemented!()
        }
        fn domain_id(&self) -> DomainId {
            1
        }
        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
        }
        fn copy_user_vars(&self) -> HashMap<String, String> {
            self.vars.clone()
        }
    }

    #[test]
    fn tab_splitting() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let panes = tab.iter_panes();
        assert_eq!(1, panes.len());
        assert_eq!(0, panes[0].index);
        assert_eq!(true, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(80, panes[0].width);
        assert_eq!(24, panes[0].height);

        assert!(tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                }
            )
            .is_none());

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            horz_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                second: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 600,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 24,
                    cols: 39,
                    pixel_width: 390,
                    pixel_height: 600,
                    dpi: 96,
                },
            }
        );

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            vert_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                second: TerminalSize {
                    rows: 12,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 300,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 11,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 275,
                    dpi: 96,
                }
            }
        );

        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(2, horz_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(2, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(24, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(600, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(40, panes[1].left);
        assert_eq!(0, panes[1].top);
        assert_eq!(40, panes[1].width);
        assert_eq!(24, panes[1].height);
        assert_eq!(400, panes[1].pixel_width);
        assert_eq!(600, panes[1].pixel_height);
        assert_eq!(2, panes[1].pane.pane_id());

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    top_level: false,
                    target_is_second: true,
                    size: Default::default(),
                },
                FakePane::new(3, vert_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(11, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(275, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(0, panes[1].left);
        assert_eq!(12, panes[1].top);
        assert_eq!(39, panes[1].width);
        assert_eq!(12, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(300, panes[1].pixel_height);
        assert_eq!(3, panes[1].pane.pane_id());

        assert_eq!(2, panes[2].index);
        assert_eq!(false, panes[2].is_active);
        assert_eq!(40, panes[2].left);
        assert_eq!(0, panes[2].top);
        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
        assert_eq!(2, panes[2].pane.pane_id());

        tab.resize_split_by(1, 1);
        let panes = tab.iter_panes();
        assert_eq!(39, panes[0].width);
        assert_eq!(12, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(300, panes[0].pixel_height);

        assert_eq!(39, panes[1].width);
        assert_eq!(11, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(275, panes[1].pixel_height);

        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
    }

    #[test]
    fn share_delta_both_fixed_goes_to_wider_side() {
        // bar (5) | sidebar view (40), both fixed: the view takes the change
        assert_eq!(share_delta(10, (5, 1, true, 1), (40, 1, true, 1)), (0, 10));
        assert_eq!(share_delta(-10, (40, 1, true, 1), (5, 1, true, 1)), (-10, 0));
        // never below a minimum
        assert_eq!(share_delta(-10, (5, 1, true, 1), (8, 1, true, 1)), (-3, -7));
    }

    #[test]
    fn single_cell_resizes_keep_grid_columns_even() {
        // one column | two columns: the single cell a shrink leaves over goes to the side with
        // the wider panes (dividers excluded), a growth to the narrower
        assert_eq!(share_delta(-1, (62, 1, false, 1), (127, 3, false, 2)), (0, -1));
        assert_eq!(share_delta(-1, (62, 1, false, 1), (123, 3, false, 2)), (-1, 0));
        assert_eq!(share_delta(1, (61, 1, false, 1), (125, 3, false, 2)), (1, 0));
        assert_eq!(share_delta(1, (62, 1, false, 1), (123, 3, false, 2)), (0, 1));

        let size = TerminalSize {
            rows: 30,
            cols: 188,
            pixel_width: 1880,
            pixel_height: 600,
            dpi: 96,
        };
        let panes = [
            FakePane::typed(1, size, false),
            FakePane::typed(2, size, false),
            FakePane::typed(3, size, false),
        ];
        let tab = Tab::new(&size);
        tab.assign_pane(&(panes[0].clone() as Arc<dyn Pane>));
        for (index, pane) in [(0, &panes[2]), (0, &panes[1])] {
            let request = SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                size: SplitSize::Cells(62),
                ..Default::default()
            };
            tab.split_and_insert(index, request, pane.clone() as Arc<dyn Pane>)
                .unwrap();
        }
        let widths = || panes.iter().map(|p| p.cols()).collect::<Vec<_>>();
        assert_eq!(widths(), vec![62, 62, 62]);
        // a window dragged narrower and wider again, one cell per step
        for cols in (158..188).rev().chain(159..=200) {
            tab.resize(TerminalSize {
                cols,
                pixel_width: cols * 10,
                ..size
            });
            let w = widths();
            let spread = w.iter().max().unwrap() - w.iter().min().unwrap();
            assert!(spread <= 1, "{} cols: {:?}", cols, w);
            assert_eq!(w.iter().sum::<usize>() + 2, cols);
        }
    }

    /// bar (5) | p1 | p2 | dock (20) in a 100x30 tab; p1 active. The dock is split off before
    /// the bar with `dock_first`, so the tree is bar | (grid | dock) instead of (bar | grid) | dock.
    fn chrome_tab(dock_first: bool) -> (Tab, [Arc<FakePane>; 4]) {
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };
        let p1 = FakePane::typed(1, size, false);
        let p2 = FakePane::typed(2, size, false);
        let bar = FakePane::typed(3, size, true);
        let dock = FakePane::typed(4, size, true);
        let tab = Tab::new(&size);
        tab.assign_pane(&(p1.clone() as Arc<dyn Pane>));
        let split = |index, pane: &Arc<FakePane>, request| {
            tab.split_and_insert(index, request, pane.clone() as Arc<dyn Pane>)
                .unwrap();
        };
        split(
            0,
            &p2,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
        );
        let bar_split = SplitRequest {
            direction: SplitDirection::Horizontal,
            top_level: true,
            target_is_second: false,
            size: SplitSize::Cells(5),
        };
        let dock_split = SplitRequest {
            direction: SplitDirection::Horizontal,
            top_level: true,
            target_is_second: true,
            size: SplitSize::Cells(20),
        };
        if dock_first {
            split(0, &dock, dock_split);
            split(0, &bar, bar_split);
        } else {
            split(0, &bar, bar_split);
            split(0, &dock, dock_split);
        }
        tab.inner.lock().active = 1;
        (tab, [bar, p1, p2, dock])
    }

    /// (left, width) of every pane, in tree order.
    fn columns(tab: &Tab) -> Vec<(usize, usize)> {
        tab.iter_panes_ignoring_zoom()
            .iter()
            .map(|p| (p.left, p.width))
            .collect()
    }

    #[test]
    fn widening_the_bar_shrinks_the_grid_not_the_dock() {
        for dock_first in [false, true] {
            let (tab, [bar, p1, p2, dock]) = chrome_tab(dock_first);
            // The Hivegrid sidebar widens the bar before it splits a view off it.
            tab.adjust_pane_size_of(3, PaneDirection::Right, 30);
            assert_eq!((bar.cols(), dock.cols()), (35, 20), "dock_first {dock_first}");
            assert_eq!(p1.cols() + 1 + p2.cols(), 43, "dock_first {dock_first}");
            let cols = columns(&tab);
            assert_eq!(cols[3], (80, 20), "dock_first {dock_first}");
            // Far too much: the other side keeps its minimum (1-column panes) and nothing
            // overflows the tab.
            tab.adjust_pane_size_of(3, PaneDirection::Right, 500);
            assert_eq!((p1.cols(), p2.cols()), (1, 1), "dock_first {dock_first}");
            let cols = columns(&tab);
            assert_eq!(cols[3].0 + cols[3].1, 100, "dock_first {dock_first}");
        }
    }

    #[test]
    fn fixed_width_pane_resizes_at_its_right_edge() {
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };
        let grid = FakePane::typed(1, size, false);
        let bar = FakePane::typed(2, size, true);
        let view = FakePane::typed(3, size, true);
        let tab = Tab::new(&size);
        tab.assign_pane(&(grid.clone() as Arc<dyn Pane>));
        // (bar | view) | grid, as the Hivegrid sidebar builds it
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                top_level: true,
                target_is_second: false,
                size: SplitSize::Cells(26),
            },
            bar.clone() as Arc<dyn Pane>,
        )
        .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                top_level: false,
                target_is_second: true,
                size: SplitSize::Cells(20),
            },
            view.clone() as Arc<dyn Pane>,
        )
        .unwrap();
        assert_eq!((bar.cols(), view.cols(), grid.cols()), (5, 20, 73));
        // The view moves its right divider; the bar keeps its width.
        tab.adjust_pane_size_of(3, PaneDirection::Right, 10);
        assert_eq!((bar.cols(), view.cols(), grid.cols()), (5, 30, 63));
        tab.adjust_pane_size_of(3, PaneDirection::Left, 4);
        assert_eq!((bar.cols(), view.cols(), grid.cols()), (5, 26, 67));
        // The bar is the first side of the bar | view split: that divider moves.
        tab.adjust_pane_size_of(2, PaneDirection::Right, 2);
        assert_eq!((bar.cols(), view.cols(), grid.cols()), (7, 24, 67));
        // A grid pane: the nearest split, as upstream.
        tab.adjust_pane_size_of(1, PaneDirection::Left, 3);
        assert_eq!((bar.cols(), view.cols(), grid.cols()), (7, 21, 70));
    }

    #[test]
    fn top_level_split_sizes_the_new_pane() {
        let (tab, [bar, p1, p2, dock]) = chrome_tab(false);
        // The bar and dock were created full width; the split gives them their slot.
        assert_eq!((bar.cols(), dock.cols()), (5, 20));
        assert_eq!(p1.cols() + 1 + p2.cols(), 73);
        let ids: Vec<_> = tab.iter_panes().iter().map(|p| p.pane.pane_id()).collect();
        assert_eq!(ids, vec![3, 1, 2, 4]);
    }

    #[test]
    fn zoom_keeps_chrome() {
        let (tab, [bar, p1, p2, dock]) = chrome_tab(false);
        let (p1_cols, p2_cols) = (p1.cols(), p2.cols());
        tab.toggle_zoom();

        // The zoomed pane fills the area between bar and dock, which stay in place.
        let panes = tab.iter_panes();
        let geometry: Vec<_> = panes
            .iter()
            .map(|p| (p.pane.pane_id(), p.index, p.left, p.width, p.is_zoomed))
            .collect();
        assert_eq!(
            geometry,
            vec![
                (3, 0, 0, 5, false),
                (1, 1, 6, 73, true),
                (4, 3, 80, 20, false)
            ]
        );
        assert_eq!(panes[1].pixel_width, 730);
        assert_eq!(p1.cols(), 73);
        assert!(panes[1].is_active);
        // Only the dividers beside chrome are visible.
        let splits: Vec<_> = tab.iter_splits().iter().map(|s| s.left).collect();
        assert_eq!(splits, vec![79, 5]);

        // Focus moves: to the bar keeps the zoom, to the hidden p2 is an ordinary move.
        assert_eq!(
            tab.inner.lock().chrome_zoom_direction(PaneDirection::Left),
            Some(Some(0))
        );
        assert_eq!(
            tab.inner.lock().chrome_zoom_direction(PaneDirection::Right),
            None
        );
        tab.inner.lock().active = 3;
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 4);
        assert_eq!(tab.get_zoomed_pane().unwrap().pane_id(), 1);
        assert_eq!(
            tab.inner.lock().chrome_zoom_direction(PaneDirection::Left),
            Some(Some(1))
        );
        tab.inner.lock().active = 1;

        // A resize while zoomed: chrome keeps its width, the zoomed pane takes the change.
        tab.resize(TerminalSize {
            rows: 30,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 600,
            dpi: 96,
        });
        assert_eq!((bar.cols(), dock.cols(), p1.cols()), (5, 20, 93));

        // Unzoom restores the grid (grown by the resize).
        tab.toggle_zoom();
        assert!(tab.get_zoomed_pane().is_none());
        assert_eq!(p1.cols() + 1 + p2.cols(), 93);
        assert!(p1.cols() > p1_cols && p2.cols() > p2_cols);
        assert_eq!(tab.iter_panes().len(), 4);
        assert_eq!(tab.iter_splits().len(), 3);

        // A split while zoomed (top-level too) unzooms.
        tab.toggle_zoom();
        assert!(tab.get_zoomed_pane().is_some());
        tab.compute_split_size(
            1,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                top_level: true,
                ..Default::default()
            },
        );
        assert!(tab.get_zoomed_pane().is_none());
    }

    #[test]
    fn zooming_chrome_or_a_tab_without_chrome_fills_the_tab() {
        let (tab, _) = chrome_tab(true);
        tab.inner.lock().active = 3; // the dock
        tab.toggle_zoom();
        let panes = tab.iter_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!((panes[0].pane.pane_id(), panes[0].width), (4, 100));
        assert!(tab.iter_splits().is_empty());
    }

    fn is_send_and_sync<T: Send + Sync>() -> bool {
        true
    }

    #[test]
    fn tab_is_send_and_sync() {
        assert!(is_send_and_sync::<Tab>());
    }
}
