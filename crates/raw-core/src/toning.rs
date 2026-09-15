//! Chemical toning — a composition of species per pixel, not a gradient map.
//!
//! The design and the argument against the prototype's duotone gradient map are in the
//! Toning brief in `docs/`. What it comes down to: a gradient map has no state for what
//! the print was dipped in before, no particles, and is lightness-preserving, so it
//! cannot express gold going blue on silver and red on a sulphide-toned print, and it
//! tints paper white — which has no image substance in it at all.
//! `the_bare_substrate_is_untouched_by_any_treatment` is that last one as a test.
//!
//! # The model
//!
//! Each level carries an **amount of each species** — silver, sulphide, selenide,
//! gold-on-silver. A bath converts between species or deposits a new one, and colour
//! and density follow from the totals:
//!
//! ```text
//! kappa  = sum(amount_i * kappa_i)     -> density, so the tonal scale moves
//! (a, b) = sum(amount_i * tint_i)      -> hue and chroma, in OKLab's plane
//! ```
//!
//! Conversion preserves the total and additive baths grow it, so one accumulator covers
//! both classes and an additive bath deepens the print for free.
//!
//! # Substance and density are two quantities
//!
//! They coincide for every print — more silver is darker — which makes the conflation
//! easy to write and never notice. A daguerreotype separates them: its material is a
//! light-*scattering* texture forming the **highlights**, and the bare mirror is black.
//!
//! So the reaction runs on **substance** and [`Optics`] owns the mapping to density, in
//! one sign. Gold warming a daguerreotype's highlights where it blackens a print's
//! shadows is then derived rather than special-cased —
//! `a_daguerreotype_carries_its_substance_in_the_highlights`.
//!
//! Substance is linear in **density**, not lightness: `D = eps * c * l` is what a
//! densitometer measures, which is why this module converts to density internally. The
//! panel still shows L\*; that conversion belongs at the widget.
//!
//! # Fineness does double duty
//!
//! [`Process::fineness`] sets both the reaction rate and the untoned hue, because finely
//! divided silver is warm and finer particles are more surface per unit substance. One
//! number explains the native colour of half the process list and the toning behaviour
//! of all of it, which is why a process costs a table row rather than a code path.
//!
//! It also means warmth and toning speed are **not** independently tunable, which is
//! correct: a process warm because it is fine also tones fast because it is fine.
//!
//! # The numbers here were set by eye
//!
//! **No tint, kappa or selectivity below is a measurement.** Stated plainly because the
//! alternative is a table that reads as measured to whoever finds it next. The Toning
//! brief records the capture protocol for making real ones.
//!
//! # This bakes to a LUT
//!
//! The chemistry is global and only the substance varies per pixel, so the model is a
//! one-dimensional function of level and [`ToningParams::bake`] collapses it to a table,
//! as [`Curve::bake`](crate::curve::Curve::bake) does. It can therefore be as elaborate
//! as it needs to be without costing anything per pixel.

use crate::curve::Curve;
use crate::display::lstar_encode;

/// How a process's material relates to how dark it looks.
///
/// The one field that lets a daguerreotype into an absorption model. See the module
/// note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Optics {
    /// More material is darker. Every print — silver, platinum, cyanotype, carbon.
    Absorbing,
    /// More material is *lighter*: a daguerreotype's scattering texture forms the
    /// highlights and the bare plate is the black.
    Scattering,
}

/// A hue and a chroma, in OKLCh. Degrees, and OKLab chroma units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tint {
    pub hue: f32,
    pub chroma: f32,
}

impl Tint {
    pub const NEUTRAL: Self = Self {
        hue: 0.0,
        chroma: 0.0,
    };

    /// The OKLab `(a, b)` this tint points at.
    fn ab(self) -> (f32, f32) {
        let r = self.hue.to_radians();
        (self.chroma * r.cos(), self.chroma * r.sin())
    }

    fn from_ab(a: f32, b: f32) -> Self {
        Self {
            hue: b.atan2(a).to_degrees().rem_euclid(360.0),
            chroma: a.hypot(b),
        }
    }
}

impl Default for Tint {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

/// What the image is made of, and what it is on.
///
/// A process is a **starting composition** plus three numbers. That is the whole of
/// why the alt-process list is cheap: none of these needs a code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Process {
    #[default]
    GelatinSilver,
    SaltPrint,
    Albumen,
    CollodionPop,
    /// Iron-silver, and **the best fit in the list**: an untoned kallitype is famously
    /// unstable, so gold, platinum or palladium toning is part of the process rather
    /// than an option on top of it.
    Kallitype,
    /// A simplified kallitype. Brown because its silver is very finely divided, and
    /// for no other reason — which makes it this model's test case.
    Vandyke,
    PlatinumPalladium,
    /// A palladium process whose colour is controlled in the *sensitizer* rather than
    /// by a bath — gold chloride for cool and split tones. So it is a **mixed starting
    /// composition** and nothing more.
    Ziatype,
    Cyanotype,
    /// Pigmented gelatin. **Nothing a bath can act on** — see
    /// [`Process::takes_chemistry`].
    Carbon,
    Tintype,
    /// Named for the look, and deliberately. What is out of reach is everything that
    /// makes a daguerreotype an *object*: it flips between positive and negative as
    /// you tilt it, it is a mirror, and it tarnishes in a ring from the edges inward.
    /// None of that is a tone-and-colour model. The panel says so.
    Daguerreotype,
}

