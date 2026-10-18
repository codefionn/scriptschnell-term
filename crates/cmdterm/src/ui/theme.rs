//! Design tokens. Worktrees are branches and branches have lanes: each
//! worktree gets a lane color by its position in the project's worktree list,
//! and that color marks its rail row, the active tab underline and the focused
//! pane border. Everything else stays quiet.
//!
//! A [`Scheme`] holds every token for one look, including the terminal's
//! ANSI colors. The settings pick one by id, or follow the desktop between
//! Spruce and Spruce light.

use crate::state::SYSTEM_SCHEME;
use crate::term::{Attention, TermColors};
use schnellui::scene::Color;
use schnellui::widgets::{
    ComponentInteractions, InteractionStates, InteractionStyle, Shape, Theme,
};

use crate::workspace::Inner;

/// Every token of one color scheme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scheme {
    /// Saved in `settings.json`; never change a published id.
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    /// Terminal and page background.
    pub page: Color,
    /// Rail and status bar background.
    pub rail: Color,
    /// Hover and input background.
    pub raised: Color,
    /// Dividers.
    pub line: Color,
    /// Text.
    pub ink: Color,
    /// Secondary text.
    pub muted: Color,
    /// Lane colors in assignment order. Lane 0 is the accent and the cursor,
    /// lane 1 the attention color, lane 4 the positive color.
    pub lanes: [Color; 6],
    /// Background of selected terminal text.
    pub selection: Color,
    /// Dims the app behind the command palette and the settings.
    pub backdrop: Color,
    /// The terminal's 16 ANSI colors. `None` keeps the emulator's xterm defaults.
    pub ansi: Option<[Color; 16]>,
}

/// Colors of the file reader, taken from a [`Scheme`] when its view mounts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReaderColors {
    pub page: Color,
    /// Background of the header row.
    pub header: Color,
    pub ink: Color,
    pub muted: Color,
    pub line: Color,
    /// The selected tree row while the tree has focus: the same tint as a
    /// chosen row in the rail and the settings.
    pub selected: Color,
    /// The bar at the left edge of the selected row while its side has
    /// focus, as the rail marks its chosen row.
    pub accent: Color,
    /// The divider's grip: a step above the divider itself, so it reads
    /// as draggable without outshining the text.
    pub grip: Color,
    /// The selected tree row while the reader has focus.
    pub unfocused: Color,
    pub folder: Color,
    /// One color per `files::highlight::Token`, in its order.
    pub syntax: [Color; 11],
    /// Markers and backgrounds of added and removed diff lines.
    pub added: Color,
    pub removed: Color,
    pub added_page: Color,
    pub removed_page: Color,
    /// Diff hunk headers.
    pub hunk: Color,
}

