//! `monopro` without its window.
//!
//! ```text
//! monopro render <raw>... [--out FILE | --out-dir DIR]
//!                [--proof] [--format png|jpeg] [--bits 8|16] [--overwrite]
//! monopro info <raw>...
//! monopro help | --version
//! ```
//!
//! Anything else — no arguments, or a path — opens the window as before. The
//! command word is checked first and a window is never created for one, so these
//! run from a script, over SSH, or in CI with no display attached. In a packaged
//! macOS build the executable is `monopro.app/Contents/MacOS/monopro`.
//!
//! **`render` is the Export buttons, not a second exporter.** It starts from the
//! file's sidecar, resolves bypasses with `Params::effective`, renders through the
//! same `Viewport::export`, builds its spec with the same `Spec::for_params`, and
//! is refused by the same size limits.
//!
//! By default it writes the **master**: a 16-bit uncompressed TIFF in the master
//! colour space, exactly as Export Master does, with no format or depth to choose.
//! `--proof` writes a **proof** with the EXPORT module's proof preferences, and
//! `--format`, `--bits` or a `.png`/`.jpg` `--out` override them for this run —
//! each of which means a proof, since a master is only ever a TIFF. Names use the
//! Settings suffixes, and the destination is the configured output folder or the
//! raw's own folder, as Quick Export's would be.
//!
//! Exit status, so a script can tell the outcomes apart:
//!
//! | code | meaning |
//! |---|---|
//! | 0 | every file done |
//! | 1 | at least one file failed; the others were still attempted |
//! | 2 | the command line was wrong, and nothing was attempted |

use std::path::{Path, PathBuf};

use raw_core::{DemosaicAlgo, Params, Sampling, SensorImage, ToneMap, scene, sidecar};
use raw_gpu::{GpuContext, Viewport};

use crate::export::{self, Container, Depth, Target};
use crate::settings::Settings;

pub const OK: i32 = 0;
pub const FAILED: i32 = 1;
pub const USAGE: i32 = 2;

const HELP: &str = "\
monopro — a monochrome RAW processor

usage:
  monopro [PATH]                 open the window, optionally at a raw or a folder
  monopro render <raw>... [options]
  monopro info <raw>...
  monopro help | --version

render writes each raw as its sidecar describes it, as the Export buttons would:
a master (16-bit TIFF) by default, or a proof (PNG or JPEG).
  -o, --out FILE      the file to write (one raw only); .tif is a master,
                      .png or .jpg a proof
  --out-dir DIR       the folder to write into (default: the Settings output folder,
                      otherwise beside each raw), named with the Settings suffixes
  --proof             write a proof with the EXPORT module's proof settings
  --format FMT        a proof in png or jpeg (implies --proof)
  --bits N            a proof at 8 or 16 bits (JPEG is 8 only; implies --proof)
  --overwrite         replace files that already exist

  for testing, applied over the sidecar:
  --agx               AgX tone map
  --mask PCT          Contrast Mask on, spacer as a percentage of the diagonal
  --demosaic ALGO     full-resolution sampling with this demosaic

info prints what a raw is and what its sidecar holds.

exit status: 0 done, 1 a file failed, 2 bad command line
";

/// Run a command if `args` is one, and return its exit code. `None` means the
/// arguments are for the window.
pub fn run(args: &[String]) -> Option<i32> {
    let first = args.first()?;
    if !is_command(first) {
        return None;
    }
    Some(match parse(args) {
        Ok(Command::Help) => {
            print!("{HELP}");
            OK
        }
        Ok(Command::Version) => {
            println!("monopro {}", env!("CARGO_PKG_VERSION"));
            OK
        }
        Ok(Command::Info(paths)) => info(&paths),
        Ok(Command::Render(r)) => render(&r),
        Err(why) => {
            eprintln!("monopro: {why}\nrun `monopro help` for usage");
            USAGE
        }
    })
}