impl Process {
    pub const ALL: [Self; 12] = [
        Self::GelatinSilver,
        Self::SaltPrint,
        Self::Albumen,
        Self::CollodionPop,
        Self::Kallitype,
        Self::Vandyke,
        Self::PlatinumPalladium,
        Self::Ziatype,
        Self::Cyanotype,
        Self::Carbon,
        Self::Tintype,
        Self::Daguerreotype,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::GelatinSilver => "Gelatin silver",
            Self::SaltPrint => "Salt print",
            Self::Albumen => "Albumen",
            Self::CollodionPop => "Collodion POP",
            Self::Kallitype => "Kallitype",
            Self::Vandyke => "Vandyke brown",
            Self::PlatinumPalladium => "Platinum / Palladium",
            Self::Ziatype => "Ziatype",
            Self::Cyanotype => "Cyanotype",
            Self::Carbon => "Carbon",
            Self::Tintype => "Tintype",
            Self::Daguerreotype => "Daguerreotype",
        }
    }

    /// Particle fineness, 0 coarse to 1 very fine. Sets both the reaction rate and —
    /// for silver — the untoned hue. See the module note.
    pub fn fineness(self) -> f32 {
        match self {
            // A cold bromide paper. Near-neutral, with the trace of warmth every
            // silver print has.
            Self::GelatinSilver => 0.15,
            Self::SaltPrint => 0.85,
            Self::Albumen => 0.70,
            Self::CollodionPop => 0.60,
            Self::Kallitype => 0.70,
            // The warmest silver in the list, which is what makes it Vandyke.
            Self::Vandyke => 0.95,
            Self::PlatinumPalladium | Self::Ziatype => 0.80,
            Self::Cyanotype => 0.50,
            Self::Carbon => 0.50,
            Self::Tintype => 0.60,
            Self::Daguerreotype => 0.90,
        }
    }

    /// The deepest density this process reaches. Glossy silver holds far more than an
    /// in-fibre matte print, and this is the axis every bath's selectivity is measured
    /// against.
    pub fn dmax(self) -> f32 {
        match self {
            Self::GelatinSilver | Self::CollodionPop | Self::Tintype => 2.10,
            Self::Albumen => 1.90,
            Self::Carbon => 1.90,
            Self::Daguerreotype => 1.80,
            Self::SaltPrint | Self::Kallitype | Self::Vandyke => 1.60,
            Self::PlatinumPalladium | Self::Ziatype => 1.45,
            Self::Cyanotype => 1.55,
        }
    }

    pub fn optics(self) -> Optics {
        match self {
            // **Both of these carry their material in the highlights.** A
            // daguerreotype's scattering texture is the obvious one; a tintype is the
            // same shape arrived at differently — it is a very underexposed negative on
            // japanned iron, so its dense areas read grey and its thin areas are the
            // black lacquer showing through. Filed as absorbing, its dark ground came
            // out in the highlights, which is the picture inside out.
            Self::Daguerreotype | Self::Tintype => Optics::Scattering,
            _ => Optics::Absorbing,
        }
    }

    /// Whether the Chemistry stack does anything at all.
    ///
    /// **False for carbon, and the panel should say why** rather than hiding the
    /// stack: *a carbon print's colour is in the tissue; there is nothing to tone*.
    /// This is the process that proves the module does not assume everything is
    /// tonable, which is much cheaper to discover here than after the stack has been
    /// built assuming otherwise.
    pub fn takes_chemistry(self) -> bool {
        self != Self::Carbon
    }

    /// The paper's own colour, where there is no image substance at all.
    ///
    /// **This is the honest half of "highlight colour"** — the one cause that really
    /// does tint the white of a print. A bath cannot.
    pub fn base(self) -> Tint {
        match self {
            Self::GelatinSilver => Tint::NEUTRAL,
            Self::SaltPrint | Self::Vandyke => Tint {
                hue: 70.0,
                chroma: 0.030,
            },
            Self::Albumen | Self::CollodionPop => Tint {
                hue: 75.0,
                chroma: 0.022,
            },
            Self::Kallitype => Tint {
                hue: 72.0,
                chroma: 0.018,
            },
            Self::PlatinumPalladium | Self::Ziatype => Tint {
                hue: 80.0,
                chroma: 0.012,
            },
            Self::Cyanotype | Self::Carbon => Tint::NEUTRAL,
            // The tintype's black is the japanned iron, not the image. Its base is
            // dark warm brown rather than any kind of white, which is the constraint
            // that stopped the paper object assuming paper is pale.
            Self::Tintype => Tint {
                hue: 45.0,
                chroma: 0.040,
            },
            Self::Daguerreotype => Tint {
                hue: 250.0,
                chroma: 0.012,
            },
        }
    }

    /// The image where the substance is **dense**, and where it is **thin**.
    ///
    /// # One tone was not enough, and the reason is the whole of "short in scale"
    ///
    /// The previous model gave each process a single tint, scaled by how much substance
    /// was there. That makes every tone the same hue at different strengths — so the
    /// only way to get more out of a process was more chroma, which the maintainer did not want
    /// because "the hue can already make things a little wonky".
    ///
    /// Real processes are not one hue. An albumen print has **deep brown-maroon
    /// shadows and pale yolk highlights**, and the distance between those two is most of
    /// what makes it look like an albumen print.
    ///
    /// # Dense and thin, not shadow and highlight
    ///
    /// These were called `shadow_tone` and `highlight_tone`, which is true of a print
    /// and false of a plate: a daguerreotype and a tintype carry their material in the
    /// *highlights*, so the "shadow" tone was the one being used at the bright end. The
    /// variable is the amount of substance, and naming it that way is the only version
    /// that is right for both.
    ///
    /// # The thin end is quiet on purpose
    ///
    /// These chromas are much lower than the first pass gave them, and that was the
    /// other half of the maintainer's "way too light/bright". A print keeps its colour in the
    /// mid and low tones and lets the *paper* carry the highlights; tinting the thin end
    /// as hard as the dense one makes every process read as a flat wash over the whole
    /// picture rather than as an image with a colour.
    pub fn dense_tone(self) -> Tint {
        match self {
            Self::GelatinSilver => Tint {
                hue: 55.0,
                chroma: 0.014,
            },
            Self::SaltPrint => Tint {
                hue: 32.0,
                chroma: 0.050,
            },
            // Albumen's shadow is purple-brown rather than red-brown — the sulfur
            // adsorbed to colloidal silver that Nishimura describes is what puts it
            // there, and it is the note most often missed in a digital imitation.
            Self::Albumen => Tint {
                hue: 6.0,
                chroma: 0.045,
            },
            // Wet-plate POP goes plum before it is toned.
            Self::CollodionPop => Tint {
                hue: 354.0,
                chroma: 0.045,
            },
            Self::Kallitype => Tint {
                hue: 46.0,
                chroma: 0.040,
            },
            // The warmest, reddest silver in the list, which is what makes it Vandyke.
            Self::Vandyke => Tint {
                hue: 24.0,
                chroma: 0.060,
            },
            // Platinum blacks are famously neutral and deep.
            Self::PlatinumPalladium => Tint {
                hue: 70.0,
                chroma: 0.010,
            },
            Self::Ziatype => Tint {
                hue: 50.0,
                chroma: 0.030,
            },
            // **Prussian blue, and deep.** `#003153` is about `a* +3, b* -25` in
            // CIELAB, and `the_hue_sweep` says that is OKLCh **245** — 258 was chosen by
            // eye and lands at `a* +19`, which is a violet. The sweep exists so the next
            // one of these is measured rather than guessed at.
            Self::Cyanotype => Tint {
                hue: 245.0,
                chroma: 0.170,
            },
            // Carbon black is a pigment, and a pigment can be neutral.
            Self::Carbon => Tint {
                hue: 40.0,
                chroma: 0.012,
            },
            // A tintype's dense areas are the grey image, not its blacks — see
            // `Process::optics`.
            Self::Tintype => Tint {
                hue: 45.0,
                chroma: 0.032,
            },
            Self::Daguerreotype => Tint {
                hue: 250.0,
                chroma: 0.020,
            },
        }
    }

    /// See [`dense_tone`](Self::dense_tone).
    pub fn thin_tone(self) -> Tint {
        match self {
            Self::GelatinSilver => Tint {
                hue: 62.0,
                chroma: 0.005,
            },
            Self::SaltPrint => Tint {
                hue: 62.0,
                chroma: 0.020,
            },
            Self::Albumen => Tint {
                hue: 88.0,
                chroma: 0.020,
            },
            Self::CollodionPop => Tint {
                hue: 46.0,
                chroma: 0.018,
            },
            Self::Kallitype => Tint {
                hue: 66.0,
                chroma: 0.016,
            },
            Self::Vandyke => Tint {
                hue: 52.0,
                chroma: 0.024,
            },
            // A platinum highlight is the paper and nothing else.
            Self::PlatinumPalladium => Tint {
                hue: 78.0,
                chroma: 0.008,
            },
            Self::Ziatype => Tint {
                hue: 72.0,
                chroma: 0.014,
            },
            // **The number that was making cyanotypes look like a wash.** It was 0.110,
            // which put a strong blue in every highlight; a real cyanotype's thin end is
            // very nearly the paper with a trace of stain in it.
            Self::Cyanotype => Tint {
                hue: 243.0,
                chroma: 0.030,
            },
            Self::Carbon => Tint {
                hue: 40.0,
                chroma: 0.006,
            },
            Self::Tintype => Tint {
                hue: 50.0,
                chroma: 0.020,
            },
            Self::Daguerreotype => Tint {
                hue: 244.0,
                chroma: 0.012,
            },
        }
    }

    /// The **paper**, where there is no image substance at all.
    ///
    /// The honest half of "highlight colour": a stained or cream base is the one cause
    /// that really does tint the white of a print, and no bath can. It does not scale
    /// with substance — a bare margin is the paper's own colour — but it *fades* under
    /// density, because silver hides the sheet it is sitting on.
    pub fn paper(self) -> Tint {
        match self {
            Self::GelatinSilver | Self::Cyanotype => Tint::NEUTRAL,
            Self::SaltPrint | Self::Vandyke => Tint {
                hue: 78.0,
                chroma: 0.018,
            },
            // Albumen's cream, which is half of why the highlights read as yolk.
            Self::Albumen => Tint {
                hue: 84.0,
                chroma: 0.020,
            },
            Self::CollodionPop => Tint {
                hue: 74.0,
                chroma: 0.016,
            },
            Self::Kallitype => Tint {
                hue: 76.0,
                chroma: 0.012,
            },
            Self::PlatinumPalladium | Self::Ziatype => Tint {
                hue: 82.0,
                chroma: 0.008,
            },
            Self::Carbon => Tint::NEUTRAL,
            // A tintype's ground is japanned iron, not paper: dark and warm.
            Self::Tintype => Tint {
                hue: 42.0,
                chroma: 0.038,
            },
            Self::Daguerreotype => Tint {
                hue: 250.0,
                chroma: 0.010,
            },
        }
    }

    /// The process's own **grade**: how hard its characteristic curve is.
    ///
    /// Above 1.0 pushes the shadows down and the highlights up about the midpoint,
    /// which is what a contrastier paper does; below 1.0 is the long, low-contrast scale
    /// a platinum print is bought for. Exactly 1.0 for gelatin silver, so the default
    /// process leaves the tonal scale exactly where the tone map put it.
    ///
    /// **the maintainer: "I'm surprised none of the processes increases contrast."** They did
    /// not, and that was the other half of the scale feeling short — a process could
    /// change the colour of a tone but never where the tone sat.
    pub fn grade(self) -> f32 {
        match self {
            Self::GelatinSilver => 1.00,
            // POP processes are contrasty and self-masking; albumen most of all.
            Self::Albumen => 1.30,
            Self::SaltPrint => 1.12,
            Self::CollodionPop => 1.22,
            Self::Kallitype => 1.08,
            Self::Vandyke => 1.10,
            // The long straight line these are printed for.
            Self::PlatinumPalladium => 0.82,
            Self::Ziatype => 0.88,
            Self::Cyanotype => 1.24,
            // A carbon print's scale is long, and its blacks are as deep as the tissue.
            Self::Carbon => 0.92,
            Self::Tintype => 1.15,
            Self::Daguerreotype => 1.20,
        }
    }

    /// The treatments this process admits, in the order a darkroom would reach for them.    /// The treatments this process admits, in the order a darkroom would reach for them.
    ///
    /// **Per process, and that is the point.** The first design let any bath be stacked
    /// on any process, which put gold toning on a carbon print in the menu. A process
    /// owns its chemistry, so what is offered is what is possible — and the ordering
    /// question disappears with it, because the chemistry knows its own sequence and the
    /// user sets amounts rather than arranging a stack.
    pub fn treatments(self) -> &'static [Treatment] {
        match self {
            Self::GelatinSilver | Self::Tintype => SILVER_DOP,
            Self::SaltPrint | Self::Albumen | Self::CollodionPop => SILVER_POP,
            Self::Kallitype | Self::Vandyke => IRON_SILVER,
            Self::PlatinumPalladium | Self::Ziatype => NOBLE,
            Self::Cyanotype => IRON_BLUE,
            Self::Daguerreotype => PLATE,
            Self::Carbon => &[],
        }
    }

    /// The name of this process's one blend control, if it has one.
    ///
    /// A process parameter rather than a treatment: it is decided in the sensitizer or
    /// the tissue, before there is an image to treat.
    pub fn mix_label(self) -> Option<&'static str> {
        match self {
            Self::PlatinumPalladium => Some("Platinum → Palladium"),
            Self::Ziatype => Some("Gold in sensitizer"),
            Self::Carbon => Some("Pigment hue"),
            _ => None,
        }
    }
}