const fn hex(rgb: u32) -> Color {
    Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

const fn ansi(colors: [u32; 16]) -> Option<[Color; 16]> {
    let mut out = [Color::rgb(0, 0, 0); 16];
    let mut i = 0;
    while i < 16 {
        out[i] = hex(colors[i]);
        i += 1;
    }
    Some(out)
}

pub const SPRUCE: Scheme = Scheme {
    id: "spruce",
    name: "Spruce",
    dark: true,
    page: hex(0x10171c),
    rail: hex(0x162027),
    raised: hex(0x1e2a32),
    line: hex(0x2b3943),
    ink: hex(0xd7e0e2),
    // Light enough for 4.5:1 on the chosen row's tint as well as on the
    // rail, since a chosen worktree's path and counts are muted text.
    muted: hex(0xafbdc3),
    // Sage, brass, periwinkle, rose, moss, heather.
    lanes: [
        hex(0x7fb7a4),
        hex(0xc9a86a),
        hex(0x8fa3d6),
        hex(0xd08c8c),
        hex(0xa3c27a),
        hex(0xb595c9),
    ],
    selection: hex(0x3a4a5c),
    backdrop: Color::rgba(0x05, 0x08, 0x0a, 0xfa),
    // The lanes' hues, so the terminal matches the chrome around it, and
    // every color but black at 4.5:1 or more on the page: bright black
    // carries prompts and comments, and xterm's grey falls to 2.4:1 here.
    ansi: ansi([
        0x1e2a32, 0xd9777a, 0xa3c27a, 0xd6b26e, 0x8fa3d6, 0xb595c9, 0x76b9b8, 0xc5ced2, 0x7c8a90,
        0xeba3a3, 0xbad692, 0xe8cb8c, 0xabbbe8, 0xcdb0dd, 0x98d2cf, 0xeef2f3,
    ]),
};

pub const SPRUCE_LIGHT: Scheme = Scheme {
    id: "spruce-light",
    name: "Spruce light",
    dark: false,
    page: hex(0xf5f7f6),
    rail: hex(0xeaefed),
    raised: hex(0xdfe6e3),
    line: hex(0xcbd5d1),
    ink: hex(0x1d2a30),
    muted: hex(0x445358),
    // Dark enough for 4.5:1 as text on the raised surface.
    lanes: [
        hex(0x326f5c),
        hex(0x7f6021),
        hex(0x4d61a5),
        hex(0xa24948),
        hex(0x4b6f25),
        hex(0x7d529b),
    ],
    selection: hex(0xc8d6e5),
    // The ink at under half: enough to set the dialog apart from the page,
    // which turns muddy at half or more.
    backdrop: Color::rgba(0x1d, 0x2a, 0x30, 0x70),
    ansi: ansi([
        0x1d2a30, 0xb0504f, 0x4b6f25, 0x6e531a, 0x4f63a8, 0x8457a3, 0x326f5c, 0x8a9894, 0x5f7178,
        0xbb5452, 0x5a7f2c, 0x7a5d1a, 0x6276bd, 0x9a6bb8, 0x3b7f6a, 0xb4c0bc,
    ]),
};

pub const GRUVBOX_DARK: Scheme = Scheme {
    id: "gruvbox-dark",
    name: "Gruvbox dark",
    dark: true,
    page: hex(0x282828),
    rail: hex(0x32302f),
    raised: hex(0x3c3836),
    line: hex(0x504945),
    ink: hex(0xebdbb2),
    muted: hex(0xb1a28a),
    lanes: [
        hex(0x8ec07c),
        hex(0xfabd2f),
        hex(0x83a598),
        hex(0xfb4934),
        hex(0xb8bb26),
        hex(0xd3869b),
    ],
    selection: hex(0x504945),
    backdrop: Color::rgba(0x10, 0x10, 0x10, 0xfa),
    ansi: ansi([
        0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, 0x928374,
        0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
    ]),
};

pub const SOLARIZED_LIGHT: Scheme = Scheme {
    id: "solarized-light",
    name: "Solarized light",
    dark: false,
    page: hex(0xfdf6e3),
    rail: hex(0xeee8d5),
    raised: hex(0xe6dfca),
    line: hex(0xd6ceb5),
    ink: hex(0x073642),
    muted: hex(0x475a60),
    // Solarized's accents, darkened to 4.5:1 as text on the raised
    // surface; the terminal keeps the original ones.
    lanes: [
        hex(0x1c6c66),
        hex(0x7b5d00),
        hex(0x1b669a),
        hex(0xb92a27),
        hex(0x5a6700),
        hex(0xae2c6b),
    ],
    selection: hex(0xe2dcc8),
    backdrop: Color::rgba(0x07, 0x36, 0x42, 0x70),
    ansi: ansi([
        0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, 0x002b36,
        0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
    ]),
};

pub const NORD: Scheme = Scheme {
    id: "nord",
    name: "Nord",
    dark: true,
    page: hex(0x2e3440),
    rail: hex(0x3b4252),
    raised: hex(0x434c5e),
    line: hex(0x4c566a),
    ink: hex(0xd8dee9),
    muted: hex(0xb5becf),
    lanes: [
        hex(0x88c0d0),
        hex(0xebcb8b),
        hex(0x81a1c1),
        hex(0xbf616a),
        hex(0xa3be8c),
        hex(0xb48ead),
    ],
    selection: hex(0x4c566a),
    backdrop: Color::rgba(0x1c, 0x20, 0x28, 0xfa),
    ansi: ansi([
        0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0, 0x4c566a,
        0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
    ]),
};

/// Every scheme in the order the settings list them.
pub const SCHEMES: [&Scheme; 5] = [
    &SPRUCE,
    &SPRUCE_LIGHT,
    &GRUVBOX_DARK,
    &SOLARIZED_LIGHT,
    &NORD,
];

/// The scheme for a saved id. [`SYSTEM_SCHEME`] and unknown ids follow the
/// desktop: Spruce when it prefers dark, Spruce light otherwise.
pub fn resolve(id: &str, system_dark: bool) -> &'static Scheme {
    if id != SYSTEM_SCHEME
        && let Some(scheme) = SCHEMES.into_iter().find(|scheme| scheme.id == id)
    {
        return scheme;
    }
    if system_dark { &SPRUCE } else { &SPRUCE_LIGHT }
}