/// **A command word is only ever the first argument**, and only these. Everything
/// else is a path for the window, which is how a double-clicked or dragged file
/// has always arrived. A file literally named `render` opens as `./render`.
fn is_command(arg: &str) -> bool {
    matches!(
        arg,
        "render" | "info" | "help" | "--help" | "-h" | "--version" | "-V"
    )
}

#[derive(Debug, PartialEq)]
enum Command {
    Render(Render),
    Info(Vec<PathBuf>),
    Help,
    Version,
}

#[derive(Debug, Default, PartialEq)]
struct Render {
    inputs: Vec<PathBuf>,
    out: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    proof: bool,
    format: Option<Container>,
    bits: Option<Depth>,
    overwrite: bool,
    overrides: Overrides,
}

/// The testing flags. They exist so a module can be judged, and timed, on real
/// frames without a window; each is applied over whatever the sidecar says.
#[derive(Debug, Default, PartialEq, Clone, Copy)]
struct Overrides {
    agx: bool,
    mask: Option<f32>,
    demosaic: Option<DemosaicAlgo>,
}

impl Overrides {
    fn apply(self, params: &mut Params) {
        if self.agx {
            params.display.enabled = true;
            params.display.tone_map = ToneMap::AGX_DEFAULT;
        }
        if let Some(spacer) = self.mask {
            params.contrast_mask.enabled = true;
            params.contrast_mask.spacer = spacer;
        }
        if let Some(algo) = self.demosaic {
            params.luminance.sampling = Sampling::Demosaic(algo);
        }
    }
}

fn parse(args: &[String]) -> Result<Command, String> {
    let (command, rest) = args.split_first().ok_or("no command")?;
    match command.as_str() {
        "help" | "--help" | "-h" => Ok(Command::Help),
        "--version" | "-V" => Ok(Command::Version),
        "info" => {
            if let Some(flag) = rest.iter().find(|a| a.starts_with('-') && a.len() > 1) {
                return Err(format!("info takes no options, got {flag}"));
            }
            if rest.is_empty() {
                return Err("info needs at least one raw".into());
            }
            Ok(Command::Info(rest.iter().map(PathBuf::from).collect()))
        }
        "render" => parse_render(rest).map(Command::Render),
        other => Err(format!("unknown command {other}")),
    }
}