/// One treatment a process admits.
///
/// A static description, not state: the amount lives in [`ToningParams::applied`], and
/// the two are matched by [`key`](Treatment::key) so a sidecar survives the list being
/// reordered or added to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Treatment {
    /// Stable across releases, because it is what the sidecar writes.
    pub key: &'static str,
    pub label: &'static str,
    /// What it says on hover. The formula, where there is a named one.
    pub note: &'static str,
    /// What it makes, at full conversion.
    pub tint: Tint,
    /// Relative extinction of what it makes — above 1.0 deepens the print.
    pub kappa: f32,
    /// Where on the tonal scale it works. Positive runs faster where there is more
    /// substance; negative runs faster where there is least, which is what a bleach
    /// does.
    pub selectivity: f32,
    /// How far it gets at full strength.
    pub rate: f32,
    /// **What this makes instead, when a named treatment has already run.**
    ///
    /// One rule, and it exists for one fact: gold on plain silver is blue-black and
    /// gold on a sulphided print is red. That was the whole argument for an ordered
    /// stack, and it survives the stack being taken away — the chemistry knows its own
    /// sequence, so the user sets amounts and this decides what they mean.
    pub after: Option<(&'static str, Tint)>,
}

/// Silver, developed out — a gelatin silver print or a tintype.
///
/// **Selenium's shadow bias and gold's lack of one are documented rather than judged.**
/// Nishimura (Image Permanence Institute) on the IPI microfilm work: selenium "worked
/// pretty well for high density areas (shadows), but failed in the highlights and
/// mid-tones… it apparently just doesn't convert the mid-tones and highlights all that
/// well." And of GP-1: it "lays down a pretty even amount of gold all over."
///
/// That second sentence corrected a number set by eye — gold was at 0.6, biased to the
/// shadows like selenium, and it should be flat.
///
/// **The order is the darkroom's sequence, and it is load-bearing.** A conversion toner
/// runs before a noble metal and an additive bath runs last, because that is what the
/// chemistry does — and because `after` is resolved by walking this list, so gold listed
/// before sepia would never see that the print had been sulphided. The first version
/// ordered it by prominence and `gold_knows_what_came_before_it` failed.
static SILVER_DOP: &[Treatment] = &[
    Treatment {
        key: "sepia",
        label: "Sepia (bleach)",
        note: "Bleach to halide, redevelop in thiourea. The bleach is the control: pull the \
            print early and the highlights bleach while the shadows are still silver — the \
            classic warm highlights against neutral blacks.",
        tint: Tint {
            hue: 65.0,
            chroma: 0.085,
        },
        kappa: 0.88,
        // The bleach attacks the finest particles first, so the highlights go first.
        selectivity: -0.7,
        rate: 4.0,
        after: None,
    },
    Treatment {
        key: "sulphide",
        label: "Sulfide",
        note: "Thiourea without the bleach. Slower, subtler and it keeps the blacks — a sulfur \
            dusting rather than a full conversion.",
        tint: Tint {
            hue: 58.0,
            chroma: 0.045,
        },
        kappa: 0.96,
        selectivity: 0.2,
        rate: 1.6,
        after: None,
    },
    Treatment {
        key: "selenium",
        label: "Selenium",
        note: "Selenosulfate. Purple-brown, and it deepens the black — the reason most people \
            tone at all.\n\nIt converts the shadows and barely touches the midtones and \
            highlights, which is measured rather than a style choice.",
        tint: Tint {
            hue: 330.0,
            chroma: 0.055,
        },
        kappa: 1.10,
        selectivity: 0.7,
        rate: 3.0,
        after: None,
    },
    Treatment {
        key: "gold-gp1",
        label: "Gold (GP-1)",
        note: "Gold chloride and thiocyanate. Blue-black, and it lays down evenly across the \
            whole scale rather than favoring the shadows.",
        tint: Tint {
            hue: 250.0,
            chroma: 0.045,
        },
        kappa: 1.05,
        // Even, per Henn and Mack via Nishimura. Not a taste decision.
        selectivity: 0.0,
        rate: 2.5,
        // The design's showpiece, kept without the stack: gold over a sulphided print
        // is the red the darkroom calls chalk, and gold over plain silver is blue-black.
        after: Some((
            "sepia",
            Tint {
                hue: 30.0,
                chroma: 0.100,
            },
        )),
    },
    Treatment {
        key: "gold-gp2",
        label: "Gold (GP-2)",
        note: "Gold chloride and thiourea. The thiourea is an active sulfur compound, so it \
            sulfides as it golds — warmer and less blue than GP-1, and the formula that \
            actually protects a print.",
        tint: Tint {
            hue: 300.0,
            chroma: 0.050,
        },
        kappa: 1.03,
        selectivity: 0.15,
        rate: 2.6,
        after: Some((
            "sepia",
            Tint {
                hue: 25.0,
                chroma: 0.110,
            },
        )),
    },
    Treatment {
        key: "iron-blue",
        label: "Iron blue",
        note: "Prussian blue, deposited in proportion to the silver rather than converting it. \
            Adds density and raises contrast.",
        tint: Tint {
            hue: 245.0,
            chroma: 0.150,
        },
        kappa: 1.15,
        selectivity: 0.4,
        rate: 1.2,
        after: None,
    },
    Treatment {
        key: "copper",
        label: "Copper",
        note: "Copper ferrocyanide. Red-brown, and additive like the blue.",
        tint: Tint {
            hue: 40.0,
            chroma: 0.090,
        },
        kappa: 1.05,
        selectivity: 0.4,
        rate: 1.0,
        after: None,
    },
];

