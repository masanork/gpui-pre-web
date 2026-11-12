// Also modified by ibis inc.: Japanese text no named family covers falls to a gothic face (`GOTHIC_FAMILIES`) before the generic `sans-serif`.
// Also modified by ibis inc. (bold and italic asked of a family whose loaded face is regular: `font_id` returns an alias of the
// face, the browser draws the text of that alias with the requested weight and style).
// Also modified by ibis inc.: a CJK character no requested family covers is drawn by the browser, never in an unrelated loaded face (K0017).
// Also modified by ibis inc.: `font_data`, `synthetic_style` and `font_variations` are the native text system's.
use crate::canvas_fallback::{
    CanvasFontFallback, classify_canvas_fallback, is_latin_grapheme, names_serif_family,
    requested_family_names,
};
use crate::canvas_text::{self, CanvasTextMetrics};
use crate::glyph_cache::{CanvasGlyph, GlyphCache};
use crate::run_replacements::{
    Replacement, apply_replacements, collect_candidates, demote_implicit_cjk,
};
use anyhow::{Context as _, Result, ensure};
use gpui::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, FontStyle, GlyphId, Hsla, LineLayout,
    MissingGlyphSink, Pixels, PlatformTextSystem, RenderGlyphParams, SUBPIXEL_VARIANTS_X,
    SUBPIXEL_VARIANTS_Y, ShapedGlyph, Size, TextRenderingMode, point, px, size,
};
use gpui_wgpu::{CosmicTextSystem, FaceClass};
use parking_lot::RwLock;
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, OnceLock},
};

// Cosmic's font IDs index its loaded-font vector. Keep browser IDs in a disjoint namespace.
const CANVAS_FONT_BIT: usize = 1 << (usize::BITS - 1);
const MAX_MEASUREMENTS: usize = 4096;

pub(crate) struct WebTextSystem {
    native: CosmicTextSystem,
    state: RwLock<State>,
    canvas_font_fallback: CanvasFontFallback,
}

#[derive(Default)]
struct State {
    missing_glyph_sink: Option<Arc<dyn MissingGlyphSink>>,
    descriptors: HashMap<FontId, Font>,
    /// Serif/sans class of each native face (read from its font file once; K0017).
    face_classes: HashMap<FontId, Option<FaceClass>>,
    canvas_fonts: Vec<Arc<CanvasFont>>,
    canvas_font_ids: HashMap<FontId, FontId>,
    glyphs: GlyphCache,
    /// (native font, requested weight, requested style) to the alias ID.
    aliases: HashMap<(FontId, u32, u8), FontId>,
    measurements: HashMap<(FontId, GlyphId, Pixels), CanvasTextMetrics>,
}

struct CanvasFont {
    native_id: FontId,
    descriptor: Font,
    monospace: bool,
    /// The first typeface the document asks for (or an alias of it) that the
    /// browser has installed: half-width Latin text is drawn with it (ibis).
    latin_family: OnceLock<Option<String>>,
    /// The font is the browser's drawing of a face whose requested bold or italic the face does not have: all its Latin text
    /// is drawn by the browser (ibis).
    force_browser: bool,
    /// An alias is the ID `font_id` returns for such a request. It behaves as the native face everywhere but in `layout_line`.
    alias: bool,
}

impl State {
    fn cached_fallback(
        &self,
        native_id: FontId,
        text: &str,
        color: bool,
        font_size: Pixels,
    ) -> Option<(FontId, GlyphId, CanvasTextMetrics)> {
        let font_id = *self.canvas_font_ids.get(&native_id)?;
        let glyph_id = self.glyphs.id_for(font_id, text, color)?;
        let metrics = *self.measurements.get(&(font_id, glyph_id, font_size))?;
        Some((font_id, glyph_id, metrics))
    }
}

impl WebTextSystem {
    pub(crate) fn new(
        system_font_fallback: &str,
        canvas_font_fallback: CanvasFontFallback,
    ) -> Self {
        Self {
            native: CosmicTextSystem::new_without_system_fonts(system_font_fallback),
            state: RwLock::new(State::default()),
            canvas_font_fallback,
        }
    }

    fn face_class(&self, face: FontId) -> Option<FaceClass> {
        if let Some(class) = self.state.read().face_classes.get(&face) {
            return *class;
        }
        let class = self.native.face_class(face);
        self.state.write().face_classes.insert(face, class);
        class
    }

