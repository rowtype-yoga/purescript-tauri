// Surface/path/paint/readback patterns adapted from fframes at
// 4ffb052d3430e0672b4f3647450c181606ddf418. See ../licenses/fframes.txt.
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

#[cfg(target_os = "macos")]
use objc2::{rc::Retained, runtime::ProtocolObject};
#[cfg(target_os = "macos")]
use objc2_metal::{MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice};

use fontdb::{Database, Family, Query, Source, Stretch, Style, Weight, ID};
use rustybuzz::{Face, Feature, UnicodeBuffer};
use skia_safe::canvas::SaveLayerRec;
#[cfg(target_os = "macos")]
use skia_safe::gpu::{self, DirectContext};
use skia_safe::{
    color_filters, image_filters, paint, surfaces, AlphaType, BlendMode, Canvas, ClipOp, ColorType,
    Data, FilterMode, Font, FontHinting, FontMgr, ImageInfo, Matrix, Paint, PaintStyle, Path,
    PathBuilder, PathFillType, PictureRecorder, Point, Rect, Surface, TileMode, Typeface,
};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

use crate::{failure, FontAsset, HostResult};

#[derive(Default)]
pub(crate) struct Metrics {
    pub width: f64,
    pub ascent: f64,
    pub descent: f64,
}

pub(crate) type Color = [u8; 4];

pub(crate) struct Stroke {
    pub color: Color,
    pub width: f32,
    pub join: i32,
    pub cap: i32,
}

pub(crate) struct CirclePattern {
    pub rect: [f32; 4],
    pub background: Color,
    pub ink: Color,
    pub tile: f32,
    pub radius: f32,
    pub origin: [f32; 2],
}
#[derive(Clone)]
struct DrawStyle {
    blend: BlendMode,
    blur: f32,
    tint: Option<skia_safe::ColorFilter>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SurfaceBackend {
    Cpu,
    #[cfg(target_os = "macos")]
    Metal,
}

struct Raster {
    surface: Surface,
    backend: SurfaceBackend,
    style: DrawStyle,
    saved: Vec<DrawStyle>,
}

#[cfg(target_os = "macos")]
struct MetalBackend {
    // Fields are dropped in declaration order. Graphics drops its surfaces before
    // this backend, then Ganesh releases the queue and device before our retains.
    context: DirectContext,
    _queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    _device: Retained<ProtocolObject<dyn MTLDevice>>,
}

#[cfg(target_os = "macos")]
impl MetalBackend {
    fn new() -> HostResult<Self> {
        let device = MTLCreateSystemDefaultDevice()
            .ok_or_else(|| failure("could not create the default Metal device"))?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| failure("could not create the Metal command queue"))?;
        // BackendContext retains its inputs while it exists; DirectContext takes
        // another retain, so the temporary backend can drop immediately.
        let backend = unsafe {
            gpu::mtl::BackendContext::new(
                Retained::as_ptr(&device) as gpu::mtl::Handle,
                Retained::as_ptr(&queue) as gpu::mtl::Handle,
            )
        };
        let mut context = gpu::direct_contexts::make_metal(&backend, None)
            .ok_or_else(|| failure("could not initialize Skia Ganesh with Metal"))?;
        if context.is_device_lost() || context.oomed() {
            return Err(failure("Skia Ganesh Metal context is unavailable"));
        }
        Ok(Self {
            context,
            _queue: queue,
            _device: device,
        })
    }

    fn encode(
        &mut self,
        image: &skia_safe::Image,
        options: &skia_safe::png_encoder::Options,
    ) -> HostResult<Data> {
        // PNG is a non-local compatibility path. Complete Ganesh work before
        // its encoder reads the snapshot back from the Metal texture.
        self.context.flush_submit_and_sync_cpu();
        if self.context.is_device_lost() || self.context.oomed() {
            return Err(failure(
                "Metal canvas became unavailable during PNG encoding",
            ));
        }
        skia_safe::png_encoder::encode_image(&mut self.context, image, options)
            .ok_or_else(|| failure("could not encode Metal canvas PNG"))
    }
}

struct RequestedFont {
    size: f64,
    weight: u16,
    style: Style,
    small_caps: bool,
}

struct GlyphRun {
    id: ID,
    size: f32,
    embolden: bool,
    skew_x: f32,
    glyphs: Vec<u16>,
    positions: Vec<Point>,
}

pub(crate) struct Graphics {
    fonts: Arc<Database>,
    features: HashMap<ID, Vec<Feature>>,
    font_manager: FontMgr,
    typefaces: HashMap<ID, Typeface>,
    // This must precede MetalBackend: every Ganesh surface is dropped before its
    // DirectContext, command queue, and device.
    surfaces: HashMap<u32, Raster>,
    #[cfg(target_os = "macos")]
    metal: Option<MetalBackend>,
    #[cfg(target_os = "macos")]
    terminal_graphics_active: bool,
    next_surface: u32,
}

