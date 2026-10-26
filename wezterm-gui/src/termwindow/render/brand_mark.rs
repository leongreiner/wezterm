use crate::color::LinearRgba;
use crate::termwindow::box_model::*;
use crate::termwindow::render::brand_mark_shapes::*;
use crate::termwindow::UIItemType;
use config::{Dimension, SrgbaTuple, TabBarBrand};
use std::rc::Rc;
use wezterm_font::LoadedFont;

/// Id the mark reports in `status-click`.
const BRAND_CLICK_ID: &str = "brand";

/// How far an outline that was not configured sits between the fill and white.
const OUTLINE_LIGHTEN: f32 = 0.55;

fn lighten(c: SrgbaTuple, t: f32) -> SrgbaTuple {
    SrgbaTuple(
        c.0 + (1. - c.0) * t,
        c.1 + (1. - c.1) * t,
        c.2 + (1. - c.2) * t,
        c.3,
    )
}

fn layer(poly: &'static [crate::customglyph::Poly], color: SrgbaTuple) -> MarkLayer {
    MarkLayer {
        poly,
        color: color.to_linear(),
    }
}

/// `color` at `alpha` coverage, premultiplied in linear space as the quad
/// blending expects.
fn translucent_layer(
    poly: &'static [crate::customglyph::Poly],
    color: SrgbaTuple,
    alpha: f32,
) -> MarkLayer {
    let LinearRgba(r, g, b, _) = color.to_linear();
    MarkLayer {
        poly,
        color: LinearRgba(r, g, b, alpha),
    }
}

/// The Hivegrid mark as one element: its polys are static, only the colours
/// come from the config. The element reports clicks as the status segment
/// `brand`; `scale` converts the configured 96 dpi pixels.
pub fn brand_mark_element(
    font: &Rc<LoadedFont>,
    brand: &TabBarBrand,
    scale: f32,
    zindex: i8,
) -> Element {
    let colors = &brand.colors;
    let center: SrgbaTuple = *colors.center;
    let glow: SrgbaTuple = colors.glow.map(|c| *c).unwrap_or(center);

    let mut layers = vec![];
    // Translucent discs, widest first: their sum fades out from the centre
    // cell.
    for (disc, alpha) in GLOW_DISCS.iter().zip([0.04, 0.06, 0.08, 0.11]) {
        layers.push(translucent_layer(disc, glow, alpha));
    }

    let ring = |i: usize| -> SrgbaTuple {
        if colors.ring.is_empty() {
            center
        } else {
            *colors.ring[i % colors.ring.len()]
        }
    };
    let fills: Vec<SrgbaTuple> = std::iter::once(center).chain((0..6).map(ring)).collect();
    let outlines: Vec<SrgbaTuple> = std::iter::once(
        colors
            .center_outline
            .map(|c| *c)
            .unwrap_or_else(|| lighten(center, OUTLINE_LIGHTEN)),
    )
    .chain((0..6).map(|i| match colors.ring_outline.get(i) {
        Some(c) => **c,
        None => lighten(ring(i), OUTLINE_LIGHTEN),
    }))
    .collect();

    for i in 0..7 {
        layers.push(layer(HEX_FILLS[i], fills[i]));
        layers.push(layer(HEX_OUTLINES[i], outlines[i]));
    }
    let core = lighten(center, 0.45);
    layers.push(translucent_layer(CENTER_CORE, core, 0.55));

    let size = Dimension::Pixels((brand.size as f32 * scale).round().max(8.));
    Element::new(
        font,
        ElementContent::Mark {
            width: size,
            height: size,
            layers,
        },
    )
    .vertical_align(VerticalAlign::Middle)
    .item_type(UIItemType::StatusClick(BRAND_CLICK_ID.to_string()))
    .margin(BoxDimension {
        left: Dimension::Pixels(brand.margin_left as f32 * scale),
        right: Dimension::Pixels(brand.margin_right as f32 * scale),
        top: Dimension::Pixels(0.),
        bottom: Dimension::Pixels(0.),
    })
    .zindex(zindex)
}