/// Silver, printed out — salt, albumen, collodion. These were gold-toned as a matter of
/// course, which is why gold leads the list rather than sitting in it.
static SILVER_POP: &[Treatment] = &[
    Treatment {
        key: "gold-gp1",
        label: "Gold (GP-1)",
        note: "The standard toner for a printing-out paper, and what stops a salt or albumen \
            print being the red of raw colloidal silver.",
        tint: Tint {
            hue: 285.0,
            chroma: 0.060,
        },
        kappa: 1.05,
        selectivity: 0.0,
        rate: 3.0,
        after: None,
    },
    Treatment {
        key: "platinum",
        label: "Platinum",
        note: "A platinum salt over printed-out silver. Cools and flattens; the nineteenth \
            century's answer to a print that was too red.",
        tint: Tint {
            hue: 75.0,
            chroma: 0.025,
        },
        kappa: 0.92,
        selectivity: 0.1,
        rate: 2.2,
        after: None,
    },
    Treatment {
        key: "selenium",
        label: "Selenium",
        note: "Shadows first, and it deepens them. Less used here than on a developed-out \
            print, and it works the same way.",
        tint: Tint {
            hue: 330.0,
            chroma: 0.050,
        },
        kappa: 1.08,
        selectivity: 0.7,
        rate: 2.4,
        after: None,
    },
];

/// Kallitype and Vandyke: iron-silver, and **defined by being toned**. An untoned
/// kallitype is famously unstable, so the noble-metal toners here are part of the
/// process rather than an option on top of it.
static IRON_SILVER: &[Treatment] = &[
    Treatment {
        key: "gold-gp1",
        label: "Gold",
        note: "Cools a kallitype to a purple-black, and is what makes one keep.",
        tint: Tint {
            hue: 290.0,
            chroma: 0.055,
        },
        kappa: 1.04,
        selectivity: 0.0,
        rate: 3.0,
        after: None,
    },
    Treatment {
        key: "palladium",
        label: "Palladium",
        note: "The cheap route to the platinum look: a palladium-toned kallitype is hard to \
            tell from a palladium print.",
        tint: Tint {
            hue: 60.0,
            chroma: 0.038,
        },
        kappa: 0.95,
        selectivity: 0.1,
        rate: 2.6,
        after: None,
    },
    Treatment {
        key: "platinum",
        label: "Platinum",
        note: "Colder than palladium, and the more expensive half of the same idea.",
        tint: Tint {
            hue: 78.0,
            chroma: 0.022,
        },
        kappa: 0.93,
        selectivity: 0.1,
        rate: 2.4,
        after: None,
    },
];

/// Platinum, palladium and the ziatype. Their colour is set in the sensitizer, so there
/// is little left to do afterwards — gold is the one bath that reaches them.
static NOBLE: &[Treatment] = &[Treatment {
    key: "gold-gp1",
    label: "Gold",
    note: "Rare on a platinum print and real. Cools it further, and adds a little density \
        to a process with none to spare.",
    tint: Tint {
        hue: 265.0,
        chroma: 0.035,
    },
    kappa: 1.04,
    selectivity: 0.0,
    rate: 2.0,
    after: None,
}];

/// A cyanotype is iron, and no silver toner touches it. What does is a tannin — tea,
/// coffee, or tannic acid — after the blue has been opened up with a carbonate.
static IRON_BLUE: &[Treatment] = &[
    Treatment {
        key: "carbonate",
        label: "Carbonate bleach",
        note: "Opens the Prussian blue so a tannin can reach it. On its own it just lifts the \
            print — the pair is the point.",
        tint: Tint {
            hue: 245.0,
            chroma: 0.030,
        },
        kappa: 0.70,
        selectivity: -0.4,
        rate: 3.0,
        after: None,
    },
    Treatment {
        key: "tannin",
        label: "Tannin",
        note: "Tea, coffee or tannic acid over a bleached cyanotype. Brown through to a violet- \
            black, well to the red of a sepia.",
        tint: Tint {
            hue: 25.0,
            chroma: 0.080,
        },
        kappa: 1.00,
        selectivity: 0.2,
        rate: 2.5,
        after: None,
    },
];

/// A daguerreotype's one treatment, and it is the reason plates are warm at all.
static PLATE: &[Treatment] = &[Treatment {
    key: "gilding",
    label: "Gilding",
    note: "Gold chloride pooled on the heated plate — Fizeau, 1840, and the whole reason a \
        daguerreotype is not steel gray.\n\nOn a plate the material makes the highlights, \
        so it lands where a print's gold would not.",
    tint: Tint {
        hue: 60.0,
        chroma: 0.045,
    },
    kappa: 1.06,
    selectivity: 0.3,
    rate: 2.8,
    after: None,
}];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Applied {
    pub key: &'static str,
    /// Time in the bath, 0–1. For a bleach it is how far the bleach was taken.
    pub amount: f32,
}

