//! Drawing of web panes (Hivegrid fork).
//!
//! A `mux::webpane::WebPane` with a frame is not drawn by the cell renderer. `paint_pane`
//! hands it to [`TermWindow::paint_web_pane`], which uploads the frame into a per-pane texture
//! (kept in [`WebRenderState`], keyed by pane id, reallocated when the frame size changes,
//! re-uploaded when its sequence number changes) and queues a [`WebDraw`]. After the zindex-0
//! layer, `call_draw_glium` / `call_draw_webgpu` draw the queued quads with the glyph program
//! and its "background image" mode (`has_color = 2.0`) with the web texture bound as the
//! atlas, then the focus border as solid quads (`has_color = 3.0`). Layers above (fancy tab
//! bar, modals) therefore still cover the page.
//!
//! A web pane without a frame (starting, browser exited) is drawn by the normal cell renderer:
//! its placeholder text grid.
//!
//! Visibility: panes painted in a pass are visible; the ones painted in the previous pass but
//! not in this one (other tab, overlay, closed) are told `set_visible(false)`, which stops the
//! screencast and freezes the page.

use crate::quad::Vertex;
use crate::renderstate::RenderState;
use ::window::bitmaps::Texture2d;
use ::window::color::LinearRgba;
use ::window::{BitmapImage, Point, Rect, RectF, Size};
use hivegrid_web::Frame;
use mux::pane::PaneId;
use mux::tab::PositionedPane;
use mux::webpane::WebPane;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Thickness in pixels of the focus border drawn over the edge of the active web pane.
pub const FOCUS_BORDER_PX: f32 = 2.0;

/// Window geometry a pane rectangle is derived from, as `paint_pane` computes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneMetrics {
    pub cell_width: f32,
    pub cell_height: f32,
    /// `window_padding.left`
    pub padding_left: f32,
    /// Left OS border.
    pub border_left: f32,
    /// Top of the first text row: tab bar (when at the top), padding and top OS border.
    pub top_pixel_y: f32,
}

/// The pixel rectangle of a pane that spans `cols` x `rows` cells from cell (`left`, `top`):
/// where its cells are drawn, and where the page texture goes.
pub fn pane_pixel_rect(
    left: usize,
    top: usize,
    cols: usize,
    rows: usize,
    m: &PaneMetrics,
) -> RectF {
    euclid::rect(
        m.padding_left + m.border_left + left as f32 * m.cell_width,
        m.top_pixel_y + top as f32 * m.cell_height,
        cols as f32 * m.cell_width,
        rows as f32 * m.cell_height,
    )
}

/// One queued draw: a page texture in `rect`, with an optional border over its edge.
pub struct WebDraw {
    pub rect: RectF,
    pub tex: Rc<dyn Texture2d>,
    pub border: Option<LinearRgba>,
}

/// Quads of a [`WebDraw`] for the glyph program: the textured page quad first, then up to four
/// solid border strips. Positions are relative to the window centre like every other quad.
pub struct WebMesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

const IS_BG_IMAGE: f32 = 2.0;
const IS_SOLID_COLOR: f32 = 3.0;

fn push_quad(
    mesh: &mut WebMesh,
    rect: RectF,
    window: (f32, f32),
    has_color: f32,
    color: LinearRgba,
) {
    let (ox, oy) = (window.0 / 2.0, window.1 / 2.0);
    let (l, t, r, b) = (
        rect.min_x() - ox,
        rect.min_y() - oy,
        rect.max_x() - ox,
        rect.max_y() - oy,
    );
    let solid = has_color == IS_SOLID_COLOR;
    let vertex = |position: [f32; 2], tex: [f32; 2]| Vertex {
        position,
        tex: if solid { [0.0, 0.0] } else { tex },
        fg_color: color.into(),
        alt_color: color.into(),
        hsv: [1.0, 1.0, 1.0],
        has_color,
        mix_value: 0.0,
    };
    let first = mesh.vertices.len() as u32;
    // Same order as `QuadTrait::set_position`: top left, top right, bottom left, bottom right.
    mesh.vertices.push(vertex([l, t], [0.0, 0.0]));
    mesh.vertices.push(vertex([r, t], [1.0, 0.0]));
    mesh.vertices.push(vertex([l, b], [0.0, 1.0]));
    mesh.vertices.push(vertex([r, b], [1.0, 1.0]));
    mesh.indices
        .extend_from_slice(&[first, first + 1, first + 2, first + 1, first + 3, first + 2]);
}