impl Graphics {
    pub fn new(assets: &[FontAsset]) -> HostResult<Self> {
        let mut fonts = Database::new();
        let mut features = HashMap::new();
        for asset in assets {
            let ids = fonts.load_font_source(Source::Binary(Arc::new(asset.data)));
            if ids.is_empty() {
                return Err(failure(format!("invalid font asset: {}", asset.family)));
            }
            let parsed_features = asset
                .features
                .split(',')
                .filter(|part| !part.trim().is_empty())
                .map(|part| {
                    Feature::from_str(part.trim())
                        .map_err(|_| failure(format!("invalid OpenType feature: {part}")))
                })
                .collect::<HostResult<Vec<_>>>()?;
            for id in ids {
                let mut face = fonts
                    .face(id)
                    .ok_or_else(|| failure("loaded font face is unavailable"))?
                    .clone();
                fonts.remove_face(id);
                // Explicit aliases and metadata are authoritative. The same bytes
                // may be registered under several CSS names without copying them.
                face.families.insert(
                    0,
                    (asset.family.into(), fontdb::Language::English_UnitedStates),
                );
                face.weight = Weight(asset.weight);
                face.style = if asset.italic {
                    Style::Italic
                } else {
                    Style::Normal
                };
                let id = fonts.push_face_info(face);
                features.insert(id, parsed_features.clone());
            }
        }
        // Supplied faces precede system faces in otherwise identical CSS matches.
        fonts.load_system_fonts();
        if fonts.is_empty() {
            return Err(failure("no fonts are available"));
        }
        Ok(Self {
            fonts: Arc::new(fonts),
            features,
            font_manager: FontMgr::new(),
            typefaces: HashMap::new(),
            surfaces: HashMap::new(),
            #[cfg(target_os = "macos")]
            metal: None,
            #[cfg(target_os = "macos")]
            terminal_graphics_active: false,
            next_surface: 1,
        })
    }

    pub fn measure(&self, shorthand: &str, text: &str) -> HostResult<Metrics> {
        self.shape_text(shorthand, text, None)
            .map(|(metrics, _, _)| metrics)
    }

    fn shape_text(
        &self,
        shorthand: &str,
        text: &str,
        mut runs: Option<&mut Vec<GlyphRun>>,
    ) -> HostResult<(Metrics, ID, f64)> {
        let font = svgtypes::FontShorthand::from_str(shorthand)
            .map_err(|error| failure(format!("invalid CSS font {shorthand:?}: {error}")))?;
        let size = font_size(font.font_size)?;
        let families = svgtypes::parse_font_families(font.font_family)?;
        let names: Vec<_> = families
            .iter()
            .map(|family| match family {
                svgtypes::FontFamily::Named(name) => Family::Name(name),
                svgtypes::FontFamily::Serif => Family::Serif,
                svgtypes::FontFamily::SansSerif => Family::SansSerif,
                svgtypes::FontFamily::Monospace => Family::Monospace,
                svgtypes::FontFamily::Cursive => Family::Cursive,
                svgtypes::FontFamily::Fantasy => Family::Fantasy,
            })
            .collect();
        let weight = match font.font_weight.unwrap_or("normal") {
            "normal" => 400,
            "bold" | "bolder" => 700,
            "lighter" => 300,
            value => value.parse::<u16>()?,
        };
        let style = match font.font_style {
            Some("italic") => Style::Italic,
            Some("oblique") => Style::Oblique,
            _ => Style::Normal,
        };
        let stretch = match font.font_stretch {
            Some("ultra-condensed") => Stretch::UltraCondensed,
            Some("extra-condensed") => Stretch::ExtraCondensed,
            Some("condensed") => Stretch::Condensed,
            Some("semi-condensed") => Stretch::SemiCondensed,
            Some("semi-expanded") => Stretch::SemiExpanded,
            Some("expanded") => Stretch::Expanded,
            Some("extra-expanded") => Stretch::ExtraExpanded,
            Some("ultra-expanded") => Stretch::UltraExpanded,
            _ => Stretch::Normal,
        };
        let base = self
            .fonts
            .query(&Query {
                families: &names,
                weight: Weight(weight),
                stretch,
                style,
            })
            .or_else(|| {
                self.fonts.query(&Query {
                    families: &[Family::SansSerif],
                    weight: Weight(weight),
                    stretch,
                    style,
                })
            })
            .or_else(|| self.fonts.faces().next().map(|face| face.id))
            .ok_or_else(|| failure(format!("no font matches {shorthand:?}")))?;
        let requested = RequestedFont {
            size,
            weight,
            style,
            small_caps: font.font_variant == Some("small-caps"),
        };
        let mut metrics = Metrics::default();
        let mut run_start = 0;
        let mut run_font = base;
        let mut run_script = Script::Common;
        // Keep combining marks and emoji sequences together during fallback.
        // Script changes split shaping runs, but common punctuation and inherited
        // marks retain the surrounding script instead of breaking ligatures.
        for (offset, grapheme) in text.grapheme_indices(true) {
            let selected = self.fallback(base, grapheme);
            let script = grapheme
                .chars()
                .map(|c| c.script())
                .find(|script| !matches!(script, Script::Common | Script::Inherited))
                .unwrap_or(run_script);
            if offset > run_start
                && (selected != run_font || (script != run_script && run_script != Script::Common))
            {
                self.shape(
                    run_font,
                    &text[run_start..offset],
                    &requested,
                    &mut metrics,
                    runs.as_deref_mut(),
                )?;
                run_start = offset;
            }
            run_font = selected;
            run_script = script;
        }
        if run_start < text.len() {
            self.shape(
                run_font,
                &text[run_start..],
                &requested,
                &mut metrics,
                runs.as_deref_mut(),
            )?;
        }
        Ok((metrics, base, size))
    }