    /// The class the request wants: serif when the descriptor names a serif family, else that of the loaded face.
    fn requested_class(&self, requested: FontId) -> Option<FaceClass> {
        let serif_named = self.state.read().descriptors.get(&requested).is_some_and(|font| {
            descriptor_names_serif(font)
        });
        if serif_named {
            Some(FaceClass::Serif)
        } else {
            self.face_class(requested)
        }
    }

    fn canvas_font(&self, font_id: FontId) -> Option<Arc<CanvasFont>> {
        if font_id.0 & CANVAS_FONT_BIT == 0 {
            return None;
        }
        self.state
            .read()
            .canvas_fonts
            .get(font_id.0 & !CANVAS_FONT_BIT)
            .filter(|font| !font.alias)
            .cloned()
    }

    fn native_font_id(&self, font_id: FontId) -> FontId {
        if font_id.0 & CANVAS_FONT_BIT == 0 {
            return font_id;
        }
        self.state
            .read()
            .canvas_fonts
            .get(font_id.0 & !CANVAS_FONT_BIT)
            .map_or(font_id, |font| font.native_id)
    }

    fn is_monospace(&self, native_id: FontId) -> bool {
        match (
            self.native.glyph_for_char(native_id, 'i'),
            self.native.glyph_for_char(native_id, 'm'),
        ) {
            (Some(narrow), Some(wide)) => {
                self.native.advance(native_id, narrow).map(|s| s.width).ok()
                    == self.native.advance(native_id, wide).map(|s| s.width).ok()
            }
            _ => false,
        }
    }

    /// The alias ID for `native_id` drawn at the requested weight and style (ibis).
    fn alias_font_id(&self, native_id: FontId, descriptor: Font) -> Result<FontId> {
        let key = (
            native_id,
            descriptor.weight.0 as u32,
            descriptor.style as u8,
        );
        if let Some(font_id) = self.state.read().aliases.get(&key) {
            return Ok(*font_id);
        }
        let monospace = self.is_monospace(native_id);
        let mut state = self.state.write();
        if let Some(font_id) = state.aliases.get(&key) {
            return Ok(*font_id);
        }
        ensure!(
            state.canvas_fonts.len() + 2 < CANVAS_FONT_BIT,
            "Canvas font ID space exhausted"
        );
        let browser = FontId(CANVAS_FONT_BIT | state.canvas_fonts.len());
        state.canvas_fonts.push(Arc::new(CanvasFont {
            native_id,
            descriptor: descriptor.clone(),
            monospace,
            latin_family: OnceLock::new(),
            force_browser: true,
            alias: false,
        }));
        let alias = FontId(CANVAS_FONT_BIT | state.canvas_fonts.len());
        state.canvas_fonts.push(Arc::new(CanvasFont {
            native_id,
            descriptor,
            monospace,
            latin_family: OnceLock::new(),
            force_browser: false,
            alias: true,
        }));
        state.canvas_font_ids.insert(alias, browser);
        state.aliases.insert(key, alias);
        Ok(alias)
    }

    fn canvas_font_id(&self, native_id: FontId) -> Result<FontId> {
        let descriptor = {
            let state = self.state.read();
            if let Some(font_id) = state.canvas_font_ids.get(&native_id) {
                return Ok(*font_id);
            }
            state
                .descriptors
                .get(&native_id)
                .cloned()
                .context("missing source font descriptor for Canvas fallback")?
        };
        let monospace = self.is_monospace(native_id);
        let mut state = self.state.write();
        if let Some(font_id) = state.canvas_font_ids.get(&native_id) {
            return Ok(*font_id);
        }
        ensure!(
            state.canvas_fonts.len() < CANVAS_FONT_BIT,
            "Canvas font ID space exhausted"
        );
        let font_id = FontId(CANVAS_FONT_BIT | state.canvas_fonts.len());
        state.canvas_fonts.push(Arc::new(CanvasFont {
            native_id,
            descriptor,
            monospace,
            latin_family: OnceLock::new(),
            force_browser: false,
            alias: false,
        }));
        state.canvas_font_ids.insert(native_id, font_id);
        Ok(font_id)
    }

    /// Whether Latin text in the native font `native_id` is drawn by the browser,
    /// because the typeface its document names is installed there.
    fn browser_draws_latin(&self, native_id: FontId) -> bool {
        let has_names = native_id.0 & CANVAS_FONT_BIT != 0
            || self
                .state
                .read()
                .descriptors
                .get(&native_id)
                .is_some_and(|font| font.fallbacks.is_some());
        has_names
            && self
                .canvas_font_id(native_id)
                .ok()
                .and_then(|font_id| self.canvas_font(font_id))
                .is_some_and(|font| font.force_browser || font.latin_family().is_some())
    }