impl WebDraw {
    pub fn mesh(&self, window: (f32, f32)) -> WebMesh {
        let mut mesh = WebMesh {
            vertices: Vec::with_capacity(20),
            indices: Vec::with_capacity(30),
        };
        push_quad(
            &mut mesh,
            self.rect,
            window,
            IS_BG_IMAGE,
            LinearRgba::with_components(1.0, 1.0, 1.0, 1.0),
        );
        if let Some(color) = self.border {
            let r = self.rect;
            let t = FOCUS_BORDER_PX
                .min(r.size.width / 2.0)
                .min(r.size.height / 2.0);
            let strips = [
                euclid::rect(r.min_x(), r.min_y(), r.size.width, t),
                euclid::rect(r.min_x(), r.max_y() - t, r.size.width, t),
                euclid::rect(r.min_x(), r.min_y() + t, t, r.size.height - 2.0 * t),
                euclid::rect(r.max_x() - t, r.min_y() + t, t, r.size.height - 2.0 * t),
            ];
            for strip in strips {
                push_quad(&mut mesh, strip, window, IS_SOLID_COLOR, color);
            }
        }
        mesh
    }
}

/// A frame as a read-only bitmap, so it can be uploaded without a copy.
struct FrameImage<'a> {
    frame: &'a Frame,
    /// A private copy, made only if something asks for mutable access (texture uploads never
    /// do); the shared frame is never written.
    copy: Option<Vec<u8>>,
}

impl<'a> FrameImage<'a> {
    fn new(frame: &'a Frame) -> Self {
        Self { frame, copy: None }
    }
}

// `pixel_data` points at `width * height * 4` bytes owned by the frame (or by the private
// copy), which outlive the image.
impl BitmapImage for FrameImage<'_> {
    unsafe fn pixel_data(&self) -> *const u8 {
        match &self.copy {
            Some(c) => c.as_ptr(),
            None => self.frame.rgba.as_ptr(),
        }
    }

    unsafe fn pixel_data_mut(&mut self) -> *mut u8 {
        let frame = self.frame;
        self.copy.get_or_insert_with(|| frame.rgba.clone()).as_mut_ptr()
    }

    fn image_dimensions(&self) -> (usize, usize) {
        (self.frame.width as usize, self.frame.height as usize)
    }
}

struct WebSurface {
    tex: Rc<dyn Texture2d>,
    width: u32,
    height: u32,
    /// What was uploaded last: the frame's sequence number and the address of the frame (a
    /// restarted view numbers its frames from 1 again; the address tells two such frames apart).
    uploaded: Option<(u64, usize)>,
}

/// Page textures by pane.
#[derive(Default)]
pub struct WebSurfaces {
    map: HashMap<PaneId, WebSurface>,
}

impl WebSurfaces {
    /// The texture holding `frame` for `pane`: allocated through `alloc` when there is none or
    /// the frame size changed, uploaded when the frame is not the one already in it.
    pub fn sync(
        &mut self,
        pane: PaneId,
        frame: &std::sync::Arc<Frame>,
        alloc: impl FnOnce(u32, u32) -> anyhow::Result<Rc<dyn Texture2d>>,
    ) -> anyhow::Result<Rc<dyn Texture2d>> {
        let fits = matches!(
            self.map.get(&pane),
            Some(s) if s.width == frame.width && s.height == frame.height
        );
        if !fits {
            self.map.remove(&pane);
            let tex = alloc(frame.width, frame.height)?;
            self.map.insert(
                pane,
                WebSurface {
                    tex,
                    width: frame.width,
                    height: frame.height,
                    uploaded: None,
                },
            );
        }
        let surface = self.map.get_mut(&pane).expect("inserted above");
        let key = (frame.seq, std::sync::Arc::as_ptr(frame) as usize);
        if surface.uploaded != Some(key) {
            surface.tex.write(
                Rect::new(
                    Point::new(0, 0),
                    Size::new(frame.width as isize, frame.height as isize),
                ),
                &FrameImage::new(frame),
            );
            surface.uploaded = Some(key);
        }
        Ok(Rc::clone(&surface.tex))
    }