fn parse_render(rest: &[String]) -> Result<Render, String> {
    fn value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a str, String> {
        it.next()
            .map(String::as_str)
            .ok_or_else(|| format!("{flag} needs a value"))
    }

    let mut r = Render::default();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-o" | "--out" => r.out = Some(value(&mut it, arg)?.into()),
            "--out-dir" => r.out_dir = Some(value(&mut it, arg)?.into()),
            "--proof" => r.proof = true,
            "--format" => {
                let v = value(&mut it, arg)?;
                r.format = Some(
                    container_named(v)
                        .ok_or_else(|| format!("unknown format {v:?}; have tiff, png, jpeg"))?,
                );
            }
            "--bits" => {
                r.bits = Some(match value(&mut it, arg)? {
                    "8" => Depth::Eight,
                    "16" => Depth::Sixteen,
                    v => return Err(format!("--bits is 8 or 16, got {v:?}")),
                });
            }
            "--overwrite" => r.overwrite = true,
            "--agx" => r.overrides.agx = true,
            "--mask" => {
                let v = value(&mut it, arg)?;
                r.overrides.mask = Some(
                    v.parse::<f32>()
                        .ok()
                        .filter(|p| p.is_finite() && *p > 0.0)
                        .ok_or_else(|| format!("--mask takes a positive percentage, got {v:?}"))?,
                );
            }
            "--demosaic" => {
                let v = value(&mut it, arg)?;
                r.overrides.demosaic = Some(
                    DemosaicAlgo::UI_ORDER
                        .into_iter()
                        .find(|a| a.label().eq_ignore_ascii_case(v))
                        .ok_or_else(|| {
                            format!(
                                "unknown demosaic {v:?}; have {:?}",
                                DemosaicAlgo::UI_ORDER.map(|a| a.label())
                            )
                        })?,
                );
            }
            "--" => r.inputs.extend(it.by_ref().map(PathBuf::from)),
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(format!("unknown option {flag}"));
            }
            path => r.inputs.push(path.into()),
        }
    }

    if r.inputs.is_empty() {
        return Err("render needs at least one raw".into());
    }
    if r.out.is_some() && r.out_dir.is_some() {
        return Err("--out and --out-dir cannot be used together".into());
    }
    if let Some(out) = &r.out {
        if r.inputs.len() > 1 {
            return Err("--out names one file; use --out-dir for several raws".into());
        }
        // The extension says which kind of file this is, and must agree with any
        // flag that says so too: writing PNG bytes into `x.tif` is a file that lies
        // about itself, and a `.tif` proof would be a proof that looks archival.
        let by_extension = out
            .extension()
            .and_then(|e| e.to_str())
            .and_then(container_named)
            .ok_or_else(|| format!("{} must end in .tif, .png or .jpg", out.display()))?;
        if by_extension == Container::Tiff {
            if r.proof || r.format.is_some() || r.bits.is_some() {
                return Err(format!(
                    "{} is a master, which is always a 16-bit TIFF; a proof is .png or .jpg",
                    out.display()
                ));
            }
        } else {
            match r.format {
                None => r.format = Some(by_extension),
                Some(f) if f != by_extension => {
                    return Err(format!(
                        "--format {} disagrees with {}",
                        f.label(),
                        out.display()
                    ));
                }
                Some(_) => {}
            }
        }
    }
    if let Some(f) = r.format
        && !Container::PROOF_ORDER.contains(&f)
    {
        return Err(format!(
            "--format picks a proof's format, PNG or JPEG; a master is always {}",
            f.label()
        ));
    }
    // A format or a depth only means anything for a proof: a master has neither.
    r.proof |= r.format.is_some() || r.bits.is_some();
    if let (Some(f), Some(d)) = (r.format, r.bits)
        && !f.supports(d)
    {
        return Err(format!("{} cannot be {}", f.label(), d.label()));
    }
    Ok(r)
}

fn container_named(name: &str) -> Option<Container> {
    match name.to_ascii_lowercase().as_str() {
        "tif" | "tiff" => Some(Container::Tiff),
        "png" => Some(Container::Png),
        "jpg" | "jpeg" => Some(Container::Jpeg),
        _ => None,
    }
}

impl Render {
    /// The file format, from Settings and then the flags.
    fn target(&self, settings: &Settings) -> Target {
        if !self.proof {
            return settings.master_target();
        }
        let mut t = settings.proof_target();
        if let Some(c) = self.format {
            t.container = c;
        }
        if let Some(d) = self.bits {
            t.depth = d;
        }
        // A JPEG chosen by flag over a 16-bit proof preference: the flag names the
        // container, so the depth is what gives way.
        t.settle();
        t
    }

    fn destination(&self, input: &Path, settings: &Settings, target: Target) -> PathBuf {
        if let Some(out) = &self.out {
            return out.clone();
        }
        let dir = self
            .out_dir
            .clone()
            .or_else(|| settings.output_folder.clone().filter(|d| d.is_dir()))
            .or_else(|| input.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        dir.join(settings.export_name(input, target))
    }
}

/// Settings as the window would read them, with the same forgiveness: a
/// preferences file must never be the reason a render cannot run.
fn load_settings() -> Settings {
    match Settings::load() {
        sidecar::Loaded::Ok(s) => s,
        sidecar::Loaded::Absent => Settings::default(),
        sidecar::Loaded::Corrupt(e) => {
            eprintln!("monopro: settings could not be read, using defaults — {e}");
            Settings::default()
        }
    }
}

// ------------------------------------------------------------------- render

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    ctx: GpuContext,
}