    fn supports(&self, id: ID, text: &str) -> bool {
        self.fonts.with_face_data(id, |data, index| {
            Face::from_slice(data, index).map(|face| text.chars().all(|c| {
                // Joiners and variation selectors modify their surrounding glyphs.
                matches!(c, '\u{200c}' | '\u{200d}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
                    || face.glyph_index(c).is_some()
            })).unwrap_or(false)
        }).unwrap_or(false)
    }

    fn fallback(&self, base: ID, text: &str) -> ID {
        if self.supports(base, text) {
            return base;
        }
        let original = self.fonts.face(base);
        self.fonts
            .faces()
            .filter(|face| {
                original
                    .map(|original| {
                        face.style == original.style
                            || face.weight == original.weight
                            || face.stretch == original.stretch
                    })
                    .unwrap_or(true)
            })
            .find(|face| self.supports(face.id, text))
            .map(|face| face.id)
            .or_else(|| {
                self.fonts
                    .faces()
                    .find(|face| self.supports(face.id, text))
                    .map(|face| face.id)
            })
            .unwrap_or(base)
    }

    fn shape(
        &self,
        id: ID,
        text: &str,
        requested: &RequestedFont,
        metrics: &mut Metrics,
        runs: Option<&mut Vec<GlyphRun>>,
    ) -> HostResult<()> {
        let selected = self
            .fonts
            .face(id)
            .ok_or_else(|| failure("font face is unavailable"))?;
        let embolden = requested.weight >= 600 && selected.weight.0 < 600;
        let skew_x = if requested.style != Style::Normal && selected.style == Style::Normal {
            -0.21255656
        } else {
            0.0
        };
        self.fonts
            .with_face_data(id, |data, index| -> HostResult<()> {
                let face = Face::from_slice(data, index)
                    .ok_or_else(|| failure("font could not be shaped"))?;
                let scale = requested.size / f64::from(face.units_per_em());
                let mut buffer = UnicodeBuffer::new();
                buffer.push_str(text);
                buffer.guess_segment_properties();
                let normal = self.features.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                let mut caps = Vec::new();
                let features = if requested.small_caps {
                    caps.extend_from_slice(normal);
                    caps.push(
                        Feature::from_str("smcp")
                            .map_err(|_| failure("invalid small-caps feature"))?,
                    );
                    caps.as_slice()
                } else {
                    normal
                };
                let glyphs = rustybuzz::shape(&face, features, buffer);
                let mut advance = 0i64;
                let mut y_advance = 0i64;
                let mut run = runs.as_ref().map(|_| GlyphRun {
                    id,
                    size: requested.size as f32,
                    embolden,
                    skew_x,
                    glyphs: Vec::with_capacity(glyphs.len()),
                    positions: Vec::with_capacity(glyphs.len()),
                });
                let total_advance: i64 = glyphs
                    .glyph_positions()
                    .iter()
                    .map(|position| i64::from(position.x_advance))
                    .sum();
                let origin = metrics.width - (total_advance.min(0) as f64) * scale;
                for (info, position) in glyphs.glyph_infos().iter().zip(glyphs.glyph_positions()) {
                    if let Some(run) = &mut run {
                        run.glyphs.push(info.glyph_id as u16);
                        run.positions.push(Point::new(
                            (origin + (advance + i64::from(position.x_offset)) as f64 * scale)
                                as f32,
                            (-(y_advance + i64::from(position.y_offset)) as f64 * scale) as f32,
                        ));
                    }
                    y_advance += i64::from(position.y_advance);
                    advance += i64::from(position.x_advance);
                    if let Some(bounds) = face
                        .glyph_bounding_box(rustybuzz::ttf_parser::GlyphId(info.glyph_id as u16))
                    {
                        metrics.ascent = metrics
                            .ascent
                            .max(f64::from(i32::from(bounds.y_max) + position.y_offset) * scale);
                        metrics.descent = metrics
                            .descent
                            .max(-f64::from(i32::from(bounds.y_min) + position.y_offset) * scale);
                    }
                }
                metrics.width += (advance as f64).abs() * scale;
                if let (Some(runs), Some(run)) = (runs, run) {
                    runs.push(run);
                }
                Ok(())
            })
            .ok_or_else(|| failure("font bytes are unavailable"))?
    }

    fn raster(&mut self, id: u32) -> HostResult<&mut Raster> {
        self.surfaces
            .get_mut(&id)
            .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))
    }

    pub fn begin_terminal_graphics(&mut self) -> HostResult<()> {
        #[cfg(target_os = "macos")]
        {
            if self.metal.is_none() {
                self.metal = Some(MetalBackend::new()?);
            }
            self.terminal_graphics_active = true;
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(failure(
                "GPU terminal graphics are unsupported on this platform",
            ))
        }
    }

    pub fn end_terminal_graphics(&mut self) {
        // Do not drop Metal here: existing Ganesh surfaces retain GPU resources
        // and must outlive the DirectContext. This changes only future creates.
        #[cfg(target_os = "macos")]
        {
            self.terminal_graphics_active = false;
        }
    }

    pub fn create(&mut self, width: i32, height: i32) -> HostResult<u32> {
        rgba_size(width, height)?;
        let id = self.next_surface;
        let next = id
            .checked_add(1)
            .ok_or_else(|| failure("canvas handles exhausted"))?;
        #[cfg(target_os = "macos")]
        let (mut surface, backend) = if self.terminal_graphics_active {
            let info = ImageInfo::new(
                (width, height),
                ColorType::RGBA8888,
                AlphaType::Premul,
                None,
            );
            let metal = self
                .metal
                .as_mut()
                .ok_or_else(|| failure("Metal terminal graphics backend is not initialized"))?;
            let surface = gpu::surfaces::render_target(
                &mut metal.context,
                gpu::Budgeted::Yes,
                &info,
                0usize,
                gpu::SurfaceOrigin::TopLeft,
                None,
                false,
                false,
            )
            .ok_or_else(|| failure(format!("could not allocate {width}x{height} Metal canvas")))?;
            (surface, SurfaceBackend::Metal)
        } else {
            (
                surfaces::raster_n32_premul((width, height)).ok_or_else(|| {
                    failure(format!("could not allocate {width}x{height} canvas"))
                })?,
                SurfaceBackend::Cpu,
            )
        };
        #[cfg(not(target_os = "macos"))]
        let (mut surface, backend) = (
            surfaces::raster_n32_premul((width, height))
                .ok_or_else(|| failure(format!("could not allocate {width}x{height} canvas")))?,
            SurfaceBackend::Cpu,
        );
        // Preserve an untouched root clip so reset can discard every user clip.
        surface.canvas().save();
        self.surfaces.insert(
            id,
            Raster {
                surface,
                backend,
                style: DrawStyle::default(),
                saved: Vec::new(),
            },
        );
        self.next_surface = next;
        Ok(id)
    }

    pub fn dimensions(&self, id: u32) -> HostResult<(i32, i32)> {
        let raster = self
            .surfaces
            .get(&id)
            .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))?;
        Ok((raster.surface.width(), raster.surface.height()))
    }

    pub fn release(&mut self, id: u32) -> HostResult<()> {
        self.surfaces
            .remove(&id)
            .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))?;
        Ok(())
    }

    pub fn reset(&mut self, id: u32) -> HostResult<()> {
        let raster = self.raster(id)?;
        let canvas = raster.surface.canvas();
        canvas.restore_to_count(1);
        canvas.reset_matrix();
        canvas.clear(skia_safe::Color::TRANSPARENT);
        canvas.save();
        raster.style = DrawStyle::default();
        raster.saved.clear();
        Ok(())
    }

    pub fn clear(&mut self, id: u32, color: Color) -> HostResult<()> {
        self.raster(id)?.surface.canvas().clear(skia_color(color));
        Ok(())
    }

    pub fn save(&mut self, id: u32) -> HostResult<()> {
        let raster = self.raster(id)?;
        raster.saved.push(raster.style.clone());
        raster.surface.canvas().save();
        Ok(())
    }

    pub fn restore(&mut self, id: u32) -> HostResult<()> {
        let raster = self.raster(id)?;
        raster.style = raster
            .saved
            .pop()
            .ok_or_else(|| failure("canvas restore without matching save"))?;
        raster.surface.canvas().restore();
        Ok(())
    }

    pub fn set_transform(&mut self, id: u32, matrix: [f32; 6]) -> HostResult<()> {
        let matrix = affine(matrix)?;
        self.raster(id)?.surface.canvas().set_matrix(&matrix.into());
        Ok(())
    }

    pub fn transform(&mut self, id: u32, matrix: [f32; 6]) -> HostResult<()> {
        let matrix = affine(matrix)?;
        self.raster(id)?.surface.canvas().concat(&matrix);
        Ok(())
    }

    pub fn clip(&mut self, id: u32, path: &Path, even_odd: bool) -> HostResult<()> {
        // SkPath clones share their immutable point/verb storage.
        let mut clip = path.clone();
        clip.set_fill_type(if even_odd {
            PathFillType::EvenOdd
        } else {
            PathFillType::Winding
        });
        self.raster(id)?
            .surface
            .canvas()
            .clip_path(&clip, ClipOp::Intersect, true);
        Ok(())
    }

    pub fn set_blend(&mut self, id: u32, mode: &str) -> HostResult<()> {
        let blend = match mode {
            "source-over" => BlendMode::SrcOver,
            "difference" => BlendMode::Difference,
            "destination-out" => BlendMode::DstOut,
            "destination-in" => BlendMode::DstIn,
            "source-in" => BlendMode::SrcIn,
            "copy" => BlendMode::Src,
            _ => return Err(failure(format!("unknown canvas blend mode: {mode}"))),
        };
        self.raster(id)?.style.blend = blend;
        Ok(())
    }

    pub fn set_blur(&mut self, id: u32, radius: f32) -> HostResult<()> {
        nonnegative(radius, "blur radius")?;
        self.raster(id)?.style.blur = radius;
        Ok(())
    }

    pub fn set_tint(&mut self, id: u32, tint: Option<Color>) -> HostResult<()> {
        let filter = tint
            .map(|color| {
                color_filters::blend(skia_color(color), BlendMode::SrcIn)
                    .ok_or_else(|| failure("could not create canvas tint"))
            })
            .transpose()?;
        self.raster(id)?.style.tint = filter;
        Ok(())
    }

    pub fn fill_path(&mut self, id: u32, path: &Path, color: Color) -> HostResult<()> {
        self.raster(id)?.draw(|canvas, mut paint| {
            paint.set_color(skia_color(color));
            canvas.draw_path(path, &paint);
        })
    }

    pub fn stroke_path(&mut self, id: u32, path: &Path, stroke: Stroke) -> HostResult<()> {
        let (join, cap) = stroke_style(&stroke)?;
        self.raster(id)?.draw(|canvas, mut paint| {
            set_stroke(&mut paint, &stroke, join, cap);
            canvas.draw_path(path, &paint);
        })
    }

    pub fn fill_stroke_path(
        &mut self,
        id: u32,
        path: &Path,
        color: Color,
        stroke: Stroke,
    ) -> HostResult<()> {
        // Validate the entire operation before painting its first component.
        stroke_style(&stroke)?;
        self.fill_path(id, path, color)?;
        self.stroke_path(id, path, stroke)
    }

    pub fn text(
        &mut self,
        id: u32,
        font: &str,
        text: &str,
        x: f32,
        y: f32,
        color: Color,
        align: i32,
        baseline: i32,
    ) -> HostResult<()> {
        finite(&[x, y], "text position")?;
        self.raster(id)?;
        if !(0..=2).contains(&align) {
            return Err(failure(format!("unknown canvas text alignment: {align}")));
        }
        if !(0..=3).contains(&baseline) {
            return Err(failure(format!("unknown canvas text baseline: {baseline}")));
        }
        let mut runs = Vec::new();
        let (metrics, base, size) = self.shape_text(font, text, Some(&mut runs))?;
        let x = x - match align {
            1 => metrics.width as f32 / 2.0,
            2 => metrics.width as f32,
            _ => 0.0,
        };
        for id in std::iter::once(base).chain(runs.iter().map(|run| run.id)) {
            if !self.typefaces.contains_key(&id) {
                let face = self
                    .fonts
                    .with_face_data(id, |data, index| {
                        self.font_manager.new_from_data(Data::new_copy(data), index)
                    })
                    .flatten()
                    .ok_or_else(|| failure("Skia could not load the shaped font face"))?;
                self.typefaces.insert(id, face);
            }
        }
        let typefaces = &self.typefaces;
        // Canvas baselines belong to the font, not to the current word's ink.
        // Otherwise a reveal from "H" to "Hg" moves the already-visible H.
        let y = if baseline == 2 {
            y
        } else {
            let (_, line) = Font::from_typeface(typefaces[&base].clone(), size as f32).metrics();
            y + match baseline {
                0 => -line.ascent,
                1 => -(line.ascent + line.descent) / 2.0,
                _ => -line.descent,
            }
        };
        let raster = self
            .surfaces
            .get_mut(&id)
            .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))?;
        raster.draw(|canvas, mut paint| {
            paint.set_color(skia_color(color));
            for run in &runs {
                let mut font = Font::from_typeface(typefaces[&run.id].clone(), run.size);
                // Keep fractional Rustybuzz placement; no independent hinted advances.
                font.set_subpixel(true)
                    .set_hinting(FontHinting::None)
                    .set_embolden(run.embolden)
                    .set_skew_x(run.skew_x);
                canvas.draw_glyphs_at(&run.glyphs, run.positions.as_slice(), (x, y), &font, &paint);
            }
        })
    }

    pub fn circle_pattern(&mut self, id: u32, pattern: CirclePattern) -> HostResult<()> {
        finite(&pattern.rect, "pattern rectangle")?;
        finite(&pattern.origin, "pattern origin")?;
        nonnegative(pattern.radius, "pattern radius")?;
        nonnegative(pattern.rect[2], "pattern width")?;
        nonnegative(pattern.rect[3], "pattern height")?;
        if !pattern.tile.is_finite() || pattern.tile <= 0.0 {
            return Err(failure("pattern tile must be positive and finite"));
        }
        // Record just one periodic tile, not one FFI call or picture per screen dot.
        let half = pattern.tile / 2.0;
        let tile = Rect::from_xywh(-half, -half, pattern.tile, pattern.tile);
        let mut recorder = PictureRecorder::new();
        let canvas = recorder.begin_recording(tile, false);
        canvas.clear(skia_color(pattern.background));
        let mut paint = Paint::default();
        paint
            .set_anti_alias(true)
            .set_color(skia_color(pattern.ink));
        // Neighbouring circles must contribute when a radius crosses a tile edge.
        let reach = ((pattern.radius as f64 / pattern.tile as f64) + 0.5).floor();
        if reach > i32::MAX as f64 {
            return Err(failure(
                "pattern radius/tile ratio exceeds native coordinate range",
            ));
        }
        let reach = reach as i32;
        for row in -reach..=reach {
            for column in -reach..=reach {
                canvas.draw_circle(
                    (column as f32 * pattern.tile, row as f32 * pattern.tile),
                    pattern.radius,
                    &paint,
                );
            }
        }
        let picture = recorder
            .finish_recording_as_picture(None)
            .ok_or_else(|| failure("could not record circle tile"))?;
        let matrix = Matrix::translate((pattern.origin[0], pattern.origin[1]));
        let shader = picture.to_shader(
            (TileMode::Repeat, TileMode::Repeat),
            FilterMode::Linear,
            &matrix,
            &tile,
        );
        let [x, y, width, height] = pattern.rect;
        self.raster(id)?.draw(|canvas, mut paint| {
            paint.set_shader(shader);
            canvas.draw_rect(Rect::from_xywh(x, y, width, height), &paint);
        })
    }

    pub fn surface(&mut self, destination: u32, source: u32) -> HostResult<()> {
        self.raster(destination)?;
        // Raster snapshots share pixels until a writer needs copy-on-write. In
        // the normal distinct-surface case this draws without a full-frame copy.
        let image = self.raster(source)?.surface.image_snapshot();
        self.raster(destination)?.draw(|canvas, paint| {
            canvas.draw_image(&image, (0.0, 0.0), Some(&paint));
        })
    }

    pub fn png(&mut self, id: u32, path: &str) -> HostResult<()> {
        let backend = self
            .surfaces
            .get(&id)
            .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))?
            .backend;
        match backend {
            SurfaceBackend::Cpu => {
                let pixels = self
                    .raster(id)?
                    .surface
                    .peek_pixels()
                    .ok_or_else(|| failure("canvas raster pixels are unavailable"))?;
                let mut file = std::fs::File::create(path)?;
                if !skia_safe::png_encoder::encode(&pixels, &mut file, &Default::default()) {
                    return Err(failure(format!("could not encode canvas PNG: {path}")));
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            SurfaceBackend::Metal => {
                let image = self.raster(id)?.surface.image_snapshot();
                let data = self
                    .metal
                    .as_mut()
                    .ok_or_else(|| failure("Metal canvas lost its Ganesh context"))?
                    .encode(&image, &Default::default())?;
                let mut file = std::fs::File::create(path)?;
                std::io::Write::write_all(&mut file, data.as_bytes())?;
                Ok(())
            }
        }
    }

    pub fn encode_png(&mut self, id: u32, output: &mut Vec<u8>) -> HostResult<()> {
        let backend = self
            .surfaces
            .get(&id)
            .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))?
            .backend;
        // Streaming frames favor encode latency over the smallest file. PNG
        // remains lossless; avoid testing every filter and level-6 compression.
        let mut options = skia_safe::png_encoder::Options::default();
        options.filter_flags = skia_safe::png_encoder::FilterFlag::NONE;
        options.z_lib_level = 1;
        match backend {
            SurfaceBackend::Cpu => {
                let pixels = self
                    .raster(id)?
                    .surface
                    .peek_pixels()
                    .ok_or_else(|| failure("canvas raster pixels are unavailable"))?;
                output.clear();
                if !skia_safe::png_encoder::encode(&pixels, output, &options) {
                    return Err(failure("could not encode canvas PNG"));
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            SurfaceBackend::Metal => {
                let image = self.raster(id)?.surface.image_snapshot();
                let data = self
                    .metal
                    .as_mut()
                    .ok_or_else(|| failure("Metal canvas lost its Ganesh context"))?
                    .encode(&image, &options)?;
                output.clear();
                output.extend_from_slice(data.as_bytes());
                Ok(())
            }
        }
    }

    pub fn read_rgba(
        &mut self,
        id: u32,
        width: i32,
        height: i32,
        output: &mut [u8],
    ) -> HostResult<()> {
        let size = rgba_size(width, height)?;
        let (actual_width, actual_height, _backend) = {
            let raster = self
                .surfaces
                .get(&id)
                .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))?;
            (
                raster.surface.width(),
                raster.surface.height(),
                raster.backend,
            )
        };
        if actual_width != width || actual_height != height {
            return Err(failure(format!(
                "canvas is {actual_width}x{actual_height}, expected {width}x{height}",
            )));
        }
        if output.len() != size {
            return Err(failure(format!(
                "RGBA buffer has {} bytes, expected {size}",
                output.len()
            )));
        }
        #[cfg(target_os = "macos")]
        if _backend == SurfaceBackend::Metal {
            // Submit this render target. Synchronous read_pixels below waits for
            // its readback; a CPU wait here would introduce a second GPU stall.
            let (surfaces, metal) = (&mut self.surfaces, &mut self.metal);
            let raster = surfaces
                .get_mut(&id)
                .ok_or_else(|| failure(format!("unknown canvas surface: {id}")))?;
            let metal = metal
                .as_mut()
                .ok_or_else(|| failure("Metal canvas lost its Ganesh context"))?;
            metal
                .context
                .flush_and_submit_surface(&mut raster.surface, gpu::SyncCpu::No);
            if metal.context.is_device_lost() || metal.context.oomed() {
                return Err(failure("Metal canvas became unavailable during readback"));
            }
        }
        let info = ImageInfo::new(
            (width, height),
            ColorType::RGBA8888,
            AlphaType::Unpremul,
            None,
        );
        if !self
            .surfaces
            .get_mut(&id)
            .expect("canvas was validated above")
            .surface
            .read_pixels(&info, output, width as usize * 4, (0, 0))
        {
            return Err(failure("could not read straight RGBA canvas pixels"));
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod metal_tests {
    use super::*;

    #[test]
    fn terminal_canvases_are_metal_backed_and_read_straight_rgba() {
        let mut graphics = Graphics::new(&[]).expect("system fonts are available");
        graphics
            .begin_terminal_graphics()
            .expect("Metal Ganesh initializes");
        let gpu = graphics.create(1, 1).expect("Metal surface allocates");
        assert!(
            graphics
                .raster(gpu)
                .expect("surface exists")
                .surface
                .image_snapshot()
                .is_texture_backed(),
            "terminal surface must be a Ganesh texture, not a CPU raster"
        );

        graphics
            .clear(gpu, [37, 73, 109, 127])
            .expect("Metal canvas clears");
        let mut rgba = [0; 4];
        graphics
            .read_rgba(gpu, 1, 1, &mut rgba)
            .expect("GPU readback succeeds");
        assert_eq!(rgba[3], 127, "readback preserves straight alpha");
        for (actual, expected) in rgba[..3].iter().zip([37, 73, 109]) {
            assert!(
                actual.abs_diff(expected) <= 1,
                "straight RGB changed from {expected} to {actual}"
            );
        }

        graphics.end_terminal_graphics();
        let cpu = graphics.create(1, 1).expect("CPU surface allocates");
        assert!(
            !graphics
                .raster(cpu)
                .expect("surface exists")
                .surface
                .image_snapshot()
                .is_texture_backed(),
            "ending terminal graphics returns subsequent exports to CPU"
        );
    }
}

impl Default for DrawStyle {
    fn default() -> Self {
        Self {
            blend: BlendMode::SrcOver,
            blur: 0.0,
            tint: None,
        }
    }
}

impl Raster {
    fn draw(&mut self, draw: impl FnOnce(&Canvas, Paint)) -> HostResult<()> {
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_blend_mode(self.style.blend);
        paint.set_color_filter(self.style.tint.clone());
        let canvas = self.surface.canvas();
        if self.style.blur == 0.0 {
            draw(canvas, paint);
        } else {
            let filter = image_filters::blur(
                (self.style.blur, self.style.blur),
                TileMode::Decal,
                None,
                None,
            )
            .ok_or_else(|| failure("could not create canvas blur"))?;
            paint.set_image_filter(filter);
            // Establish the filter layer in device space, then restore the
            // geometry transform inside it. Sigma never inherits a zoom/shear.
            let matrix = canvas.local_to_device();
            let count = canvas.save();
            canvas.reset_matrix();
            canvas.save_layer(&SaveLayerRec::default().paint(&paint));
            canvas.set_matrix(&matrix);
            let mut content = Paint::default();
            content.set_anti_alias(true);
            draw(canvas, content);
            canvas.restore_to_count(count);
        }
        Ok(())
    }
}

pub(crate) fn path_from_values(
    mut values: impl Iterator<Item = HostResult<f64>>,
) -> HostResult<Path> {
    fn coordinate(values: &mut impl Iterator<Item = HostResult<f64>>) -> HostResult<f32> {
        let value = values
            .next()
            .ok_or_else(|| failure("truncated canvas path command"))??;
        let coordinate = value as f32;
        if !coordinate.is_finite() {
            return Err(failure(
                "canvas path coordinates must be finite native numbers",
            ));
        }
        Ok(coordinate)
    }
    fn point(values: &mut impl Iterator<Item = HostResult<f64>>) -> HostResult<Point> {
        Ok(Point::new(coordinate(values)?, coordinate(values)?))
    }
    let mut builder = PathBuilder::new();
    let mut has_move = false;
    while let Some(tag) = values.next() {
        let tag = tag?;
        if tag != 1.0 && !has_move && (2.0..=5.0).contains(&tag) {
            return Err(failure("canvas path must begin with move-to"));
        }
        match tag {
            1.0 => {
                builder.move_to(point(&mut values)?);
                has_move = true;
            }
            2.0 => {
                builder.line_to(point(&mut values)?);
            }
            3.0 => {
                let control = point(&mut values)?;
                builder.quad_to(control, point(&mut values)?);
            }
            4.0 => {
                let first = point(&mut values)?;
                let second = point(&mut values)?;
                builder.cubic_to(first, second, point(&mut values)?);
            }
            5.0 => {
                builder.close();
            }
            _ => return Err(failure(format!("unknown canvas path command: {tag}"))),
        }
    }
    Ok(builder.detach())
}

fn skia_color([r, g, b, a]: Color) -> skia_safe::Color {
    skia_safe::Color::from_argb(a, r, g, b)
}

fn finite(values: &[f32], name: &str) -> HostResult<()> {
    if values.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(failure(format!(
            "{name} must contain finite native numbers"
        )))
    }
}