    pub fn forget(&mut self, pane: PaneId) {
        self.map.remove(&pane);
    }

    /// Drop the textures of panes that were not painted (hidden tabs): the GPU memory goes
    /// back, and the next paint uploads the latest frame again.
    pub fn retain_painted(&mut self, painted: &HashSet<PaneId>) {
        self.map.retain(|id, _| painted.contains(id));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// Which panes become visible or hidden, given the panes painted before and now (both sorted).
pub fn visibility_changes(
    before: &HashSet<PaneId>,
    now: &HashSet<PaneId>,
) -> (Vec<PaneId>, Vec<PaneId>) {
    let mut show: Vec<PaneId> = now.difference(before).copied().collect();
    let mut hide: Vec<PaneId> = before.difference(now).copied().collect();
    show.sort_unstable();
    hide.sort_unstable();
    (show, hide)
}

/// Everything the web panes of a window keep between frames. Lives in `RenderState`, so it is
/// dropped with the GL/wgpu context its textures belong to.
#[derive(Default)]
pub struct WebRenderState {
    pub draws: RefCell<Vec<WebDraw>>,
    surfaces: RefCell<WebSurfaces>,
    /// Panes painted in the current pass.
    painted: RefCell<HashSet<PaneId>>,
    /// Panes told they are visible.
    visible: RefCell<HashSet<PaneId>>,
    /// Panes whose texture could not be allocated, with the frame size that failed: logged once,
    /// drawn as their placeholder until the frame size changes.
    alloc_failed: RefCell<HashMap<PaneId, (u32, u32)>>,
}

impl WebRenderState {
    /// Start of a paint pass.
    pub fn begin_pass(&self) {
        self.draws.borrow_mut().clear();
        self.painted.borrow_mut().clear();
    }

    /// End of a paint pass: panes to show and to hide.
    pub fn end_pass(&self) -> (Vec<PaneId>, Vec<PaneId>) {
        let painted = self.painted.borrow();
        let mut visible = self.visible.borrow_mut();
        let changes = visibility_changes(&visible, &painted);
        visible.clone_from(&painted);
        self.surfaces.borrow_mut().retain_painted(&painted);
        self.alloc_failed.borrow_mut().retain(|id, _| painted.contains(id));
        changes
    }
}

/// What `paint_pane` still has to draw of a pane after `paint_web_pane`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebPaint {
    /// Not a web pane, or one without a frame (starting, crashed, no texture): the pane's cells
    /// are drawn as for any pane.
    Cells,
    /// The page is queued as a texture: nothing else to draw.
    Page,
    /// The page is queued as a texture below the header row, which is drawn from the pane's
    /// cells. No scroll bar or anything else of a terminal pane.
    PageWithHeader,
}

impl crate::TermWindow {
    /// Called by `paint_pane` for every pane: queues the page of a web pane that has a frame
    /// (below its header row, when it has one) and says what is left to draw.
    pub fn paint_web_pane(
        &mut self,
        pos: &PositionedPane,
        metrics: &PaneMetrics,
    ) -> anyhow::Result<WebPaint> {
        let web = match pos.pane.downcast_ref::<WebPane>() {
            Some(web) => web,
            None => return Ok(WebPaint::Cells),
        };
        let pane_id = pos.pane.pane_id();
        let state = self.render_state.as_ref().unwrap();
        state.web.painted.borrow_mut().insert(pane_id);

        let frame = match web.frame() {
            Some(frame) => frame,
            None => {
                state.web.surfaces.borrow_mut().forget(pane_id);
                return Ok(WebPaint::Cells);
            }
        };
        if state.web.alloc_failed.borrow().get(&pane_id) == Some(&(frame.width, frame.height)) {
            return Ok(WebPaint::Cells);
        }
        let context = state.context.clone();
        let synced = state
            .web
            .surfaces
            .borrow_mut()
            .sync(pane_id, &frame, |w, h| {
                context.allocate_texture(w as usize, h as usize)
            });
        let tex = match synced {
            Ok(tex) => {
                state.web.alloc_failed.borrow_mut().remove(&pane_id);
                tex
            }
            Err(err) => {
                // One pane that cannot get a texture (larger than the GPU allows, out of
                // memory) must not stop the rest of the window from being drawn.
                log::error!(
                    "web pane {pane_id}: cannot allocate a {}x{} texture, showing its placeholder: {err:#}",
                    frame.width,
                    frame.height
                );
                state
                    .web
                    .alloc_failed
                    .borrow_mut()
                    .insert(pane_id, (frame.width, frame.height));
                state.web.surfaces.borrow_mut().forget(pane_id);
                return Ok(WebPaint::Cells);
            }
        };
        let border = if pos.is_active {
            Some(pos.pane.palette().cursor_bg.to_linear())
        } else {
            None
        };
        let header_rows = web.header_rows().min(pos.height.saturating_sub(1));
        state.web.draws.borrow_mut().push(WebDraw {
            rect: pane_pixel_rect(
                pos.left,
                pos.top + header_rows,
                pos.width,
                pos.height - header_rows,
                metrics,
            ),
            tex,
            border,
        });
        Ok(if header_rows == 0 { WebPaint::Page } else { WebPaint::PageWithHeader })
    }