fn render(r: &Render) -> i32 {
    let settings = load_settings();
    let target = r.target(&settings);
    let proof = r.proof.then(|| settings.proof_scale());
    let dests: Vec<PathBuf> = r
        .inputs
        .iter()
        .map(|input| r.destination(input, &settings, target))
        .collect();

    // Checked before any work, because both would otherwise surface halfway
    // through a batch: two raws that share a stem (`a.RAF`, `a.DNG`) name the same
    // export, and a missing folder fails every file in turn.
    if let Some(dir) = &r.out_dir
        && !dir.is_dir()
    {
        eprintln!("monopro: {} is not a folder", dir.display());
        return USAGE;
    }
    for (i, d) in dests.iter().enumerate() {
        if let Some(j) = dests[..i].iter().position(|e| e == d) {
            eprintln!(
                "monopro: {} and {} would both be written to {}",
                r.inputs[j].display(),
                r.inputs[i].display(),
                d.display()
            );
            return USAGE;
        }
    }

    let Some((device, queue)) = raw_gpu::headless_device() else {
        eprintln!("monopro: no usable GPU adapter");
        return FAILED;
    };
    let ctx = GpuContext::new(&device);
    let mut gpu = Gpu { device, queue, ctx };

    let mut failed = 0;
    for (input, dest) in r.inputs.iter().zip(&dests) {
        match render_one(input, dest, target, proof, r, &settings, &mut gpu) {
            Ok(line) => println!("{line}"),
            Err(why) => {
                eprintln!("{}: {why}", input.display());
                failed += 1;
            }
        }
    }
    if failed == 0 {
        return OK;
    }
    if r.inputs.len() > 1 {
        eprintln!("monopro: {failed} of {} failed", r.inputs.len());
    }
    FAILED
}

fn render_one(
    input: &Path,
    dest: &Path,
    target: Target,
    proof: Option<export::ProofScale>,
    r: &Render,
    settings: &Settings,
    gpu: &mut Gpu,
) -> Result<String, String> {
    if dest.exists() && !r.overwrite {
        return Err(format!(
            "{} already exists; pass --overwrite to replace it",
            dest.display()
        ));
    }
    // Rendering at defaults from a file someone believes carries their edits would
    // quietly write the wrong picture, and a batch is exactly where nobody looks.
    let mut params = match sidecar::read(input) {
        sidecar::Loaded::Ok(s) => s.params,
        sidecar::Loaded::Absent => Params::default(),
        sidecar::Loaded::Corrupt(e) => {
            return Err(format!(
                "sidecar unreadable, not rendering at defaults: {e}"
            ));
        }
    };
    r.overrides.apply(&mut params);
    let params = params.effective();

    let sensor = SensorImage::load(input).map_err(|e| e.to_string())?;
    let (sc, _) = scene::decode(&sensor, params.decode);
    let luma = scene::derive_luminance(&sc, params.luminance.sampling, params.luminance.weighting);
    drop(sc);
    // The composition from the sidecar over the file's own orientation tag, as the
    // window resolves it, so this is the picture the viewport showed.
    let frame = raw_core::Frame::resolve(
        luma.output_dims,
        sensor.meta.orientation,
        &params.composition,
    );

    // Read now, as the button reads it: IPTC edited in Lightbox lives in the sidecar.
    let metadata = sidecar::effective_metadata(input)?;
    let meta = settings.export_metadata.then_some(&metadata);
    let camera = settings
        .export_camera_exif
        .then(|| raw_core::camera_exif::CameraExif::read(input))
        .flatten();
    let spec = export::Spec::for_params(target, proof, &params, meta, settings.proof_dither)
        .with_camera(camera);
    // Before the render, not after: FRAME margins are physical and can take even a
    // proof past the limits, and a refused file should not cost a GPU pass first.
    spec.checked_layout(frame.output_dims())?;

    let mut vp = Viewport::new(&gpu.device, &gpu.queue, &luma);
    let (w, h, data) = vp
        .export(
            &mut gpu.ctx,
            &gpu.device,
            &gpu.queue,
            &params,
            &frame,
            |_, _| {},
        )
        .ok_or("render failed")?;
    let d = spec.dims(raw_core::Dims {
        w: w as usize,
        h: h as usize,
    });
    export::write(dest, w, h, &data, &spec).map_err(|e| format!("write failed: {e}"))?;
    Ok(format!(
        "{} · {} · {}x{} · {:.0} ppi",
        dest.display(),
        spec.target.label(),
        d.w,
        d.h,
        spec.output.ppi
    ))
}