fn nonnegative(value: f32, name: &str) -> HostResult<()> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(failure(format!("{name} must be finite and nonnegative")))
    }
}

fn affine([a, b, c, d, e, f]: [f32; 6]) -> HostResult<Matrix> {
    finite(&[a, b, c, d, e, f], "canvas transform")?;
    Ok(Matrix::new_all(a, c, e, b, d, f, 0.0, 0.0, 1.0))
}

fn stroke_style(stroke: &Stroke) -> HostResult<(paint::Join, paint::Cap)> {
    nonnegative(stroke.width, "stroke width")?;
    let join = match stroke.join {
        0 => paint::Join::Round,
        1 => paint::Join::Bevel,
        2 => paint::Join::Miter,
        value => return Err(failure(format!("unknown canvas stroke join: {value}"))),
    };
    let cap = match stroke.cap {
        0 => paint::Cap::Butt,
        1 => paint::Cap::Round,
        2 => paint::Cap::Square,
        value => return Err(failure(format!("unknown canvas stroke cap: {value}"))),
    };
    Ok((join, cap))
}

fn set_stroke(paint: &mut Paint, stroke: &Stroke, join: paint::Join, cap: paint::Cap) {
    paint.set_color(skia_color(stroke.color));
    paint.set_style(PaintStyle::Stroke);
    paint.set_stroke_width(stroke.width);
    paint.set_stroke_join(join);
    paint.set_stroke_cap(cap);
    paint.set_stroke_miter(4.0);
}

fn rgba_size(width: i32, height: i32) -> HostResult<usize> {
    if width <= 0 || height <= 0 {
        return Err(failure("canvas dimensions must be positive"));
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|size| size.checked_mul(4))
        .ok_or_else(|| failure("canvas dimensions overflow the RGBA buffer size"))
}

fn font_size(value: &str) -> HostResult<f64> {
    let length = svgtypes::Length::from_str(value)?;
    let scale = match length.unit {
        svgtypes::LengthUnit::None | svgtypes::LengthUnit::Px => 1.0,
        svgtypes::LengthUnit::Pt => 96.0 / 72.0,
        svgtypes::LengthUnit::Pc => 16.0,
        svgtypes::LengthUnit::In => 96.0,
        svgtypes::LengthUnit::Cm => 96.0 / 2.54,
        svgtypes::LengthUnit::Mm => 96.0 / 25.4,
        _ => {
            return Err(failure(
                "native font measurements require an absolute CSS font size",
            ))
        }
    };
    let size = length.number * scale;
    if size > 0.0 && (size as f32).is_finite() && (size as f32) > 0.0 {
        Ok(size)
    } else {
        Err(failure(
            "font size must be positive and finite in native coordinates",
        ))
    }
}