/// The Toning Process module's state.
#[derive(Debug, Clone, PartialEq)]
pub struct ToningParams {
    /// Module bypass. **Off by default**: every frame judged before this module
    /// existed must still render the same way.
    pub enabled: bool,
    pub process: Process,
    /// How strongly the process's own image tone shows, against its nominal 1.0.
    ///
    /// **The control the first design did not have.** Every process came out the same
    /// warm hue with nothing to move unless a treatment was added, which is not how a
    /// paper works — the same process on two papers is two colours.
    pub tone: f32,
    /// Degrees off the process's own hue. A trim, not a colour picker: the process
    /// decides what it is and this says how yours came out.
    pub hue: f32,
    /// The process's one blend control, 0–1. Its meaning is per process — see
    /// [`Process::mix_label`] — and it is `None` for the processes that have none.
    pub mix: f32,
    /// The treatments in use, in the order the process lists them.
    pub applied: Vec<Applied>,
    /// Whether the selected chemical treatments participate in the result.
    /// The authored amounts remain in place while this submodule is bypassed.
    pub chemistry_enabled: bool,
    /// **Where the toning lands across the tonal scale**, as strength against L\*.
    ///
    /// Flat at 1.0 is even. Pulled down at the right, the highlights keep the paper and
    /// only the shadows tone; pulled down at the left, the reverse. It replaces the
    /// per-bath range trapezoid the first design gave every bath, which was four
    /// controls per treatment for a thing most prints want once.
    pub placement: Curve,
    /// Whether the placement curve shapes the process and chemistry.
    /// Off reads exactly like the flat, nominal placement without discarding the curve.
    pub placement_enabled: bool,
    /// A carbon tissue's pigment strength. Meaningful for [`Process::Carbon`] only.
    pub pigment: f32,
    /// The print's grade, against the process's own — see [`Process::grade`].
    ///
    /// A multiplier rather than an absolute, so a process's own character is the datum
    /// and this says how hard you printed it. 1.0 is the process as it usually comes
    /// out, which is why the default leaves every table value exactly as written.
    pub contrast: f32,
}

impl Default for ToningParams {
    fn default() -> Self {
        Self {
            enabled: false,
            process: Process::GelatinSilver,
            tone: 1.0,
            hue: 0.0,
            mix: 0.5,
            applied: Vec::new(),
            chemistry_enabled: true,
            placement: flat_placement(),
            placement_enabled: true,
            pigment: 0.5,
            contrast: 1.0,
        }
    }
}

/// Entries in the baked table.
///
/// **One constant, in the crate both consumers depend on.** It lived twice — once in
/// `raw-gpu` for the shader's buffer and once in `raw-app` for the export tail — with
/// nothing holding the two equal. They agreed, and a table sampled at two different
/// resolutions is a preview and an export that disagree in the shadows, which is the
/// one failure this module's whole design is arranged to prevent. It is not the sort of
/// thing to leave to two people noticing.
///
/// Indexed by L\*, which is perceptually uniform, so 512 steps is well under half an
/// 8-bit code everywhere on the scale — including the shadows, where a table on
/// luminance would have almost no entries and where toning does most of its work. Three
/// floats each, so the upload is 6 KB.
pub const LUT_ENTRIES: usize = 512;

/// Even across the whole scale, which is what a bath does when nobody has shaped it.
///
/// **The datum is a half, not a one**, so that the editor draws it through the *middle*
/// of the graph and there is room above it as well as below. Drawn at the top — which
/// is what a `1.0` datum gives — the control could only ever take toning away, and a
/// line pinned to the ceiling does not read as a datum at all. the maintainer reported both.
///
/// `Curve`'s own default is the identity diagonal, which as a placement would mean
/// "tone the highlights and leave the blacks" — a strange thing to open a module on.
pub fn flat_placement() -> Curve {
    Curve::from_points(&[[0.0, 0.5], [1.0, 0.5]]).expect("two points is a curve")
}

/// The placement curve read as a **strength**: `0` to `2`, with the flat datum at `1`.
///
/// The curve stores `0..1` because that is what `Curve` is; the module wants a
/// multiplier that can go either side of nominal. One doubling, in one place, so the
/// editor and the model cannot disagree about what the middle of the graph means.
pub fn placement_strength(c: &Curve, lstar: f32) -> f32 {
    (c.eval(lstar) * 2.0).clamp(0.0, 2.0)
}

/// What one level came out as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Toned {
    /// Print luminance after the density change, 0–1. **Not the input**: a toner
    /// moves the tonal scale, which is the thing a gradient map cannot do.
    pub y: f32,
    /// OKLab chroma.
    pub chroma: f32,
    /// OKLCh hue, degrees.
    pub hue: f32,
}

impl Toned {
    /// OKLab's `(a, b)` for this level.
    ///
    /// **What the LUT stores, rather than chroma and hue.** Two adjacent table entries
    /// either side of 0°/360° interpolate the long way round the wheel in polar form —
    /// a hue that should cross from 355° to 5° instead sweeps through the whole
    /// spectrum. In Cartesian `(a, b)` the interpolation is simply correct, and the
    /// table is read a great many more times than it is built.
    pub fn ab(self) -> (f32, f32) {
        Tint {
            hue: self.hue,
            chroma: self.chroma,
        }
        .ab()
    }
}

impl ToningParams {
    /// The amount of one treatment, 0 when it is not in use.
    pub fn amount(&self, key: &str) -> f32 {
        self.applied
            .iter()
            .find(|a| a.key == key)
            .map_or(0.0, |a| a.amount)
    }

    /// Add a treatment, or return the index of the one already there.
    pub fn apply(&mut self, key: &'static str, amount: f32) -> usize {
        match self.applied.iter().position(|a| a.key == key) {
            Some(i) => i,
            None => {
                self.applied.push(Applied { key, amount });
                self.applied.len() - 1
            }
        }
    }

    /// Drop everything the current process does not offer.
    ///
    /// **Called when the process changes**, because a treatment carried across would be
    /// one the panel cannot show and the chemistry cannot run — a value with no control,
    /// which is the definition of a setting nobody can find.
    pub fn reconcile(&mut self) {
        let offered = self.process.treatments();
        self.applied
            .retain(|a| offered.iter().any(|t| t.key == a.key));
    }

    /// The LUT as flat `[y, a, b]` triples, ready to upload.
    ///
    /// **This is what makes preview and export the same picture.** Both paths sample
    /// this one table at the same point in the signal — after the tone map, before the
    /// transfer function — rather than each carrying an implementation of the model.
    pub fn bake_flat(&self, n: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(n.max(2) * 3);
        for t in self.bake(n) {
            let (a, b) = t.ab();
            out.extend_from_slice(&[t.y, a, b]);
        }
        out
    }

    /// Whether this changes any rendered pixel.
    ///
    /// **Not the same as "was touched".** A treatment sitting at zero is in the list
    /// and does nothing, and reading the list's emptiness as activity made an untoned
    /// export take the three-channel path and promote its container — caught by
    /// `a_bath_at_zero_strength_changes_no_byte`, which is exactly the test that exists
    /// to notice it.
    pub fn is_active(&self) -> bool {
        let d = Self::default();
        self.enabled
            && (self.process != d.process
                || self.tone != d.tone
                || self.hue != d.hue
                || self.contrast != d.contrast
                || (self.process.mix_label().is_some() && self.mix != d.mix)
                || (self.process == Process::Carbon && self.pigment != d.pigment)
                || (self.placement_enabled && self.placement != d.placement)
                || (self.chemistry_enabled && self.applied.iter().any(|a| a.amount > 1e-6)))
    }

    /// Whether the user has touched it, ignoring the bypass.
    pub fn is_default(&self) -> bool {
        let d = Self::default();
        Self {
            enabled: d.enabled,
            ..self.clone()
        } == d
    }

    /// Whether the module's dot should read as modified.
    pub fn is_modified(&self) -> bool {
        crate::params::is_modified(
            self.is_default(),
            self.is_active(),
            Self::default().is_active(),
        )
    }