// --------------------------------------------------------------------- info

fn info(paths: &[PathBuf]) -> i32 {
    let mut failed = 0;
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{}", path.display());
        let row = |k: &str, v: &str| println!("  {k:<12} {v}");

        match SensorImage::load(path) {
            Ok(s) => {
                let g = &s.geom;
                let pattern: String = g
                    .pattern
                    .iter()
                    .flatten()
                    .map(|c| match c {
                        raw_core::CfaColor::Red => 'R',
                        raw_core::CfaColor::Green => 'G',
                        raw_core::CfaColor::Blue => 'B',
                    })
                    .collect();
                row("camera", &s.camera);
                row(
                    "sensor",
                    &format!("{} x {}, Bayer {pattern}", g.crop.w, g.crop.h),
                );
                row("orientation", s.meta.orientation.label());
                let m = &s.meta;
                let shot: Vec<String> = [
                    m.iso.map(|v| format!("ISO {v}")),
                    m.shutter.map(shutter),
                    m.aperture.map(|v| format!("f/{v:.1}")),
                    m.focal_len.map(|v| format!("{v:.0} mm")),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !shot.is_empty() {
                    row("exposure", &shot.join(" · "));
                }
                if let Some(lens) = &m.lens {
                    row("lens", lens);
                }
                if let Some(when) = &m.date_time {
                    row("taken", when);
                }
            }
            Err(e) => {
                row("cannot open", &e.to_string());
                failed += 1;
                // A file that is there but will not decode is still worth the
                // sidecar rows below: it can carry a rating from Lightbox. One
                // that is not there has nothing more to say.
                if !path.exists() {
                    continue;
                }
            }
        }

        let side = sidecar::path_for(path);
        match sidecar::read(path) {
            sidecar::Loaded::Absent => row("sidecar", "none"),
            sidecar::Loaded::Ok(s) => row(
                "sidecar",
                &format!(
                    "{} ({})",
                    if s.is_developed() {
                        "edited"
                    } else {
                        "not edited"
                    },
                    side.display()
                ),
            ),
            sidecar::Loaded::Corrupt(e) => {
                row("sidecar", &format!("unreadable: {e}"));
                failed += 1;
            }
        }
        if let Ok(meta) = sidecar::effective_metadata(path) {
            if let Some(stars) = meta.rating {
                row("rating", &stars.to_string());
            }
            if let Some(label) = &meta.label {
                row("label", label);
            }
            if let Some(title) = &meta.title {
                row("title", title);
            }
        }
    }
    if failed > 0 { FAILED } else { OK }
}