    /// Called at the end of a paint pass: tell web panes that appeared or went away.
    pub fn web_end_pass(&mut self) {
        let (show, hide) = self.render_state.as_ref().unwrap().web.end_pass();
        if show.is_empty() && hide.is_empty() {
            return;
        }
        let mux = mux::Mux::get();
        for (ids, visible) in [(hide, false), (show, true)] {
            for id in ids {
                if let Some(pane) = mux.get_pane(id) {
                    if let Some(web) = pane.downcast_ref::<WebPane>() {
                        web.set_visible(visible);
                    }
                }
            }
        }
    }
}

/// What `draw.rs` has at hand when it reaches the web draws of a frame.
pub struct DrawEnv<'a> {
    pub window_size: (f32, f32),
    pub projection: &'a [[f32; 4]; 4],
    pub foreground_text_hsb: (f32, f32, f32),
    pub milliseconds: u32,
}

pub fn draw_glium(
    state: &RenderState,
    frame: &mut ::window::glium::Frame,
    env: &DrawEnv,
    blink: [&crate::colorease::ColorEaseUniform; 3],
) -> anyhow::Result<()> {
    use crate::uniforms::UniformBuilder;
    use ::window::glium::texture::SrgbTexture2d;
    use ::window::glium::uniforms::{
        MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
    };
    use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};

    let draws = state.web.draws.borrow();
    if draws.is_empty() {
        return Ok(());
    }
    let context = match &state.context {
        crate::renderstate::RenderContext::Glium(context) => context,
        _ => unreachable!("draw_glium on a wgpu context"),
    };
    let alpha_blending = ::window::glium::DrawParameters {
        blend: ::window::glium::Blend {
            color: BlendingFunction::Addition {
                source: LinearBlendingFactor::SourceAlpha,
                destination: LinearBlendingFactor::OneMinusSourceAlpha,
            },
            alpha: BlendingFunction::Addition {
                source: LinearBlendingFactor::One,
                destination: LinearBlendingFactor::OneMinusSourceAlpha,
            },
            constant_value: (0.0, 0.0, 0.0, 0.0),
        },
        ..Default::default()
    };
    for draw in draws.iter() {
        let tex = draw
            .tex
            .downcast_ref::<SrgbTexture2d>()
            .expect("web textures of a glium context are SrgbTexture2d");
        let sampler = Sampler::new(tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Linear)
            .minify_filter(MinifySamplerFilter::Linear);
        let mesh = draw.mesh(env.window_size);
        let vertices = ::window::glium::VertexBuffer::new(context, &mesh.vertices)?;
        let indices = ::window::glium::IndexBuffer::new(
            context,
            ::window::glium::index::PrimitiveType::TrianglesList,
            &mesh.indices,
        )?;
        let mut uniforms = UniformBuilder::default();
        uniforms.add("projection", env.projection);
        uniforms.add("atlas_nearest_sampler", &sampler);
        uniforms.add("atlas_linear_sampler", &sampler);
        uniforms.add("foreground_text_hsb", &env.foreground_text_hsb);
        uniforms.add("subpixel_aa", &false);
        uniforms.add("milliseconds", &env.milliseconds);
        uniforms.add_struct("cursor_blink", blink[0]);
        uniforms.add_struct("blink", blink[1]);
        uniforms.add_struct("rapid_blink", blink[2]);
        frame.draw(
            &vertices,
            &indices,
            state.glyph_prog.as_ref().unwrap(),
            &uniforms,
            &alpha_blending,
        )?;
    }
    Ok(())
}