    /// Tone one level. `y` is print luminance, 0–1, where 1 is paper white.
    pub fn evaluate(&self, y: f32) -> Toned {
        let y = y.clamp(0.0, 1.0);
        let dmax = self.process.dmax();

        // Density, and the substance that carries it. Linear in density rather than in
        // lightness, which is the whole reason this converts rather than working on the
        // value it was handed — see the module note.
        let floor = 10f32.powf(-dmax);
        let d = -y.max(floor).log10();
        let ratio = (d / dmax).clamp(0.0, 1.0);
        let s = match self.process.optics() {
            Optics::Absorbing => ratio,
            Optics::Scattering => 1.0 - ratio,
        };

        // L\* is the axis the placement curve is drawn on, because it is what the panel
        // shows. The model works in density; the conversion happens here.
        let lstar = lstar_encode(y);
        let placed = if self.placement_enabled {
            placement_strength(&self.placement, lstar)
        } else {
            1.0
        };

        // The image substance's own colour, read between the two ends by density. One
        // tone at two strengths is what made the scale feel short; see
        // `Process::shadow_tone`.
        let ends = |t: Tint| {
            let mut t = t;
            if self.process.mix_label().is_some() {
                // `mix` moves the noble metals between their ends and a carbon tissue
                // between its pigments. Elsewhere it has no meaning and no control.
                t.hue += (self.mix - 0.5) * 60.0;
            }
            if self.process == Process::Carbon {
                t.chroma *= self.pigment * 2.0;
            }
            t.hue = (t.hue + self.hue).rem_euclid(360.0);
            t.chroma *= self.tone.max(0.0);
            t
        };
        let (sa, sb) = ends(self.process.dense_tone()).ab();
        let (ha, hb) = ends(self.process.thin_tone()).ab();
        let base = Tint::from_ab(ha + (sa - ha) * s, hb + (sb - hb) * s);

        // What is left untreated, and what the treatments made of the rest.
        let mut untreated = 1.0f32;
        let mut kappa = 0.0f32;
        let (mut a, mut b) = (0.0f32, 0.0f32);
        let mut ran: Vec<&str> = Vec::new();

        if self.chemistry_enabled {
            for t in self.process.treatments() {
                let amount = self.amount(t.key);
                if amount <= 1e-6 {
                    continue;
                }
                let f = (fraction(t, amount, s, self.process.fineness()) * placed).clamp(0.0, 1.0);
                let taken = untreated * f;
                if taken <= 0.0 {
                    ran.push(t.key);
                    continue;
                }
                // Gold over a sulphided print is red; gold over plain silver is blue-black.
                // The chemistry knows its own sequence, so this needs no stack to arrange.
                let tint = match t.after {
                    Some((prior, alt)) if ran.contains(&prior) => alt,
                    _ => t.tint,
                };
                let (ta, tb) = tint.ab();
                a += ta * taken;
                b += tb * taken;
                kappa += t.kappa * taken;
                untreated -= taken;
                ran.push(t.key);
            }
        }

        let (ba, bb) = base.ab();
        // Placement belongs to the process, not only to its optional baths. Without
        // this multiplier a print with no Chemistry treatment had nothing for the
        // curve to shape, so the control appeared broken.
        a += ba * untreated * placed;
        b += bb * untreated * placed;
        kappa += untreated;

        // Chroma follows the amount of substance, so the bare substrate keeps its own
        // colour whatever the chemistry says. This is the invariant, held by
        // construction rather than by a clamp.
        a *= s;
        b *= s;

        // The paper, which does not scale with substance — a bare margin is the sheet's
        // own colour — but fades under density, because silver hides what it sits on.
        let (pa, pb) = self.process.paper().ab();
        a += pa * (1.0 - s);
        b += pb * (1.0 - s);

        let tint = Tint::from_ab(a, b);
        // **The density *change*, not the density.** Writing `10^(-d * kappa)` is the
        // tempting form and it is wrong here: it re-imposes the process's Dmax on the
        // output, so switching the module on with nothing applied would lift the deepest
        // black by about 7 L\*. Dmax is this module's reference axis, not a ceiling it
        // may impose; imposing one is an output intent, and that module does not exist.
        //
        // **The grade goes here too**, as a pivot about mid-density — which is what a
        // paper grade does: push the shadows down and the highlights up around a fixed
        // middle. A process could change the colour of a tone but never where the tone
        // sat, which was half of why the scale felt short.
        let g = (self.process.grade() * self.contrast.max(0.0)).clamp(0.2, 3.0);
        let graded = if (g - 1.0).abs() < 1e-6 {
            d
        } else {
            // **Both ends are fixed and the pivot is the middle**, which a straight
            // linear pivot is not: `0.5 + (ratio - 0.5) * g` sends `ratio = 0` to
            // `0.5 - 0.5g`, so any process softer than neutral gave *paper white a
            // density*. Platinum, ziatype and carbon all came out at L\* 89 to 93
            // instead of 100 — a white that is not white, from a control that has no
            // business touching it.
            //
            // A power on each half about the midpoint fixes 0, 0.5 and 1 by
            // construction. A higher exponent moves low density toward paper white and
            // high density toward maximum black: increasing the control is therefore
            // harder, and decreasing it is softer.
            let e = g;
            let shaped = if ratio <= 0.5 {
                0.5 * (2.0 * ratio).powf(e)
            } else {
                1.0 - 0.5 * (2.0 * (1.0 - ratio)).powf(e)
            };
            dmax * shaped.clamp(0.0, 1.0)
        };
        let y_out = y * 10f32.powf(-(graded * kappa.max(0.0) - d));
        Toned {
            y: y_out.clamp(0.0, 1.0),
            chroma: tint.chroma,
            hue: tint.hue,
        }
    }

    /// Collapse the whole model to a table indexed by **L\***, normalised 0–1.
    ///
    /// Indexed by L\* rather than by luminance because L\* is perceptually uniform: a
    /// table on luminance would spend most of its entries on the highlights and almost
    /// none on the shadows, which is where toning does its work.
    pub fn bake(&self, n: usize) -> Vec<Toned> {
        let n = n.max(2);
        (0..n)
            .map(|i| {
                let lstar = i as f32 / (n - 1) as f32;
                self.evaluate(y_from_lstar(lstar))
            })
            .collect()
    }
}

/// How far a bath gets, at one substance level.
///
/// ```text
/// f = 1 - exp(-k * t * phi * selectivity(s))
/// ```
///
/// The selectivity term is `s^sigma` for positive sigma and `(1-s)^|sigma|` for
/// negative, which keeps it finite at both ends — a plain `s^sigma` with negative
/// sigma runs to infinity at paper white.
fn fraction(t: &Treatment, amount: f32, s: f32, fineness: f32) -> f32 {
    let sigma = t.selectivity;
    let sel = if sigma >= 0.0 {
        s.clamp(0.0, 1.0).powf(sigma)
    } else {
        (1.0 - s).clamp(0.0, 1.0).powf(-sigma)
    };
    // Fineness is more surface per unit substance, so a fine emulsion converts
    // further in the same time. Never zero: a coarse paper still tones.
    let phi = 0.5 + fineness.clamp(0.0, 1.0);
    let x = t.rate * amount.clamp(0.0, 1.0) * phi * sel;
    1.0 - (-x).exp()
}