/// Seconds as a photographer writes them: 1/250 s, 2 s.
fn shutter(seconds: f32) -> String {
    if seconds >= 1.0 || seconds <= 0.0 {
        format!("{seconds} s")
    } else {
        format!("1/{:.0} s", 1.0 / seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn render_of(s: &str) -> Result<Render, String> {
        match parse(&args(s))? {
            Command::Render(r) => Ok(r),
            other => panic!("expected render, got {other:?}"),
        }
    }

    #[test]
    fn paths_and_no_arguments_open_the_window() {
        assert_eq!(run(&[]), None, "no arguments is the window");
        assert_eq!(run(&args("/photos/a.RAF")), None, "a path is the window");
        assert_eq!(
            run(&args("./render")),
            None,
            "a file named render is reachable"
        );
    }

    #[test]
    fn a_bad_command_line_is_a_usage_error_before_any_work() {
        for bad in [
            "render",
            "render a.RAF --bits 12",
            "render a.RAF --format webp",
            "render a.RAF --frobnicate",
            "render a.RAF b.RAF --out x.tif",
            "render a.RAF --out x.tif --out-dir d",
            "render a.RAF --out x.webp",
            "render a.RAF --out x.png --format tiff",
            "render a.RAF --proof --format tiff",
            "render a.RAF --format tiff",
            "render a.RAF --out x.tif --proof",
            "render a.RAF --out x.tif --bits 8",
            "render a.RAF --format jpeg --bits 16",
            "render a.RAF --mask -3",
            "render a.RAF --demosaic nope",
            "render a.RAF --out",
            "info",
            "info --json a.RAF",
        ] {
            assert_eq!(
                run(&args(bad)),
                Some(USAGE),
                "{bad:?} should be a usage error"
            );
        }
    }

    #[test]
    fn the_out_extension_says_master_or_proof() {
        let r = render_of("render a.RAF -o x.PNG").unwrap();
        assert!(r.proof, "a .png is a proof");
        assert_eq!(r.format, Some(Container::Png));
        let r = render_of("render a.RAF --out x.jpeg --format jpg").unwrap();
        assert_eq!(r.format, Some(Container::Jpeg));
        let r = render_of("render a.RAF --out x.tif").unwrap();
        assert!(!r.proof, "a .tif is the master");
    }

    #[test]
    fn a_master_is_always_a_sixteen_bit_tiff_and_a_proof_follows_its_preferences() {
        let settings = Settings::default();
        let master = render_of("render a.RAF").unwrap().target(&settings);
        assert_eq!(master, Target::master(settings.master_space()));
        assert_eq!(master.depth, Depth::Sixteen);
        let proof = render_of("render a.RAF --proof").unwrap().target(&settings);
        assert_eq!(proof, settings.proof_target());
    }

    #[test]
    fn a_format_or_a_depth_means_a_proof() {
        assert!(render_of("render a.RAF --format png").unwrap().proof);
        assert!(render_of("render a.RAF --bits 16").unwrap().proof);
    }

    #[test]
    fn a_jpeg_flag_over_a_sixteen_bit_proof_preference_settles_to_eight() {
        let settings = Settings {
            proof_depth: Depth::Sixteen.key().into(),
            ..Settings::default()
        };
        let t = render_of("render a.RAF --format jpeg")
            .unwrap()
            .target(&settings);
        assert_eq!(t.container, Container::Jpeg);
        assert_eq!(t.depth, Depth::Eight);
    }

    #[test]
    fn exports_land_beside_the_raw_under_the_settings_name() {
        let settings = Settings {
            output_folder: None,
            ..Settings::default()
        };
        let r = render_of("render /shoot/a.RAF").unwrap();
        let t = r.target(&settings);
        assert_eq!(
            r.destination(Path::new("/shoot/a.RAF"), &settings, t),
            Path::new("/shoot").join(settings.export_name(Path::new("a.RAF"), t))
        );
        let r = render_of("render /shoot/a.RAF --out-dir /out").unwrap();
        assert_eq!(
            r.destination(Path::new("/shoot/a.RAF"), &settings, t),
            Path::new("/out").join(settings.export_name(Path::new("a.RAF"), t))
        );
    }

    #[test]
    fn two_raws_with_one_stem_are_refused_before_rendering() {
        let dir = std::env::temp_dir().join(format!("monopro-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.to_string_lossy();
        assert_eq!(
            run(&args(&format!("render a.RAF a.DNG --out-dir {out}"))),
            Some(USAGE),
            "both would be written to the same file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shutter_speeds_read_as_written_on_the_dial() {
        assert_eq!(shutter(0.004), "1/250 s");
        assert_eq!(shutter(2.0), "2 s");
    }
}