    fn register_glyph(&self, font_id: FontId, text: &str, color: bool) -> Result<GlyphId> {
        if let Some(glyph_id) = self.state.read().glyphs.id_for(font_id, text, color) {
            return Ok(glyph_id);
        }
        // Another thread may register the same glyph between the read and write locks.
        self.state
            .write()
            .glyphs
            .get_or_insert(font_id, text, color)
    }

    fn canvas_glyph(&self, font_id: FontId, glyph_id: GlyphId) -> Result<CanvasGlyph> {
        self.state
            .read()
            .glyphs
            .glyph(font_id, glyph_id)
            .cloned()
            .context("invalid Canvas glyph ID")
    }

    fn measure(
        &self,
        font_id: FontId,
        glyph_id: GlyphId,
        font_size: Pixels,
    ) -> Result<CanvasTextMetrics> {
        let key = (font_id, glyph_id, font_size);
        if let Some(metrics) = self.state.read().measurements.get(&key) {
            return Ok(*metrics);
        }
        let font = self
            .canvas_font(font_id)
            .context("invalid Canvas font ID")?;
        let glyph = self.canvas_glyph(font_id, glyph_id)?;
        let metrics = canvas_text::measure(&glyph.text, &font.css_font(font_size, glyph.color, &glyph.text)?)?;
        ensure!(metrics.advance >= 0.0, "Canvas returned a negative advance");
        let mut state = self.state.write();
        if state.measurements.len() >= MAX_MEASUREMENTS {
            state.measurements.clear();
        }
        state.measurements.insert(key, metrics);
        Ok(metrics)
    }

    fn fallback_glyph(
        &self,
        native_id: FontId,
        text: &str,
        color: bool,
        font_size: Pixels,
    ) -> Result<(FontId, GlyphId, CanvasTextMetrics)> {
        if let Some(cached) = self
            .state
            .read()
            .cached_fallback(native_id, text, color, font_size)
        {
            return Ok(cached);
        }
        let font_id = self.canvas_font_id(native_id)?;
        let glyph_id = self.register_glyph(font_id, text, color)?;
        let metrics = self.measure(font_id, glyph_id, font_size)?;
        Ok((font_id, glyph_id, metrics))
    }

    fn apply_fallback(&self, text: &str, font_runs: &[FontRun], layout: &mut LineLayout) {
        if self.canvas_font_fallback == CanvasFontFallback::Disabled
            || !f32::from(layout.font_size).is_finite()
            || layout.font_size <= px(0.)
        {
            return;
        }
        let mut replacements = Vec::new();
        let latin_allowed = self.canvas_font_fallback == CanvasFontFallback::EmojiAndCjk;
        let candidates = collect_candidates(text, font_runs, layout, |font_id| {
            latin_allowed && self.browser_draws_latin(font_id)
        });
        // A ligature ("fi") is one glyph for two graphemes: only the first has
        // a glyph of its own. Replacing it would drop the second letter.
        let mut ligated = vec![false; candidates.len()];
        for index in 1..candidates.len() {
            if candidates[index].latin
                && candidates[index].glyphs.is_none()
                && candidates[index - 1].source.end == candidates[index].source.start
            {
                ligated[index - 1] = true;
            }
        }
        for (index, candidate) in candidates.into_iter().enumerate() {
            if !self.canvas_font_fallback.allows(candidate.is_emoji) {
                continue;
            }
            let Some(glyphs) = candidate.glyphs else {
                continue;
            };
            if candidate.latin {
                if ligated[index] || glyphs.range.len() != 1 {
                    continue;
                }
            } else if !glyphs.contiguous
                || !(glyphs.missing || (candidate.color && !glyphs.native_color))
            {
                continue;
            }
            let old_width = glyphs.end_x - glyphs.position.x;
            if old_width < px(0.) {
                continue;
            }
            let replacement = (|| -> Result<Replacement> {
                let text = text
                    .get(candidate.source.clone())
                    .context("invalid Canvas fallback source range")?;
                let (font_id, glyph_id, metrics) = self.fallback_glyph(
                    candidate.font_id,
                    text,
                    candidate.color,
                    layout.font_size,
                )?;
                Ok(Replacement {
                    glyph_range: glyphs.range,
                    font_id,
                    glyph: ShapedGlyph {
                        id: glyph_id,
                        position: glyphs.position,
                        index: candidate.source.start,
                        is_emoji: candidate.color,
                    },
                    width_delta: px(metrics.advance) - old_width,
                })
            })();
            match replacement {
                Ok(replacement) => replacements.push(replacement),
                Err(error) => log::warn!("Canvas font fallback failed: {error:#}"),
            }
        }
        if replacements.is_empty() {
            return;
        }
        replacements.sort_unstable_by_key(|replacement| replacement.glyph_range.start);
        // Keep the primary font's baseline and line spacing stable. Browser ink
        // extents affect raster bounds, not the surrounding editor's line metrics.
        apply_replacements(layout, replacements);
    }
}