/// Print luminance from normalised L\*. The inverse of
/// [`lstar_encode`](crate::display::lstar_encode).
pub fn y_from_lstar(lstar_norm: f32) -> f32 {
    const E: f32 = 0.008_856;
    const K: f32 = 903.3;
    let l = lstar_norm.clamp(0.0, 1.0) * 100.0;
    let y = ((l + 16.0) / 116.0).powi(3);
    if y > E { y } else { l / K }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silver(applied: Vec<Applied>) -> ToningParams {
        ToningParams {
            enabled: true,
            applied,
            ..Default::default()
        }
    }

    fn at(key: &'static str, amount: f32) -> Applied {
        Applied { key, amount }
    }

    /// **The invariant the duotone model violates**, and the sharpest statement of why
    /// this module is not that model. There is no image substance in the bare
    /// substrate, and a toner reacts with substance — so nothing a print is dipped in
    /// can reach it.
    ///
    /// **Stated as a comparison, not as a zero**, which is the correction a stained
    /// paper forced. A salt print's sheet is cream and an albumen's is more so; their
    /// bare substrate is *not* neutral and should not be. What must hold is that a
    /// treatment cannot move it — so this runs each process with and without the bath
    /// and asserts the two agree exactly.
    ///
    /// **The bare substrate, not paper white**, for the other end of the same care: a
    /// daguerreotype and a tintype carry their material in the *highlights*, so their
    /// untouched end is the black.
    #[test]
    fn the_bare_substrate_is_untouched_by_any_treatment() {
        for p in Process::ALL {
            let bare = match p.optics() {
                Optics::Absorbing => 1.0,
                Optics::Scattering => 0.0,
            };
            let plain = ToningParams {
                enabled: true,
                process: p,
                ..Default::default()
            }
            .evaluate(bare);
            for t in p.treatments() {
                for amount in [0.25, 0.6, 1.0] {
                    let out = ToningParams {
                        enabled: true,
                        process: p,
                        applied: vec![at(t.key, amount)],
                        ..Default::default()
                    }
                    .evaluate(bare);
                    assert_eq!(
                        out, plain,
                        "{:?}/{} at {amount} moved the bare substrate",
                        p, t.key
                    );
                }
            }
        }
    }

    /// Zero is not approximately nothing, it is nothing.
    #[test]
    fn a_treatment_at_zero_is_identity() {
        let p = silver(vec![at("selenium", 0.0)]);
        let plain = ToningParams {
            enabled: true,
            ..Default::default()
        };
        for i in 0..=100 {
            let y = y_from_lstar(i as f32 / 100.0);
            assert_eq!(p.evaluate(y), plain.evaluate(y), "at L*={i}");
        }
    }

    /// **The claim a gradient map cannot make.** Selenium deepens the blacks and sepia
    /// lifts them, because the treatments carry different extinction — so toning
    /// reshapes the tonal scale rather than only recolouring it.
    #[test]
    fn toning_moves_the_tonal_scale() {
        let shadow = y_from_lstar(0.2);
        let plain = ToningParams {
            enabled: true,
            ..Default::default()
        }
        .evaluate(shadow)
        .y;
        let selenium = silver(vec![at("selenium", 1.0)]).evaluate(shadow).y;
        let sepia = silver(vec![at("sepia", 1.0)]).evaluate(shadow).y;

        assert!(
            selenium < plain,
            "selenium should deepen: {selenium} vs {plain}"
        );
        assert!(sepia > plain, "sepia should flatten: {sepia} vs {plain}");
    }

    /// **The result that shows the model is doing work.** Selenium converts the shadows
    /// and barely reaches the midtones and highlights — which is measured rather than
    /// judged: Nishimura, on the IPI microfilm study, "it apparently just doesn't
    /// convert the mid-tones and highlights all that well."
    #[test]
    fn selenium_works_the_shadows_and_leaves_the_highlights() {
        let p = silver(vec![at("selenium", 0.8)]);
        let shadow = p.evaluate(y_from_lstar(0.2)).chroma;
        let highlight = p.evaluate(y_from_lstar(0.85)).chroma;
        assert!(
            shadow > highlight * 2.0,
            "selenium spread evenly: shadow {shadow}, highlight {highlight}"
        );
    }

    /// And gold does **not**: GP-1 "lays down a pretty even amount of gold all over".
    /// The number that corrected — it was biased to the shadows like selenium, by eye,
    /// and the source says flat.
    #[test]
    fn gold_lays_down_evenly() {
        let s = SILVER_DOP.iter().find(|t| t.key == "gold-gp1").unwrap();
        assert_eq!(s.selectivity, 0.0, "gold should have no tonal bias");

        // **Against selenium, not against a flat line.** Even at zero selectivity the
        // *visible* effect still favours the shadows, because it scales with the amount
        // of substance converted and there is more substance down there — which is the
        // model's whole point and not something gold escapes. What "even" means is that
        // gold is flatter than a toner that really does prefer the shadows.
        let ratio = |k: &'static str| {
            let q = silver(vec![at(k, 0.8)]);
            q.evaluate(y_from_lstar(0.85)).chroma / q.evaluate(y_from_lstar(0.2)).chroma
        };
        assert!(
            ratio("gold-gp1") > ratio("selenium") * 1.5,
            "gold {:.3} is not appreciably flatter than selenium {:.3}",
            ratio("gold-gp1"),
            ratio("selenium")
        );
    }

    /// **Gold over a sulphided print is red, and gold over plain silver is blue-black.**
    /// The fact that justified an ordered stack, kept after the stack was removed: the
    /// chemistry knows its own sequence, so the user sets amounts.
    #[test]
    fn gold_knows_what_came_before_it() {
        let shadow = y_from_lstar(0.2);
        let alone = silver(vec![at("gold-gp1", 1.0)]).evaluate(shadow).hue;
        let after = silver(vec![at("sepia", 1.0), at("gold-gp1", 1.0)])
            .evaluate(shadow)
            .hue;
        let apart = (alone - after).abs().min(360.0 - (alone - after).abs());
        assert!(
            apart > 45.0,
            "order stopped mattering: {alone} against {after}"
        );
        assert!(
            !(90.0..=330.0).contains(&after),
            "gold on sulphide should be red, got {after}"
        );
    }

    /// A daguerreotype's material makes the **highlights** — the darkest areas are bare
    /// mirror. Separating substance from density is what lets an absorption model hold
    /// it, and gilding lands where a print's gold would not.
    #[test]
    fn a_daguerreotype_carries_its_substance_in_the_highlights() {
        let p = ToningParams {
            enabled: true,
            process: Process::Daguerreotype,
            applied: vec![at("gilding", 1.0)],
            ..Default::default()
        };
        let shadow = p.evaluate(y_from_lstar(0.15)).chroma;
        let highlight = p.evaluate(y_from_lstar(0.85)).chroma;
        assert!(
            highlight > shadow,
            "the plate should colour its highlights: shadow {shadow}, highlight {highlight}"
        );
    }

    /// An untouched module is **skipped**, which is what makes an untoned print the
    /// picture the app has always rendered.
    ///
    /// Note what this does *not* say: that `evaluate` is the identity at defaults. It is
    /// not, and should not be — a gelatin silver print carries a trace of warmth, which
    /// is why it does not read as a scan. `is_active` is what keeps that out of a file
    /// nobody asked to tone, and the luminance is untouched either way.
    #[test]
    fn an_untouched_module_is_skipped_rather_than_neutral() {
        let p = ToningParams {
            enabled: true,
            ..Default::default()
        };
        assert!(!p.is_active(), "an untouched module must render nothing");
        for i in 0..=100 {
            let y = y_from_lstar(i as f32 / 100.0);
            let out = p.evaluate(y);
            // Near-neutral, not neutral: silver gelatin's own tone, and no more. A
            // print that read as a scan would be the wrong default for this app.
            assert!(out.chroma < 0.02, "chroma {} at L*={i}", out.chroma);
            assert!((out.y - y).abs() < 1e-5, "luminance moved at L*={i}");
        }
    }

    /// The bypass is a bypass, and the default state is not active.
    #[test]
    fn the_module_is_off_until_something_is_in_it() {
        let d = ToningParams::default();
        assert!(!d.is_active());
        assert!(d.is_default());
        assert!(!d.is_modified());

        let toned = silver(vec![at("selenium", 0.6)]);
        assert!(toned.is_active());
        assert!(!toned.is_default());
    }

    /// Placement is flat by default, because a diagonal would mean "tone the highlights
    /// and leave the blacks" — a strange thing to open a module on.
    #[test]
    fn placement_opens_flat_rather_than_diagonal() {
        let f = flat_placement();
        for i in 0..=10 {
            let x = i as f32 / 10.0;
            let s = placement_strength(&f, x);
            assert!((s - 1.0).abs() < 1e-5, "flat is not flat at {x}: {s}");
        }
        // And it sits at the middle of the editor's range, so the datum is drawn through
        // the middle of the graph rather than along its ceiling.
        assert!((f.eval(0.5) - 0.5).abs() < 1e-5);
    }

    /// Placement shapes the process's own image colour, not only optional baths. A
    /// fresh process with an empty Chemistry list must therefore still respond.
    #[test]
    fn placement_works_without_chemistry() {
        let flat = ToningParams {
            enabled: true,
            process: Process::Albumen,
            ..Default::default()
        };
        assert!(flat.applied.is_empty());
        let placed = ToningParams {
            placement: Curve::from_points(&[[0.0, 0.0], [1.0, 0.0]]).unwrap(),
            ..flat.clone()
        };
        let y = y_from_lstar(0.25);
        let ordinary = flat.evaluate(y);
        let shaped = placed.evaluate(y);
        assert!(
            shaped.chroma < ordinary.chroma * 0.5,
            "placement did not quiet the untreated process: {} vs {}",
            shaped.chroma,
            ordinary.chroma
        );

        let bypassed = ToningParams {
            placement_enabled: false,
            ..placed
        };
        assert_eq!(
            bypassed.evaluate(y),
            ordinary,
            "placement bypass was not neutral"
        );
    }

    /// Chemistry bypass keeps the bath settings but removes their colour and density
    /// effects until the submodule is switched back on.
    #[test]
    fn chemistry_can_be_bypassed_without_discarding_it() {
        let plain = ToningParams {
            enabled: true,
            ..Default::default()
        };
        let toned = silver(vec![at("selenium", 0.8)]);
        let bypassed = ToningParams {
            chemistry_enabled: false,
            ..toned.clone()
        };
        let y = y_from_lstar(0.2);
        assert_eq!(bypassed.amount("selenium"), 0.8);
        assert_eq!(bypassed.evaluate(y), plain.evaluate(y));
        assert_ne!(toned.evaluate(y), plain.evaluate(y));
    }

    /// Increasing Contrast is a harder grade: highlights rise and shadows deepen.
    #[test]
    fn contrast_control_runs_in_the_named_direction() {
        let soft = ToningParams {
            enabled: true,
            contrast: 0.7,
            ..Default::default()
        };
        let hard = ToningParams {
            enabled: true,
            contrast: 1.4,
            ..Default::default()
        };
        let highlight = y_from_lstar(0.8);
        let shadow = y_from_lstar(0.2);
        assert!(hard.evaluate(highlight).y > soft.evaluate(highlight).y);
        assert!(hard.evaluate(shadow).y < soft.evaluate(shadow).y);
    }

    /// The warm printing-out processes should read as material colour rather than a
    /// saturated red wash. Chemistry can still take them further when requested.
    #[test]
    fn warm_process_defaults_are_restrained() {
        for p in [
            Process::SaltPrint,
            Process::Albumen,
            Process::CollodionPop,
            Process::Kallitype,
            Process::Vandyke,
        ] {
            assert!(
                p.dense_tone().chroma <= 0.060,
                "{} dense preset is too saturated: {}",
                p.label(),
                p.dense_tone().chroma
            );
        }
    }

    /// The LUT is the model, sampled — not an approximation of it. And entry `i` is the
    /// model at the luminance whose L\* is `i / (n - 1)`, which is the arithmetic the
    /// display shader indexes with.
    #[test]
    fn the_bake_matches_the_model_it_came_from() {
        let p = silver(vec![at("sepia", 0.7), at("gold-gp1", 0.4)]);
        let n = 64;
        let flat = p.bake_flat(n);
        assert_eq!(flat.len(), n * 3);
        for i in 0..n {
            let want = p.evaluate(y_from_lstar(i as f32 / (n - 1) as f32));
            let (a, b) = want.ab();
            assert_eq!(flat[i * 3], want.y, "y at {i}");
            assert_eq!(flat[i * 3 + 1], a, "a at {i}");
            assert_eq!(flat[i * 3 + 2], b, "b at {i}");
        }
    }

    /// The table stores Cartesian `(a, b)` so interpolating across 0°/360° does not
    /// sweep a hue the long way round the wheel. This is that failure, constructed.
    #[test]
    fn interpolating_the_table_does_not_take_the_long_way_round() {
        let (a0, b0) = Tint {
            hue: 355.0,
            chroma: 0.08,
        }
        .ab();
        let (a1, b1) = Tint {
            hue: 5.0,
            chroma: 0.08,
        }
        .ab();
        let mid = Tint::from_ab(0.5 * (a0 + a1), 0.5 * (b0 + b1));
        let from_zero = mid.hue.min(360.0 - mid.hue);
        assert!(
            from_zero < 1.0,
            "Cartesian interpolation drifted to {}",
            mid.hue
        );
        assert!(
            mid.chroma > 0.07,
            "and it should stay saturated, got {}",
            mid.chroma
        );
    }

    /// L\* and luminance round-trip, because the bake indexes on one and the model
    /// works in the other.
    #[test]
    fn lstar_and_luminance_round_trip() {
        for i in 0..=100 {
            let l = i as f32 / 100.0;
            let back = lstar_encode(y_from_lstar(l));
            assert!((back - l).abs() < 1e-3, "L*={l} came back as {back}");
        }
    }

    /// Every treatment key a process offers is unique within that process, or the
    /// panel's add-menu and the sidecar's key lookup would both pick the first of two.
    #[test]
    fn a_process_never_offers_one_key_twice() {
        for p in Process::ALL {
            let mut seen: Vec<&str> = Vec::new();
            for t in p.treatments() {
                assert!(!seen.contains(&t.key), "{:?} offers {} twice", p, t.key);
                seen.push(t.key);
            }
        }
    }
}