/// Share of the lane color in the background of added and removed lines,
/// out of 256. Terminal cells are opaque, so the mix happens here.
const DIFF_WASH: u16 = 44;
/// Share of the lane color in the background of removed lines, out of 256.
const REMOVED_WASH: u16 = 54;
/// Share of the ink, out of 256, in a keycap pill's fill.
const KEYCAP_SHARE: u16 = 56;
/// The same on a light page, where 56 darkens the pill until its muted
/// keys fall under 4.5:1.
const KEYCAP_SHARE_LIGHT: u16 = 18;
/// The same on a dark page, where rose at 54 still reads as brown.
const REMOVED_WASH_DARK: u16 = 62;
/// Share of the ink in the removed color, out of 256.
const REMOVED_LIFT: u16 = 48;
/// The same on a dark page, where the removed lines' wash is redder.
const REMOVED_LIFT_DARK: u16 = 64;
/// Share of the ink in the added lines' color on a light scheme.
const ADDED_DARKEN: u16 = 96;

/// `base` with `share`/256 of `over` mixed in.
pub(super) fn mix(base: Color, over: Color, share: u16) -> Color {
    let channel =
        |a: u8, b: u8| ((u16::from(a) * (256 - share) + u16::from(b) * share) / 256) as u8;
    Color::rgb(
        channel(base.r, over.r),
        channel(base.g, over.g),
        channel(base.b, over.b),
    )
}

/// Share of the ink, out of 256, in link text on a light scheme.
const LIGHT_LINK_SHARE: u16 = 80;

/// Share of the muted text color in the switch track, out of 256.
const MEDIA_SHARE: u16 = 180;
/// The same on dark schemes: dark enough that the light off knob stands out
/// from it at 3:1 or more, light enough that the track does from the page.
const MEDIA_SHARE_DARK: u16 = 132;
/// Share of the muted text color in control frames, out of 256, on light
/// and on dark schemes.
const OUTLINE_SHARE: u16 = 150;
const OUTLINE_SHARE_DARK: u16 = 110;
/// Share of the page color in filled controls on dark schemes, out of 256.
const FILL_SHARE: u16 = 12;
/// Share of the page color in the focus ring, out of 256.
const FOCUS_SHARE: u16 = 48;
/// Share of the ink in the focus ring on dark schemes, out of 256.
const FOCUS_INK_SHARE: u16 = 96;
/// Share of the rail in a text field's hint, out of 256: dimmer than typed
/// text, but still 3:1 on the raised field in every scheme.
const HINT_SHARE: u16 = 56;
/// Share of the page color in a light field's hint, out of 256.
const LIGHT_HINT_SHARE: u16 = 40;
/// Share of the warning color in a warning callout's fill, out of 256.
const WARNING_SHARE: u16 = 28;
/// Padding scale of dropdowns and combo boxes, against 0.8 elsewhere.
const CONTROL_DENSITY: f32 = 1.5;
/// Share of the accent in the chosen row's background, out of 256, on dark
/// and on light schemes. The hover wash blends in linear light, where a
/// faint wash over a dark surface shows about as much as a third of the
/// accent mixed in sRGB, so dark schemes need the larger share for the
/// chosen row to stay the stronger one.
const SELECTED_SHARE: (u16, u16) = (64, 46);