impl PlatformTextSystem for WebTextSystem {
    fn set_missing_glyph_sink(&self, sink: Option<Arc<dyn MissingGlyphSink>>) {
        // Cosmic must not report missing glyphs before Canvas has a chance to replace them.
        self.state.write().missing_glyph_sink = sink;
    }

    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.native.add_fonts(fonts)?;
        self.state.write().measurements.clear();
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        self.native.all_font_names()
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let font_id = self.native.font_id(font)?;
        ensure!(
            font_id.0 & CANVAS_FONT_BIT == 0,
            "native font ID space exhausted"
        );
        let mut descriptor = font.clone();
        let requested = descriptor.clone();
        (descriptor.weight, descriptor.style) = self.native.font_weight_and_style(font_id)?;
        let face_weight = descriptor.weight.0;
        let face_style = descriptor.style;
        self.state
            .write()
            .descriptors
            .entry(font_id)
            .or_insert(descriptor);
        // ibis: the face is the closest the loaded fonts have. A bold or italic it does not have is drawn by the browser
        // (Canvas takes the weight and style in its CSS font and picks a real face or synthesizes one).
        let needs_bold = requested.weight.0 >= 600. && face_weight < 600.;
        let needs_italic = requested.style != FontStyle::Normal && face_style == FontStyle::Normal;
        if needs_bold || needs_italic {
            return self.alias_font_id(font_id, requested);
        }
        Ok(font_id)
    }

    fn prewarm_fonts(&self, font_ids: &[FontId]) {
        if font_ids
            .iter()
            .all(|font_id| font_id.0 & CANVAS_FONT_BIT == 0)
        {
            return self.native.prewarm_fonts(font_ids);
        }
        let native_ids: Vec<_> = font_ids
            .iter()
            .map(|font_id| self.native_font_id(*font_id))
            .collect();
        self.native.prewarm_fonts(&native_ids);
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.native.font_metrics(self.native_font_id(font_id))
    }

    // ibis: the bytes of the fonts GPUI carries, for text drawn as glyph outlines (glow, gradient fills, PDF).
    // A font the browser draws (Canvas) has none: its glyph IDs are not the file's.
    fn font_data(&self, font_id: FontId) -> Option<(Arc<Vec<u8>>, u32)> {
        if self.canvas_font(font_id).is_some() {
            return None;
        }
        self.native.font_data(self.native_font_id(font_id))
    }

    fn synthetic_style(&self, font_id: FontId) -> (bool, bool) {
        if self.canvas_font(font_id).is_some() {
            return (false, false);
        }
        self.native.synthetic_style(self.native_font_id(font_id))
    }

    fn font_variations(&self, font_id: FontId) -> Vec<([u8; 4], f32)> {
        if self.canvas_font(font_id).is_some() {
            return Vec::new();
        }
        self.native.font_variations(self.native_font_id(font_id))
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let Some(font) = self.canvas_font(font_id) else {
            return self.native.typographic_bounds(self.native_font_id(font_id), glyph_id);
        };
        let font_size = px(self.native.font_metrics(font.native_id).units_per_em as f32);
        let metrics = self.measure(font_id, glyph_id, font_size)?;
        Ok(Bounds {
            origin: point(metrics.left, -metrics.ascent),
            size: size(
                metrics.right - metrics.left,
                metrics.ascent + metrics.descent,
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let Some(font) = self.canvas_font(font_id) else {
            return self.native.advance(self.native_font_id(font_id), glyph_id);
        };
        let font_size = px(self.native.font_metrics(font.native_id).units_per_em as f32);
        Ok(size(
            self.measure(font_id, glyph_id, font_size)?.advance,
            0.,
        ))
    }

    fn glyph_for_char(&self, font_id: FontId, character: char) -> Option<GlyphId> {
        if self.canvas_font(font_id).is_none() {
            return self.native.glyph_for_char(self.native_font_id(font_id), character);
        }
        let mut buffer = [0; 4];
        let text = character.encode_utf8(&mut buffer);
        let color =
            classify_canvas_fallback(text).is_some_and(|fallback| fallback.emoji_presentation);
        match self.register_glyph(font_id, text, color) {
            Ok(glyph_id) => Some(glyph_id),
            Err(error) => {
                log::warn!("registering Canvas glyph failed: {error:#}");
                None
            }
        }
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        if self.canvas_font(params.font_id).is_none() {
            return self.native.glyph_raster_bounds(params);
        }
        let metrics = self.measure(
            params.font_id,
            params.glyph_id,
            params.font_size * params.scale_factor,
        )?;
        if metrics.right <= metrics.left || metrics.ascent + metrics.descent <= 0. {
            return Ok(Bounds::default());
        }
        let (offset_x, offset_y) = subpixel_offset(params);
        let left = (f64::from(metrics.left) + f64::from(offset_x)).floor() - 1.;
        let top = (-f64::from(metrics.ascent) + f64::from(offset_y)).floor() - 1.;
        let right = (f64::from(metrics.right) + f64::from(offset_x)).ceil() + 1.;
        let bottom = (f64::from(metrics.descent) + f64::from(offset_y)).ceil() + 1.;
        ensure!(
            [left, top, right, bottom]
                .into_iter()
                .all(|value| value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX)),
            "Canvas text bounds exceed device coordinates"
        );
        let width = (right as i32)
            .checked_sub(left as i32)
            .context("Canvas text width overflow")?;
        let height = (bottom as i32)
            .checked_sub(top as i32)
            .context("Canvas text height overflow")?;
        Ok(Bounds {
            origin: point((left as i32).into(), (top as i32).into()),
            size: size(width.into(), height.into()),
        })
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        let Some(font) = self.canvas_font(params.font_id) else {
            return self.native.rasterize_glyph(params, bounds);
        };
        let glyph = self.canvas_glyph(params.font_id, params.glyph_id)?;
        let css_font = font.css_font(
            params.font_size * params.scale_factor,
            glyph.color,
            &glyph.text,
        )?;
        let mut pixels = canvas_text::rasterize(
            &glyph.text,
            &css_font,
            bounds,
            subpixel_offset(params),
            glyph.color,
        )?;
        if params.subpixel_rendering && !glyph.color {
            pixels = pixels.into_iter().flat_map(|alpha| [alpha; 4]).collect();
        }
        Ok((bounds.size, pixels))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let native_runs: Cow<'_, [FontRun]> =
            if runs.iter().any(|run| run.font_id.0 & CANVAS_FONT_BIT != 0) {
                Cow::Owned(
                    runs.iter()
                        .map(|run| FontRun {
                            len: run.len,
                            font_id: self.native_font_id(run.font_id),
                        })
                        .collect(),
                )
            } else {
                Cow::Borrowed(runs)
            };
        let mut layout = self.native.layout_line(text, font_size, &native_runs);
        // ibis (K0017): a CJK character no requested family covers goes to the browser, not to any face the native system holds.
        if self.canvas_font_fallback.allows(false) {
            // Rule: a face the request did not name is used only when it is of the same class (serif/sans) as the request.
            demote_implicit_cjk(text, &native_runs, &mut layout, |requested, face| {
                self.native.face_in_requested_families(requested, face)
                    || matches!(
                        (self.requested_class(requested), self.face_class(face)),
                        (Some(wanted), Some(got)) if wanted == got
                    )
            });
        }
        self.apply_fallback(text, runs, &mut layout);
        let sink = self.state.read().missing_glyph_sink.clone();
        if let Some(sink) = sink {
            let missing_indices = layout
                .runs
                .iter()
                .filter(|run| run.font_id.0 & CANVAS_FONT_BIT == 0)
                .flat_map(|run| &run.glyphs)
                .filter(|glyph| glyph.id.0 == 0)
                .map(|glyph| glyph.index);
            sink.report(
                self.native
                    .missing_glyphs(text, &native_runs, missing_indices),
            );
        }
        layout
    }

    fn recommended_rendering_mode(&self, font_id: FontId, font_size: Pixels) -> TextRenderingMode {
        if self.canvas_font(font_id).is_some() {
            TextRenderingMode::Grayscale
        } else {
            self.native.recommended_rendering_mode(self.native_font_id(font_id), font_size)
        }
    }

    fn glyph_dilation_for_color(&self, color: Hsla) -> u8 {
        self.native.glyph_dilation_for_color(color)
    }
}

fn descriptor_names_serif(font: &Font) -> bool {
    names_serif_family(&font.family)
        || font.fallbacks.as_ref().is_some_and(|fallbacks| {
            requested_family_names(fallbacks.fallback_list()).any(names_serif_family)
        })
}

impl CanvasFont {
    fn latin_family(&self) -> Option<&str> {
        self.latin_family
            .get_or_init(|| {
                let fallbacks = self.descriptor.fallbacks.as_ref()?;
                requested_family_names(fallbacks.fallback_list())
                    .find(|name| canvas_text::family_installed(name))
                    .map(str::to_owned)
            })
            .as_deref()
    }

    /// The CSS font for `text`. Half-width Latin text puts the installed
    /// typeface the document names first, so it and its measurement agree.
    fn css_font(&self, font_size: Pixels, color: bool, text: &str) -> Result<String> {
        let font_size = f32::from(font_size);
        ensure!(
            font_size.is_finite() && font_size > 0.,
            "invalid Canvas font size"
        );
        let style = match self.descriptor.style {
            FontStyle::Normal => "normal",
            FontStyle::Italic => "italic",
            FontStyle::Oblique => "oblique",
        };
        let mut families = Vec::new();
        if !color && is_latin_grapheme(text) {
            families.extend(self.latin_family().map(quote_family));
        }
        if !color && !self.descriptor.family.starts_with('.') {
            families.push(quote_family(&self.descriptor.family));
        }
        if let Some(fallbacks) = &self.descriptor.fallbacks {
            families.extend(
                fallbacks
                    .fallback_list()
                    .iter()
                    .filter(|family| !family.is_empty())
                    .map(|family| quote_family(family)),
            );
        }
        // ibis: a Japanese character no named family covers is drawn in a gothic face the viewer's system has (the Mac's Hiragino,
        // Windows' Yu Gothic UI / Meiryo UI, Linux's Noto Sans CJK JP), not in whatever the browser's bare `sans-serif` picks for the
        // page's language (a Chinese face or a mincho on some systems; K0017).
        // A request that names a serif family gets a mincho list and `serif`: the class of the request decides, so a gothic
        // request is never drawn in a mincho and a mincho request is never drawn in a gothic.
        let serif = !color && !self.monospace && descriptor_names_serif(&self.descriptor);
        if !color && !self.monospace {
            let list: &[&str] = if serif { &MINCHO_FAMILIES } else { &GOTHIC_FAMILIES };
            families.extend(list.iter().map(|family| quote_family(family)));
        }
        families.push(
            if color {
                "emoji"
            } else if self.monospace {
                "monospace"
            } else if serif {
                "serif"
            } else {
                "sans-serif"
            }
            .to_owned(),
        );
        Ok(format!(
            "{style} {} {font_size}px {}",
            self.descriptor.weight.0,
            families.join(", ")
        ))
    }
}

/// The gothic faces of the common systems, in the order tried (a family the system lacks is skipped by the browser).
const GOTHIC_FAMILIES: [&str; 8] = [
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Yu Gothic UI",
    "Meiryo UI",
    "Yu Gothic",
    "Noto Sans CJK JP",
    "Noto Sans JP",
    "IPAexGothic",
];

/// The mincho faces of the common systems, for a request that names a serif family.
const MINCHO_FAMILIES: [&str; 6] = [
    "Hiragino Mincho ProN",
    "Yu Mincho",
    "MS PMincho",
    "Noto Serif CJK JP",
    "Noto Serif JP",
    "IPAexMincho",
];

fn quote_family(family: &str) -> String {
    let mut quoted = String::from("\"");
    for character in family.chars() {
        match character {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(character);
            }
            character if character.is_control() => quoted.push(' '),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn subpixel_offset(params: &RenderGlyphParams) -> (f32, f32) {
    (
        f32::from(params.subpixel_variant.x) / SUBPIXEL_VARIANTS_X as f32,
        f32::from(params.subpixel_variant.y) / SUBPIXEL_VARIANTS_Y as f32,
    )
}