#[cfg(test)]
mod scale_report {
    use super::*;

    /// Not an assertion — a way to read what each process actually produces without
    /// launching the app. `cargo test -p raw-core the_process_scale -- --nocapture`.
    /// A hue sweep, so an OKLCh number can be chosen against the CIELAB it produces
    /// rather than against a guess. `cargo test -p raw-core the_hue_sweep -- --nocapture`.
    #[test]
    fn the_hue_sweep() {
        for h in (220..=280).step_by(5) {
            let t = Tint {
                hue: h as f32,
                chroma: 0.17,
            };
            let (a, b) = t.ab();
            let lab = crate::colour::lab_of(0.12, a, b);
            eprintln!("oklch {h:>3}  ->  a*{:>+6.1} b*{:>+6.1}", lab[1], lab[2]);
        }
    }

    #[test]
    fn the_process_scale() {
        for p in Process::ALL {
            let t = ToningParams {
                enabled: true,
                process: p,
                ..Default::default()
            };
            let at = |l: f32| {
                let o = t.evaluate(y_from_lstar(l));
                let lab = crate::colour::lab_of(o.y, o.ab().0, o.ab().1);
                format!("L{:>5.1} a{:>+6.1} b{:>+6.1}", lab[0], lab[1], lab[2])
            };
            eprintln!(
                "{:<22} white[{}]  hi[{}]  mid[{}]  shadow[{}]",
                p.label(),
                at(1.0),
                at(0.8),
                at(0.45),
                at(0.15)
            );
        }
    }
}