/// Alpha of the ink wash over a hovered and a pressed control, for dark
/// and light schemes. Blending happens in linear light, where a dark wash
/// over a light page shows far less than a light wash over a dark one, so
/// light schemes need about twice the alpha.
/// On dark schemes the wash stays below the chosen row's fill, so a
/// hovered row never reads as more chosen than the chosen one, but lifts
/// the rail by about 32 levels per channel, enough to see at a glance.
const HOVER_WASH_ALPHA: (u8, u8) = (0x32, 0x22);
/// Alpha of the tab strip's hover wash on dark schemes.
const TAB_HOVER_ALPHA: u8 = 0x26;
/// Share of the lane in the dark schemes' hover wash, out of 256.
const HOVER_LANE_SHARE: u16 = 64;
/// Share of the lane in the dark schemes' tab hover wash, out of 256: less
/// than a row's, so a hovered tab never reads as the active tab's tint.
const TAB_HOVER_LANE_SHARE: u16 = 64;
const ACTIVE_WASH_ALPHA: (u8, u8) = (0x2c, 0x50);

fn with_alpha(color: Color, alpha: u8) -> Color {
    Color::rgba(color.r, color.g, color.b, alpha)
}

fn rgb(color: Color) -> (u8, u8, u8) {
    (color.r, color.g, color.b)
}

impl Scheme {
    /// Color of the lane at `index`; the palette repeats after six lanes.
    pub fn lane(&self, index: usize) -> Color {
        self.lanes[index % self.lanes.len()]
    }

    /// The accent at the dark or light alpha of `alpha`: hover and press
    /// take the hue of the chosen rows' tint, a step fainter, so the three
    /// states read as one family.
    /// Hover is a neutral wash of the ink, so it reads as "under the
    /// pointer" and never as the lane-tinted chosen row.
    fn hover_wash(&self) -> Color {
        if self.dark {
            // Mostly ink with a little of the lane, so hover belongs to the
            // scheme without reading as the chosen row's tint.
            with_alpha(
                mix(self.ink, self.lanes[0], HOVER_LANE_SHARE),
                HOVER_WASH_ALPHA.0,
            )
        } else {
            // A grey wash of the ink: the lane's own wash looked like the
            // chosen row's tint, a step fainter.
            with_alpha(self.ink, HOVER_WASH_ALPHA.1)
        }
    }

    /// `theme` with the tab strip's hover: the label turns to full ink over
    /// a wash fainter than the shown tab's fill, so a hovered tab reads as
    /// under the pointer and never as the shown one.
    pub fn tab_theme(&self, mut theme: Theme) -> Theme {
        let wash = if self.dark {
            with_alpha(
                mix(self.ink, self.lanes[0], TAB_HOVER_LANE_SHARE),
                TAB_HOVER_ALPHA,
            )
        } else {
            self.hover_wash()
        };
        theme.interactions.hover = InteractionStyle {
            background: Some(wash),
            foreground: Some(self.ink),
            border: None,
        };
        theme
    }

    fn accent_wash(&self, (dark, light): (u8, u8)) -> Color {
        with_alpha(self.lanes[0], if self.dark { dark } else { light })
    }

    /// The theme of switches: the full lane color when on, under a knob in
    /// the page color, so on reads apart from the off track.
    ///
    /// Off, a dark scheme draws a light knob in ink on its dark track. A
    /// light scheme frames the track in the muted text color and draws the
    /// knob in it too, past 3:1 on the page, where a white knob on a pale
    /// track would vanish. In both the switch never reads as disabled.
    pub fn switch_theme(&self) -> Theme {
        let base = self.theme();
        if self.dark {
            Theme {
                accent: self.lanes[0],
                on_accent: self.page,
                text_muted: self.ink,
                ..base
            }
        } else {
            Theme {
                accent: self.lanes[0],
                on_accent: self.page,
                text_muted: self.muted,
                outline: self.muted,
                ..base
            }
        }
    }