/// Draw the queued web panes into the current wgpu frame. `cleared` is whether something
/// already cleared the target in this frame.
pub fn draw_webgpu(
    webgpu: &crate::termwindow::webgpu::WebGpuState,
    state: &RenderState,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    cleared: &mut bool,
    env: &DrawEnv,
) -> anyhow::Result<()> {
    use crate::termwindow::webgpu::{ShaderUniform, WebGpuTexture};
    use wgpu::util::DeviceExt;

    let draws = state.web.draws.borrow();
    for draw in draws.iter() {
        let tex = draw
            .tex
            .downcast_ref::<WebGpuTexture>()
            .expect("web textures of a wgpu context are WebGpuTexture");
        let texture_view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = |sampler: &wgpu::Sampler, label| {
            webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &webgpu.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
                label: Some(label),
            })
        };
        let linear = bind_group(&webgpu.texture_linear_sampler, "web linear bind group");
        let nearest = bind_group(&webgpu.texture_nearest_sampler, "web nearest bind group");

        let mesh = draw.mesh(env.window_size);
        let vertex_buffer = webgpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("web vertices"),
                contents: bytemuck::cast_slice(&mesh.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let index_buffer = webgpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("web indices"),
                contents: bytemuck::cast_slice(&mesh.indices),
                usage: wgpu::BufferUsages::INDEX,
            });
        let uniforms = webgpu.create_uniform(ShaderUniform {
            foreground_text_hsb: [
                env.foreground_text_hsb.0,
                env.foreground_text_hsb.1,
                env.foreground_text_hsb.2,
            ],
            milliseconds: env.milliseconds,
            projection: *env.projection,
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Web Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: if *cleared {
                        wgpu::LoadOp::Load
                    } else {
                        wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.,
                            g: 0.,
                            b: 0.,
                            a: 0.,
                        })
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
        });
        *cleared = true;
        pass.set_pipeline(&webgpu.render_pipeline);
        pass.set_bind_group(0, &uniforms, &[]);
        pass.set_bind_group(1, &linear, &[]);
        pass.set_bind_group(2, &nearest, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.indices.len() as u32, 0, 0..1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;

    #[test]
    fn frame_image_mutable_access_copies_and_leaves_the_frame_alone() {
        let frame = Frame {
            seq: 1,
            width: 2,
            height: 1,
            rgba: vec![1, 2, 3, 255, 4, 5, 6, 255],
        };
        let mut im = FrameImage::new(&frame);
        im.pixel_data_slice_mut()[0] = 99;
        assert_eq!(im.pixel_data_slice()[0], 99);
        assert_eq!(frame.rgba[0], 1);
    }

    struct FakeTexture {
        width: usize,
        height: usize,
        writes: RefCell<Vec<(Rect, Vec<u8>)>>,
        id: usize,
    }

    impl Texture2d for FakeTexture {
        fn write(&self, rect: Rect, im: &dyn BitmapImage) {
            assert_eq!(
                (rect.size.width as usize, rect.size.height as usize),
                im.image_dimensions(),
                "rect and image must agree"
            );
            self.writes
                .borrow_mut()
                .push((rect, im.pixel_data_slice().to_vec()));
        }
        fn read(&self, _rect: Rect, _im: &mut dyn BitmapImage) {
            unimplemented!()
        }
        fn width(&self) -> usize {
            self.width
        }
        fn height(&self) -> usize {
            self.height
        }
    }

    fn frame(seq: u64, w: u32, h: u32, fill: u8) -> Arc<Frame> {
        Arc::new(Frame {
            seq,
            width: w,
            height: h,
            rgba: vec![fill; (w * h * 4) as usize],
        })
    }

    fn fake(
        allocs: &Cell<usize>,
    ) -> impl FnOnce(u32, u32) -> anyhow::Result<Rc<dyn Texture2d>> + '_ {
        move |w, h| {
            allocs.set(allocs.get() + 1);
            Ok(Rc::new(FakeTexture {
                width: w as usize,
                height: h as usize,
                writes: RefCell::new(vec![]),
                id: allocs.get(),
            }))
        }
    }

    fn as_fake(t: &Rc<dyn Texture2d>) -> &FakeTexture {
        t.downcast_ref::<FakeTexture>().unwrap()
    }

    #[test]
    fn pane_rect_follows_cells_padding_border_and_tab_bar() {
        let m = PaneMetrics {
            cell_width: 10.0,
            cell_height: 20.0,
            padding_left: 4.0,
            border_left: 1.0,
            // tab bar 30 + padding 6 + border 1
            top_pixel_y: 37.0,
        };
        assert_eq!(
            pane_pixel_rect(0, 0, 80, 24, &m),
            euclid::rect(5.0, 37.0, 800.0, 480.0)
        );
        assert_eq!(
            pane_pixel_rect(40, 12, 40, 12, &m),
            euclid::rect(405.0, 277.0, 400.0, 240.0)
        );
        // Fractional cell metrics are kept (no rounding): the page is scaled by the sampler.
        let m = PaneMetrics {
            cell_width: 9.5,
            ..m
        };
        assert_eq!(pane_pixel_rect(2, 0, 10, 1, &m).size.width, 95.0);
        assert_eq!(pane_pixel_rect(2, 0, 10, 1, &m).origin.x, 24.0);
    }

    #[test]
    fn first_frame_allocates_and_uploads() {
        let allocs = Cell::new(0);
        let mut s = WebSurfaces::default();
        let f = frame(1, 4, 2, 7);
        let tex = s.sync(1, &f, fake(&allocs)).unwrap();
        assert_eq!(allocs.get(), 1);
        let t = as_fake(&tex);
        assert_eq!((t.width, t.height), (4, 2));
        let writes = t.writes.borrow();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, Rect::new(Point::new(0, 0), Size::new(4, 2)));
        assert_eq!(writes[0].1, vec![7; 32]);
    }

    #[test]
    fn same_frame_is_not_uploaded_twice_and_new_seq_reuses_the_texture() {
        let allocs = Cell::new(0);
        let mut s = WebSurfaces::default();
        let f1 = frame(1, 4, 2, 1);
        let a = s.sync(1, &f1, fake(&allocs)).unwrap();
        let b = s.sync(1, &f1, fake(&allocs)).unwrap();
        assert!(Rc::ptr_eq(&a, &b));
        assert_eq!(as_fake(&a).writes.borrow().len(), 1);
        let f2 = frame(2, 4, 2, 2);
        let c = s.sync(1, &f2, fake(&allocs)).unwrap();
        assert!(Rc::ptr_eq(&a, &c));
        assert_eq!(allocs.get(), 1, "same size: no reallocation");
        let writes = as_fake(&c).writes.borrow();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1].1[0], 2);
    }

    #[test]
    fn a_new_frame_with_a_reused_sequence_number_is_still_uploaded() {
        // A restarted view numbers its frames from 1 again.
        let allocs = Cell::new(0);
        let mut s = WebSurfaces::default();
        let f1 = frame(1, 2, 2, 1);
        let tex = s.sync(1, &f1, fake(&allocs)).unwrap();
        let f2 = frame(1, 2, 2, 9);
        s.sync(1, &f2, fake(&allocs)).unwrap();
        let writes = as_fake(&tex).writes.borrow();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1].1[0], 9);
    }

    #[test]
    fn a_size_change_allocates_a_new_texture() {
        let allocs = Cell::new(0);
        let mut s = WebSurfaces::default();
        let a = s.sync(1, &frame(1, 4, 2, 1), fake(&allocs)).unwrap();
        let b = s.sync(1, &frame(2, 6, 2, 1), fake(&allocs)).unwrap();
        assert!(!Rc::ptr_eq(&a, &b));
        assert_eq!(allocs.get(), 2);
        assert_eq!(as_fake(&b).id, 2);
        assert_eq!(as_fake(&b).width, 6);
        // The new texture got its full upload.
        assert_eq!(as_fake(&b).writes.borrow().len(), 1);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn panes_have_their_own_textures() {
        let allocs = Cell::new(0);
        let mut s = WebSurfaces::default();
        let f = frame(1, 2, 2, 1);
        let a = s.sync(1, &f, fake(&allocs)).unwrap();
        let b = s.sync(2, &f, fake(&allocs)).unwrap();
        assert!(!Rc::ptr_eq(&a, &b));
        assert_eq!(s.len(), 2);
        s.forget(1);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn allocation_failure_is_reported_and_leaves_nothing_behind() {
        let mut s = WebSurfaces::default();
        let err = s
            .sync(1, &frame(1, 2, 2, 1), |_, _| anyhow::bail!("too big"))
            .err()
            .expect("allocation fails");
        assert!(format!("{err:#}").contains("too big"));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn end_of_pass_hides_what_was_not_painted_and_frees_its_texture() {
        let allocs = Cell::new(0);
        let st = WebRenderState::default();
        let f = frame(1, 2, 2, 1);

        st.begin_pass();
        for id in [3, 5] {
            st.painted.borrow_mut().insert(id);
            st.surfaces
                .borrow_mut()
                .sync(id, &f, fake(&allocs))
                .unwrap();
        }
        assert_eq!(st.end_pass(), (vec![3, 5], vec![]));

        // Nothing changed.
        st.begin_pass();
        for id in [3, 5] {
            st.painted.borrow_mut().insert(id);
        }
        assert_eq!(st.end_pass(), (vec![], vec![]));

        // Pane 5's tab was left.
        st.begin_pass();
        st.painted.borrow_mut().insert(3);
        assert_eq!(st.end_pass(), (vec![], vec![5]));
        assert_eq!(st.surfaces.borrow().len(), 1);

        // And came back.
        st.begin_pass();
        for id in [3, 5] {
            st.painted.borrow_mut().insert(id);
        }
        assert_eq!(st.end_pass(), (vec![5], vec![]));
    }

    #[test]
    fn begin_pass_clears_the_queue() {
        let allocs = Cell::new(0);
        let st = WebRenderState::default();
        let tex = st
            .surfaces
            .borrow_mut()
            .sync(1, &frame(1, 2, 2, 1), fake(&allocs))
            .unwrap();
        st.draws.borrow_mut().push(WebDraw {
            rect: euclid::rect(0.0, 0.0, 10.0, 10.0),
            tex,
            border: None,
        });
        st.begin_pass();
        assert!(st.draws.borrow().is_empty());
    }

    fn draw(border: Option<LinearRgba>) -> WebDraw {
        let allocs = Cell::new(0);
        let tex = WebSurfaces::default()
            .sync(1, &frame(1, 2, 2, 1), fake(&allocs))
            .unwrap();
        WebDraw {
            rect: euclid::rect(100.0, 50.0, 400.0, 200.0),
            tex,
            border,
        }
    }

    #[test]
    fn page_quad_covers_the_rect_with_the_whole_texture() {
        // Window 800x600: positions are relative to the centre (400, 300).
        let mesh = draw(None).mesh((800.0, 600.0));
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.indices, vec![0, 1, 2, 1, 3, 2]);
        let v = &mesh.vertices;
        assert_eq!(v[0].position, [-300.0, -250.0]);
        assert_eq!(v[1].position, [100.0, -250.0]);
        assert_eq!(v[2].position, [-300.0, -50.0]);
        assert_eq!(v[3].position, [100.0, -50.0]);
        assert_eq!(v[0].tex, [0.0, 0.0]);
        assert_eq!(v[1].tex, [1.0, 0.0]);
        assert_eq!(v[2].tex, [0.0, 1.0]);
        assert_eq!(v[3].tex, [1.0, 1.0]);
        for vert in v {
            assert_eq!(vert.has_color, 2.0, "the 'background image' mode");
            assert_eq!(vert.fg_color, [1.0, 1.0, 1.0, 1.0], "opaque, untinted");
            assert_eq!(vert.hsv, [1.0, 1.0, 1.0]);
        }
    }

    #[test]
    fn focus_border_is_four_solid_strips_inside_the_rect() {
        let accent = LinearRgba::with_components(0.1, 0.2, 0.3, 1.0);
        let mesh = draw(Some(accent)).mesh((800.0, 600.0));
        assert_eq!(mesh.vertices.len(), 20);
        assert_eq!(mesh.indices.len(), 30);
        assert_eq!(*mesh.indices.last().unwrap(), 18);
        let solid = &mesh.vertices[4..];
        assert!(solid.iter().all(|v| v.has_color == 3.0));
        assert!(solid.iter().all(|v| v.fg_color == [0.1, 0.2, 0.3, 1.0]));
        // Every strip lies within the page rect and the strips are FOCUS_BORDER_PX thick.
        let (l, t, r, b) = (-300.0, -250.0, 100.0, -50.0);
        for quad in solid.chunks(4) {
            for v in quad {
                assert!(v.position[0] >= l && v.position[0] <= r);
                assert!(v.position[1] >= t && v.position[1] <= b);
            }
        }
        // top strip
        assert_eq!(solid[0].position, [l, t]);
        assert_eq!(solid[3].position, [r, t + FOCUS_BORDER_PX]);
        // bottom strip
        assert_eq!(solid[4].position, [l, b - FOCUS_BORDER_PX]);
        assert_eq!(solid[7].position, [r, b]);
        // left and right strips do not overlap the top and bottom ones
        assert_eq!(solid[8].position, [l, t + FOCUS_BORDER_PX]);
        assert_eq!(
            solid[11].position,
            [l + FOCUS_BORDER_PX, b - FOCUS_BORDER_PX]
        );
        assert_eq!(
            solid[12].position,
            [r - FOCUS_BORDER_PX, t + FOCUS_BORDER_PX]
        );
    }

    #[test]
    fn border_never_exceeds_a_tiny_pane() {
        let mut d = draw(Some(LinearRgba::with_components(1.0, 0.0, 0.0, 1.0)));
        d.rect = euclid::rect(0.0, 0.0, 3.0, 3.0);
        let mesh = d.mesh((100.0, 100.0));
        for v in &mesh.vertices[4..] {
            assert!(v.position[0] >= -50.0 && v.position[0] <= -47.0);
            assert!(v.position[1] >= -50.0 && v.position[1] <= -47.0);
        }
    }

    #[test]
    fn visibility_changes_are_sorted_set_differences() {
        let a: HashSet<PaneId> = [1, 2, 9].iter().copied().collect();
        let b: HashSet<PaneId> = [2, 7, 4].iter().copied().collect();
        assert_eq!(visibility_changes(&a, &b), (vec![4, 7], vec![1, 9]));
        assert_eq!(visibility_changes(&a, &a), (vec![], vec![]));
    }
}
