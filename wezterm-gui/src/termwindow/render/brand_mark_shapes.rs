// Geometry of the Hivegrid mark: the app icon's seven flat-topped hexagons
// (assets/make-icon.py there), centre cell plus six around it, clockwise from
// the top. Coordinates are thousandths of the square the mark is drawn in.
use crate::customglyph::*;

const HEX_CENTER_FILL: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(662), BlockCoord::Permille(500)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(640)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(640)),
        PolyCommand::LineTo(BlockCoord::Permille(338), BlockCoord::Permille(500)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(360)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(360)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

const HEX_CENTER_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(662), BlockCoord::Permille(500)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(640)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(640)),
        PolyCommand::LineTo(BlockCoord::Permille(338), BlockCoord::Permille(500)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(360)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(360)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineFine,
}];

const HEX_TOP_FILL: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(662), BlockCoord::Permille(158)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(298)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(298)),
        PolyCommand::LineTo(BlockCoord::Permille(338), BlockCoord::Permille(158)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(18)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(18)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

const HEX_TOP_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(662), BlockCoord::Permille(158)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(298)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(298)),
        PolyCommand::LineTo(BlockCoord::Permille(338), BlockCoord::Permille(158)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(18)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(18)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineFine,
}];

const HEX_UPPER_RIGHT_FILL: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(958), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(634), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(189)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(189)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

const HEX_UPPER_RIGHT_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(958), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(634), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(189)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(189)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineFine,
}];

const HEX_LOWER_RIGHT_FILL: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(958), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(634), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(531)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(531)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

const HEX_LOWER_RIGHT_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(958), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(634), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(715), BlockCoord::Permille(531)),
        PolyCommand::LineTo(BlockCoord::Permille(877), BlockCoord::Permille(531)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineFine,
}];

const HEX_BOTTOM_FILL: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(662), BlockCoord::Permille(842)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(982)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(982)),
        PolyCommand::LineTo(BlockCoord::Permille(338), BlockCoord::Permille(842)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(702)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(702)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

const HEX_BOTTOM_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(662), BlockCoord::Permille(842)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(982)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(982)),
        PolyCommand::LineTo(BlockCoord::Permille(338), BlockCoord::Permille(842)),
        PolyCommand::LineTo(BlockCoord::Permille(419), BlockCoord::Permille(702)),
        PolyCommand::LineTo(BlockCoord::Permille(581), BlockCoord::Permille(702)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineFine,
}];

const HEX_LOWER_LEFT_FILL: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(366), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(42), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(531)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(531)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

const HEX_LOWER_LEFT_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(366), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(811)),
        PolyCommand::LineTo(BlockCoord::Permille(42), BlockCoord::Permille(671)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(531)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(531)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineFine,
}];

const HEX_UPPER_LEFT_FILL: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(366), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(42), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(189)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(189)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

const HEX_UPPER_LEFT_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Permille(366), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(469)),
        PolyCommand::LineTo(BlockCoord::Permille(42), BlockCoord::Permille(329)),
        PolyCommand::LineTo(BlockCoord::Permille(123), BlockCoord::Permille(189)),
        PolyCommand::LineTo(BlockCoord::Permille(285), BlockCoord::Permille(189)),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineFine,
}];

/// Fill of each hexagon, index 0 the centre, then the ring clockwise from the top.
pub const HEX_FILLS: [&[Poly]; 7] = [
    HEX_CENTER_FILL,
    HEX_TOP_FILL,
    HEX_UPPER_RIGHT_FILL,
    HEX_LOWER_RIGHT_FILL,
    HEX_BOTTOM_FILL,
    HEX_LOWER_LEFT_FILL,
    HEX_UPPER_LEFT_FILL,
];

/// Outline of each hexagon, same order as `HEX_FILLS`.
pub const HEX_OUTLINES: [&[Poly]; 7] = [
    HEX_CENTER_OUTLINE,
    HEX_TOP_OUTLINE,
    HEX_UPPER_RIGHT_OUTLINE,
    HEX_LOWER_RIGHT_OUTLINE,
    HEX_BOTTOM_OUTLINE,
    HEX_LOWER_LEFT_OUTLINE,
    HEX_UPPER_LEFT_OUTLINE,
];

/// Concentric discs behind the centre cell; drawn translucent they fake its glow.
pub const GLOW_DISCS: [&[Poly]; 4] = [
    &[Poly {
        path: &[PolyCommand::Circle {
            center: (BlockCoord::Permille(500), BlockCoord::Permille(500)),
            radius: BlockCoord::Permille(330),
        }],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Fill,
    }],
    &[Poly {
        path: &[PolyCommand::Circle {
            center: (BlockCoord::Permille(500), BlockCoord::Permille(500)),
            radius: BlockCoord::Permille(290),
        }],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Fill,
    }],
    &[Poly {
        path: &[PolyCommand::Circle {
            center: (BlockCoord::Permille(500), BlockCoord::Permille(500)),
            radius: BlockCoord::Permille(250),
        }],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Fill,
    }],
    &[Poly {
        path: &[PolyCommand::Circle {
            center: (BlockCoord::Permille(500), BlockCoord::Permille(500)),
            radius: BlockCoord::Permille(210),
        }],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Fill,
    }],
];

/// Bright core over the centre cell.
pub const CENTER_CORE: &[Poly] = &[Poly {
    path: &[PolyCommand::Circle {
        center: (BlockCoord::Permille(500), BlockCoord::Permille(500)),
        radius: BlockCoord::Permille(80),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];