    /// The SchnellUI theme the whole app mounts with.
    pub fn theme(&self) -> Theme {
        let accent = self.lanes[0];
        Theme {
            text: self.ink,
            text_muted: self.muted,
            surface: self.raised,
            surface_muted: self.rail,
            separator: self.line,
            // Frames of fields, outlined buttons and dropdowns, and the
            // scroll thumb: a step stronger than the dividers, about 3:1 on
            // the rail, so a control's edge shows before the pointer finds it.
            outline: mix(
                self.line,
                self.muted,
                if self.dark {
                    OUTLINE_SHARE_DARK
                } else {
                    OUTLINE_SHARE
                },
            ),
            // Filled controls take the lane color under page-colored text
            // in every scheme. On a dark scheme the fill is a step deeper,
            // so it does not glare, but stays bright enough that a chosen
            // or primary button never reads as a disabled one.
            accent: if self.dark {
                mix(accent, self.page, FILL_SHARE)
            } else {
                accent
            },
            on_accent: self.page,
            selection: with_alpha(accent, 0x38),
            // Hover and press lay a translucent ink wash over the control. An
            // opaque fill would cover controls with no text to slip under, such
            // as switches, checkboxes and the ghost buttons over rail icons.
            interactions: InteractionStates {
                hover: InteractionStyle::background(self.hover_wash()),
                focus: InteractionStyle::border(self.focus_ring()),
                active: InteractionStyle::background(self.accent_wash(ACTIVE_WASH_ALPHA)),
            },
            component_interactions: ComponentInteractions::NONE,
            text_selection: with_alpha(accent, 0x55),
            // The rail itself on a dark scheme, inside a divider-colored
            // frame: a disabled button sinks into the surface it sits on
            // instead of reading as pressed, and never as the main one nor
            // as an outlined one at rest.
            disabled: if self.dark {
                self.rail
            } else {
                mix(self.rail, self.line, 72)
            },
            positive: self.lanes[4],
            attention: self.lanes[1],
            // The off track of switches and the empty part of sliders: a
            // step lighter than the raised surface, so an off switch still
            // reads as a control on the rail and on the settings surface.
            media: mix(
                self.raised,
                self.muted,
                if self.dark {
                    MEDIA_SHARE_DARK
                } else {
                    MEDIA_SHARE
                },
            ),
            page: self.page,
            shape: Shape {
                roundness: 1.0,
                density: 0.8,
                frame: 1.0,
                shadow: 0.0,
            },
        }
    }

    /// The theme panels mount with: [`Scheme::theme`] with an invisible
    /// focus ring on terminals and scroll views, whose pane frame or cursor
    /// shows focus instead.
    pub fn panel_theme(&self) -> Theme {
        Theme {
            component_interactions: ComponentInteractions {
                raw_surface: Some(InteractionStates {
                    hover: InteractionStyle::NONE,
                    focus: InteractionStyle::border(Color::TRANSPARENT),
                    active: InteractionStyle::NONE,
                }),
                ..ComponentInteractions::NONE
            },
            ..self.theme()
        }
    }

    /// The 1 px frame of a pane without focus: a step above the divider
    /// color, so a split still shows where each pane ends.
    pub fn pane_edge(&self) -> Color {
        // A light page washes a thin line out more: a larger step there.
        mix(
            self.line,
            self.muted,
            if self.dark {
                OUTLINE_SHARE_DARK / 2
            } else {
                OUTLINE_SHARE_DARK
            },
        )
    }

    /// Fill of a keycap pill. Apart from the row at rest, hovered and
    /// chosen, so the keys never melt into a row's wash.
    pub fn keycap(&self) -> Color {
        let share = if self.dark {
            KEYCAP_SHARE
        } else {
            KEYCAP_SHARE_LIGHT
        };
        mix(self.line, self.ink, share)
    }

    /// Fill of a keycap pill on a chosen row. On a light page the chosen
    /// row's tint and [`Scheme::keycap`] come out alike, so the pill takes
    /// the raised surface there and stands out lighter than its row.
    pub fn keycap_on_selected(&self) -> Color {
        if self.dark {
            self.keycap()
        } else {
            self.raised
        }
    }

    /// Background of the chosen row in a list: the raised surface with a
    /// share of the accent, so the choice reads apart from a hovered row and
    /// from the raised inputs beside it.
    pub fn selected(&self) -> Color {
        self.tinted(self.lanes[0])
    }

    /// The fill of a warning callout: the raised surface with a share of
    /// the warning color, so the warning outweighs the plain panels.
    pub fn warning_surface(&self) -> Color {
        mix(self.raised, self.lanes[1], WARNING_SHARE)
    }

    /// [`Scheme::selected`] in another color, for a chosen option whose
    /// meaning has one, such as Block or Allow.
    pub fn tinted(&self, color: Color) -> Color {
        let (dark, light) = SELECTED_SHARE;
        mix(self.raised, color, if self.dark { dark } else { light })
    }

    /// The keyboard focus ring: the accent mixed a quarter of the way to
    /// the page. SchnellUI swaps a ring in the exact accent color for black
    /// or white, because the accent fails contrast against itself, and a 3px
    /// white box reads as an error. The mix keeps 3:1 against the rail.
    fn focus_ring(&self) -> Color {
        // On a dark scheme the ring is lighter than the lane, toward the ink,
        // so it reads apart from the lane-tinted selection and filled buttons.
        if self.dark {
            mix(self.lanes[0], self.ink, FOCUS_INK_SHARE)
        } else {
            mix(self.lanes[0], self.page, FOCUS_SHARE)
        }
    }

    /// The theme for dropdowns and combo boxes: more padding, so they stand
    /// as tall as the buttons and text fields beside them.
    pub fn control_theme(&self) -> Theme {
        let mut theme = self.theme();
        theme.shape.density = CONTROL_DENSITY;
        theme
    }

    /// The theme with muted text, for section titles and meta lines.
    pub fn muted_theme(&self) -> Theme {
        Theme {
            text: self.muted,
            ..self.theme()
        }
    }

    /// The theme for a text field whose hint must not read as typed text:
    /// the hint fades toward the background.
    pub fn field_theme(&self) -> Theme {
        if self.dark {
            Theme {
                text_muted: mix(self.muted, self.rail, HINT_SHARE),
                ..self.theme()
            }
        } else {
            // A light field takes the page color, a step lighter than the
            // dialogs and the rail it sits on, so its bounds show without
            // the frame. Its hint steps toward that color, so a hint and a
            // typed value differ, and keeps 4.5:1.
            Theme {
                surface: self.page,
                text_muted: mix(self.muted, self.page, LIGHT_HINT_SHARE),
                ..self.theme()
            }
        }
    }

    /// The theme with accent text, for rows that act like links.
    pub fn link_theme(&self) -> Theme {
        Theme {
            // On a light scheme the lane color is a step lighter than body
            // text and fell under 4.5:1 on the row's hover wash, so it
            // leans toward the ink.
            text: if self.dark {
                self.lanes[0]
            } else {
                mix(self.lanes[0], self.ink, LIGHT_LINK_SHARE)
            },
            ..self.theme()
        }
    }

    /// The color that marks a terminal's attention state in the rail:
    /// rose for a permission request, the attention color for input, the
    /// positive color for a finished turn, muted text while busy.
    pub fn attention_color(&self, attention: Attention) -> Color {
        match attention {
            Attention::Permission => self.lanes[3],
            Attention::Input => self.lanes[1],
            Attention::Done => self.lanes[4],
            Attention::Busy | Attention::None => self.muted,
        }
    }

    /// The theme a [`schnellui::widgets::Badge`] for `attention` mounts with.
    pub fn attention_theme(&self, attention: Attention) -> Theme {
        Theme {
            attention: self.attention_color(attention),
            on_accent: self.page,
            ..self.theme()
        }
    }

    /// The file reader's colors. Syntax tokens take the lane colors, so they
    /// follow the scheme like everything else.
    pub fn reader_colors(&self) -> ReaderColors {
        let [sage, brass, periwinkle, rose, moss, heather] = self.lanes;
        ReaderColors {
            page: self.page,
            header: self.rail,
            ink: self.ink,
            muted: self.muted,
            line: self.line,
            selected: self.selected(),
            accent: self.lanes[0],
            grip: mix(self.line, self.muted, OUTLINE_SHARE_DARK),
            // Still plainly chosen while the other side has the keys, but
            // without the focused side's full tint.
            unfocused: mix(self.raised, self.selected(), 160),
            folder: periwinkle,
            // In `files::highlight::Token` order.
            syntax: [
                self.ink, heather, sage, periwinkle, moss, brass, brass, self.muted, rose, rose,
                sage,
            ],
            // On a light page the lane alone is under 3.5:1 for the small
            // `+` and counts; a step toward the ink brings it past 4.5:1.
            added: if self.dark {
                moss
            } else {
                mix(moss, self.ink, ADDED_DARKEN)
            },
            // A step lighter than the lane, so the `-` and the counts keep
            // 4.5:1 on the removed lines' wash.
            removed: mix(
                rose,
                self.ink,
                if self.dark {
                    REMOVED_LIFT_DARK
                } else {
                    REMOVED_LIFT
                },
            ),
            added_page: mix(self.page, moss, DIFF_WASH),
            // Rose is the paler lane in most schemes; a larger share keeps
            // its wash as clearly red as the added lines' is green.
            removed_page: mix(
                self.page,
                rose,
                if self.dark {
                    REMOVED_WASH_DARK
                } else {
                    REMOVED_WASH
                },
            ),
            // Darker on a light page, where the lane alone falls under 4.5:1.
            hunk: if self.dark {
                periwinkle
            } else {
                mix(periwinkle, self.ink, 64)
            },
        }
    }

    /// The terminal's default colors and ANSI palette.
    pub fn term_colors(&self) -> TermColors {
        TermColors {
            foreground: rgb(self.ink),
            background: rgb(self.page),
            cursor: rgb(self.lanes[0]),
            ansi: self.ansi.map(|colors| colors.map(rgb)),
        }
    }
}

/// Lane index of the worktree at `path` in the project's worktree list. Bare
/// entries count, so the index matches the order the VCS reports. Unknown
/// worktrees and projects without a snapshot use lane 0.
pub fn lane_index<P: AsRef<std::path::Path>>(worktrees: &[P], path: &std::path::Path) -> usize {
    worktrees
        .iter()
        .position(|worktree| worktree.as_ref() == path)
        .unwrap_or(0)
}

/// Lane color of the shown worktree.
pub fn active_lane(inner: &Inner) -> Color {
    let scheme = inner.scheme;
    let Some(project) = inner.state.active() else {
        return scheme.lane(0);
    };
    let paths: Vec<&std::path::Path> = inner
        .snapshots
        .get(&project.id)
        .map(|snapshot| {
            snapshot
                .worktrees
                .iter()
                .map(|worktree| worktree.path.as_path())
                .collect()
        })
        .unwrap_or_default();
    scheme.lane(lane_index(&paths, project.current_worktree()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast of two colors.
    fn contrast(a: Color, b: Color) -> f32 {
        let linear = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let luma = |c: Color| 0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b);
        let (x, y) = (luma(a), luma(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn spruce_terminal_colors_read_on_its_page() {
        let ansi = SPRUCE.ansi.expect("Spruce has its own palette");
        for (index, color) in ansi.iter().enumerate().skip(1) {
            let ratio = contrast(*color, SPRUCE.page);
            assert!(ratio >= 4.5, "ANSI {index}: {ratio}");
        }
    }

    #[test]
    fn diff_signs_read_on_their_line_washes() {
        for scheme in SCHEMES {
            let colors = scheme.reader_colors();
            for (sign, wash) in [
                (colors.added, colors.added_page),
                (colors.removed, colors.removed_page),
            ] {
                let ratio = contrast(sign, wash);
                assert!(ratio >= 3.0, "{}: {ratio}", scheme.id);
            }
        }
        let colors = SPRUCE.reader_colors();
        let ratio = contrast(colors.removed, colors.removed_page);
        assert!(ratio >= 4.5, "{ratio}");
    }

    #[test]
    fn field_hints_read_on_their_fields() {
        for scheme in SCHEMES {
            let field = scheme.field_theme();
            let ratio = contrast(field.text_muted, field.surface);
            let least = if scheme.dark { 3.0 } else { 4.5 };
            assert!(ratio >= least, "{}: {ratio}", scheme.id);
        }
    }

    #[test]
    fn muted_text_reads_on_the_chosen_row() {
        let ratio = contrast(SPRUCE.muted, SPRUCE.selected());
        assert!(ratio >= 4.5, "{ratio}");
    }

    #[test]
    fn the_terminal_palette_follows_the_tokens() {
        for scheme in SCHEMES {
            let colors = scheme.term_colors();
            assert_eq!(colors.foreground, rgb(scheme.ink), "{}", scheme.id);
            assert_eq!(colors.background, rgb(scheme.page), "{}", scheme.id);
            assert_eq!(colors.cursor, rgb(scheme.lanes[0]), "{}", scheme.id);
        }
    }

    #[test]
    fn scheme_ids_are_unique_and_not_the_system_id() {
        for (index, scheme) in SCHEMES.iter().enumerate() {
            assert_ne!(scheme.id, SYSTEM_SCHEME);
            assert!(
                SCHEMES[index + 1..]
                    .iter()
                    .all(|other| other.id != scheme.id)
            );
        }
    }

    #[test]
    fn system_and_unknown_ids_follow_the_desktop() {
        assert_eq!(resolve(SYSTEM_SCHEME, true).id, "spruce");
        assert_eq!(resolve(SYSTEM_SCHEME, false).id, "spruce-light");
        assert_eq!(resolve("no-such-scheme", true).id, "spruce");
        assert_eq!(resolve("nord", false).id, "nord");
    }

    #[test]
    fn light_schemes_have_dark_text_on_a_light_page() {
        let luma =
            |c: Color| 0.2126 * f32::from(c.r) + 0.7152 * f32::from(c.g) + 0.0722 * f32::from(c.b);
        for scheme in SCHEMES {
            assert_eq!(
                luma(scheme.page) < luma(scheme.ink),
                scheme.dark,
                "{}",
                scheme.id
            );
        }
    }

    #[test]
    fn lanes_follow_the_palette_order_and_wrap() {
        assert_eq!(SPRUCE.lane(0), hex(0x7fb7a4));
        assert_eq!(SPRUCE.lane(1), hex(0xc9a86a));
        assert_eq!(SPRUCE.lane(5), hex(0xb595c9));
        assert_eq!(SPRUCE.lane(6), SPRUCE.lane(0));
        assert_eq!(SPRUCE.lane(13), SPRUCE.lane(1));
    }

    #[test]
    fn lane_index_is_the_position_in_the_worktree_list() {
        use std::path::{Path, PathBuf};
        let list: Vec<PathBuf> = ["/r", "/r-bare", "/r-a", "/r-b"]
            .map(PathBuf::from)
            .to_vec();
        assert_eq!(lane_index(&list, Path::new("/r")), 0);
        assert_eq!(lane_index(&list, Path::new("/r-a")), 2);
        assert_eq!(lane_index(&list, Path::new("/r-b")), 3);
        assert_eq!(lane_index(&list, Path::new("/elsewhere")), 0);
        assert_eq!(lane_index::<PathBuf>(&[], Path::new("/r")), 0);
    }

    #[test]
    fn a_chosen_row_stands_apart_from_a_hovered_one() {
        // The chosen row's tint is opaque; hover is a wash at a low alpha
        // over the same surface, so it must stay the fainter one.
        for scheme in SCHEMES {
            assert_ne!(scheme.selected(), scheme.raised, "{}", scheme.name);
            let (dark, light) = HOVER_WASH_ALPHA;
            let alpha = if scheme.dark { dark } else { light };
            let (dark, light) = SELECTED_SHARE;
            let share = if scheme.dark { dark } else { light };
            // Fainter than the chosen row's share.
            assert!(u16::from(alpha) * 256 / 255 < share, "{}", scheme.name);
        }
    }
}
