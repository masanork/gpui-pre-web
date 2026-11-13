use crate::canvas_fallback::{classify_canvas_fallback, is_latin_grapheme};
use gpui::{FontId, FontRun, LineLayout, Pixels, Point, ShapedGlyph, ShapedRun};
use smallvec::SmallVec;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, PartialEq)]
pub(crate) struct Candidate {
    pub(crate) source: Range<usize>,
    pub(crate) font_id: FontId,
    pub(crate) is_emoji: bool,
    pub(crate) color: bool,
    /// Half-width Latin text of a font whose typeface the browser has (ibis).
    pub(crate) latin: bool,
    pub(crate) glyphs: Option<GlyphSpan>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct GlyphSpan {
    pub(crate) range: Range<usize>,
    pub(crate) position: Point<Pixels>,
    pub(crate) end_x: Pixels,
    pub(crate) contiguous: bool,
    pub(crate) missing: bool,
    pub(crate) native_color: bool,
}

pub(crate) fn collect_candidates(
    text: &str,
    font_runs: &[FontRun],
    layout: &LineLayout,
    browser_latin: impl Fn(FontId) -> bool,
) -> Vec<Candidate> {
    let mut font_runs = font_runs.iter().peekable();
    let mut font_run = font_runs.next();
    let mut run_end = font_run.map_or(0, |run| run.len);
    let mut candidates = Vec::new();
    for (start, grapheme) in text.grapheme_indices(true) {
        while start >= run_end {
            font_run = font_runs.next();
            let Some(run) = font_run else { break };
            run_end += run.len;
        }
        let Some(run) = font_run else { break };
        // Paint-only run boundaries must not split a fallback grapheme.
        while let Some(next_run) = font_runs.next_if(|next| next.font_id == run.font_id) {
            run_end += next_run.len;
        }
        let end = start + grapheme.len();
        if end > run_end {
            continue;
        }
        let Some(fallback) = classify_canvas_fallback(grapheme) else {
            if is_latin_grapheme(grapheme) && browser_latin(run.font_id) {
                candidates.push(Candidate {
                    source: start..end,
                    font_id: run.font_id,
                    is_emoji: false,
                    color: false,
                    latin: true,
                    glyphs: None,
                });
            }
            continue;
        };
        candidates.push(Candidate {
            source: start..end,
            font_id: run.font_id,
            is_emoji: fallback.is_emoji,
            color: fallback.emoji_presentation,
            latin: false,
            glyphs: None,
        });
    }
    if candidates.is_empty() {
        return candidates;
    }

    let mut glyphs = layout
        .runs
        .iter()
        .flat_map(|run| run.glyphs.iter())
        .enumerate()
        .peekable();
    while let Some((index, glyph)) = glyphs.next() {
        let candidate_index =
            candidates.partition_point(|candidate| candidate.source.end <= glyph.index);
        let Some(candidate) = candidates
            .get_mut(candidate_index)
            .filter(|candidate| candidate.source.contains(&glyph.index))
        else {
            continue;
        };
        let span = candidate.glyphs.get_or_insert_with(|| GlyphSpan {
            range: index..index,
            position: glyph.position,
            end_x: glyph.position.x,
            contiguous: true,
            missing: false,
            native_color: true,
        });
        span.contiguous &= span.range.end == index;
        span.range.end = index + 1;
        span.end_x = glyphs
            .peek()
            .map_or(layout.width, |(_, next)| next.position.x);
        span.missing |= glyph.id.0 == 0;
        span.native_color &= glyph.is_emoji;
    }
    candidates
}

/// ibis: a CJK character the requested families (the run's font and the fallbacks the descriptor names) lack is not drawn in
/// whatever other face the native text system happens to hold (cosmic-text's last-resort fallback takes any loaded face, e.g. a
/// downloaded IPAmj Mincho; K0017). Such a glyph is made a missing glyph (id 0), so the Canvas fallback draws it in the
/// browser's gothic face. A document that names the face keeps it (`in_requested(requested, face)` is true for it).
pub(crate) fn demote_implicit_cjk(
    text: &str,
    font_runs: &[FontRun],
    layout: &mut LineLayout,
    in_requested: impl Fn(FontId, FontId) -> bool,
) {
    for shaped in &mut layout.runs {
        let face = shaped.font_id;
        for glyph in &mut shaped.glyphs {
            if glyph.id.0 == 0 || glyph.is_emoji {
                continue;
            }
            let mut offset = 0;
            let Some(requested) = font_runs.iter().find_map(|run| {
                offset += run.len;
                (glyph.index < offset).then_some(run.font_id)
            }) else {
                continue;
            };
            if requested == face || in_requested(requested, face) {
                continue;
            }
            let Some(grapheme) = text
                .get(glyph.index..)
                .and_then(|rest| rest.graphemes(true).next())
            else {
                continue;
            };
            if classify_canvas_fallback(grapheme).is_some_and(|fallback| !fallback.is_emoji) {
                glyph.id = gpui::GlyphId(0);
            }
        }
    }
}

pub(crate) struct Replacement {
    pub(crate) glyph_range: Range<usize>,
    pub(crate) font_id: FontId,
    pub(crate) glyph: ShapedGlyph,
    pub(crate) width_delta: Pixels,
}

struct Segment {
    font_id: FontId,
    length: usize,
}

/// Ranges refer to the original flattened glyph stream and must be sorted,
/// non-overlapping, nonempty, and in bounds.
pub(crate) fn apply_replacements(layout: &mut LineLayout, replacements: Vec<Replacement>) {
    if replacements.is_empty() {
        return;
    }

    let mut replacements = replacements.into_iter().peekable();
    let mut original_index = 0;
    let mut consumed_end = 0;
    let mut shift = Pixels::ZERO;
    let mut segments_by_run: SmallVec<[SmallVec<[Segment; 4]>; 4]> = SmallVec::new();
    let mut spare_buffers: SmallVec<[Vec<ShapedGlyph>; 4]> = SmallVec::new();
    let mut final_run_count = 0;

    layout.runs.retain_mut(|run| {
        let mut segments: SmallVec<[Segment; 4]> = SmallVec::new();
        run.glyphs.retain_mut(|glyph| {
            let index = original_index;
            original_index += 1;
            if index < consumed_end {
                return false;
            }

            let font_id = if let Some(replacement) =
                replacements.next_if(|replacement| replacement.glyph_range.start == index)
            {
                consumed_end = replacement.glyph_range.end;
                *glyph = replacement.glyph;
                glyph.position.x += shift;
                shift += replacement.width_delta;
                replacement.font_id
            } else {
                glyph.position.x += shift;
                run.font_id
            };

            if let Some(segment) = segments.last_mut().filter(|last| last.font_id == font_id) {
                segment.length += 1;
            } else {
                segments.push(Segment { font_id, length: 1 });
            }
            true
        });

        if segments.is_empty() {
            spare_buffers.push(std::mem::take(&mut run.glyphs));
            false
        } else {
            final_run_count += segments.len();
            segments_by_run.push(segments);
            true
        }
    });
    layout.width += shift;

    let source_run_count = layout.runs.len();
    layout.runs.resize_with(final_run_count, || ShapedRun {
        font_id: FontId(0),
        glyphs: Vec::new(),
    });

    // Every destination is at or after its source. Expanding from the right
    // therefore cannot overwrite an original run that still needs processing.
    let mut destination_end = final_run_count;
    for (source_index, segments) in (0..source_run_count).zip(segments_by_run).rev() {
        let destination_start = destination_end - segments.len();
        let mut glyph_end = layout.runs[source_index].glyphs.len();
        for (segment_index, segment) in segments.into_iter().enumerate().rev() {
            let destination_index = destination_start + segment_index;
            if segment_index == 0 {
                layout.runs[source_index].font_id = segment.font_id;
                layout.runs.swap(source_index, destination_index);
            } else {
                let glyph_start = glyph_end - segment.length;
                let mut glyphs = spare_buffers.pop().unwrap_or_default();
                glyphs.extend(
                    layout.runs[source_index]
                        .glyphs
                        .drain(glyph_start..glyph_end),
                );
                layout.runs[destination_index] = ShapedRun {
                    font_id: segment.font_id,
                    glyphs,
                };
                glyph_end = glyph_start;
            }
        }
        destination_end = destination_start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context as _, Result};
    use gpui::{GlyphId, point, px};

    fn source_glyph(index: usize, x: f32, id: u32, color: bool) -> ShapedGlyph {
        ShapedGlyph {
            id: GlyphId(id),
            position: point(px(x), px(2.0)),
            index,
            is_emoji: color,
        }
    }

    #[test]
    fn implicit_cjk_fallback_becomes_missing_unless_requested() {
        let text = "a\u{6f22}\u{5b57}";
        let runs = [FontRun {
            len: text.len(),
            font_id: FontId(1),
        }];
        let build = || {
            let mut layout = layout(&[1, 0]);
            layout.runs[0].glyphs = vec![source_glyph(0, 0.0, 5, false)];
            layout.runs[1].font_id = FontId(9);
            layout.runs[1].glyphs = vec![
                source_glyph(1, 8.0, 7, false),
                source_glyph(4, 16.0, 8, false),
            ];
            layout
        };
        let ids = |layout: &LineLayout| -> Vec<u32> {
            layout
                .runs
                .iter()
                .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.id.0))
                .collect()
        };
        // The face FontId(9) is not one the run asked for: its CJK glyphs go to the Canvas fallback.
        let mut implicit = build();
        demote_implicit_cjk(text, &runs, &mut implicit, |_, _| false);
        assert_eq!(ids(&implicit), vec![5, 0, 0]);
        // The document names that face (or lists it as a fallback): it is kept.
        let mut named = build();
        demote_implicit_cjk(text, &runs, &mut named, |requested, face| {
            requested == FontId(1) && face == FontId(9)
        });
        assert_eq!(ids(&named), vec![5, 7, 8]);
    }

    #[test]
    fn candidates_capture_bounds_without_a_flattened_vector() -> Result<()> {
        let text = "中a文 ";
        let mut layout = layout(&[2, 0, 2]);
        layout.width = px(50.0);
        layout.runs[0].glyphs = vec![
            source_glyph(0, 0.0, 0, false),
            source_glyph(3, 10.0, 1, false),
        ];
        layout.runs[2].glyphs = vec![
            source_glyph(4, 25.0, 0, false),
            source_glyph(7, 40.0, 3, false),
        ];
        let candidates = collect_candidates(
            text,
            &[FontRun {
                len: text.len(),
                font_id: FontId(7),
            }],
            &layout,
            |_| false,
        );
        assert_eq!(candidates.len(), 2);
        let first = candidates.first().context("missing first candidate")?;
        assert_eq!(first.source, 0..3);
        assert_eq!(first.font_id, FontId(7));
        assert_eq!(
            first.glyphs,
            Some(GlyphSpan {
                range: 0..1,
                position: point(px(0.0), px(2.0)),
                end_x: px(10.0),
                contiguous: true,
                missing: true,
                native_color: false,
            })
        );
        let last = candidates.last().context("missing last candidate")?;
        assert_eq!(last.source, 4..7);
        let span = last.glyphs.as_ref().context("missing glyph span")?;
        assert_eq!(span.range, 2..3);
        assert_eq!(span.position.x, px(25.0));
        assert_eq!(span.end_x, px(40.0));
        Ok(())
    }

    #[test]
    fn latin_candidates_only_for_fonts_the_browser_has() {
        let text = "A1 あ";
        let mut layout = layout(&[1]);
        layout.runs[0].glyphs = vec![source_glyph(0, 0.0, 5, false)];
        let font_runs = [FontRun {
            len: text.len(),
            font_id: FontId(7),
        }];
        let off = collect_candidates(text, &font_runs, &layout, |_| false);
        assert_eq!(off.iter().filter(|c| c.latin).count(), 0);
        let on = collect_candidates(text, &font_runs, &layout, |font| font == FontId(7));
        let latin: Vec<_> = on.iter().filter(|c| c.latin).map(|c| c.source.clone()).collect();
        assert_eq!(latin, [0..1, 1..2, 2..3]);
        assert_eq!(on.len(), 4);
        let other = collect_candidates(text, &font_runs, &layout, |font| font == FontId(8));
        assert_eq!(other.iter().filter(|c| c.latin).count(), 0);
    }

    #[test]
    fn candidates_keep_graphemes_across_same_font_boundaries() -> Result<()> {
        let text = "か\u{3099}";
        let mut layout = layout(&[1, 1]);
        layout.runs[0].glyphs = vec![source_glyph(0, 0.0, 0, false)];
        layout.runs[1].glyphs = vec![source_glyph(3, 10.0, 0, false)];
        let mut font_runs = [
            FontRun {
                len: 3,
                font_id: FontId(7),
            },
            FontRun {
                len: 3,
                font_id: FontId(7),
            },
        ];
        let candidates = collect_candidates(text, &font_runs, &layout, |_| false);
        assert_eq!(candidates.len(), 1);
        let span = candidates
            .first()
            .and_then(|candidate| candidate.glyphs.as_ref())
            .context("missing glyph span")?;
        assert_eq!(span.range, 0..2);
        assert_eq!(span.position.x, px(0.0));
        assert_eq!(span.end_x, layout.width);
        assert!(span.contiguous);

        font_runs[1].font_id = FontId(8);
        assert!(collect_candidates(text, &font_runs, &layout, |_| false).is_empty());
        Ok(())
    }

    #[test]
    fn candidates_handle_nonmonotonic_and_disjoint_source_indices() -> Result<()> {
        let mut layout = layout(&[3]);
        layout.width = px(50.0);
        layout.runs[0].glyphs = vec![
            source_glyph(3, 0.0, 0, false),
            source_glyph(0, 20.0, 0, false),
            source_glyph(3, 30.0, 0, false),
        ];
        let candidates = collect_candidates(
            "中文",
            &[FontRun {
                len: 6,
                font_id: FontId(7),
            }],
            &layout,
            |_| false,
        );
        let first = candidates
            .first()
            .and_then(|candidate| candidate.glyphs.as_ref())
            .context("missing first span")?;
        assert_eq!(first.range, 1..2);
        assert_eq!(first.position.x, px(20.0));
        assert_eq!(first.end_x, px(30.0));
        assert!(first.contiguous);
        let last = candidates
            .last()
            .and_then(|candidate| candidate.glyphs.as_ref())
            .context("missing last span")?;
        assert_eq!(last.range, 0..3);
        assert_eq!(last.position.x, px(0.0));
        assert_eq!(last.end_x, layout.width);
        assert!(!last.contiguous);
        Ok(())
    }

    #[test]
    fn candidates_preserve_native_coverage_and_color() -> Result<()> {
        let mut layout = layout(&[1]);
        let font_runs = [FontRun {
            len: "😀".len(),
            font_id: FontId(7),
        }];
        for (id, color) in [(0, false), (1, false), (0, true), (1, true)] {
            layout.runs[0].glyphs = vec![source_glyph(0, 0.0, id, color)];
            let candidates = collect_candidates("😀", &font_runs, &layout, |_| false);
            let candidate = candidates.first().context("missing candidate")?;
            assert!(candidate.color);
            let span = candidate.glyphs.as_ref().context("missing span")?;
            assert_eq!(span.missing, id == 0);
            assert_eq!(span.native_color, color);
        }
        layout.runs[0].glyphs.clear();
        let candidates = collect_candidates("😀", &font_runs, &layout, |_| false);
        assert!(
            candidates
                .first()
                .context("missing candidate")?
                .glyphs
                .is_none()
        );
        Ok(())
    }

    fn glyph(index: usize) -> ShapedGlyph {
        ShapedGlyph {
            id: GlyphId(index as u32),
            position: point(px(index as f32 * 10.0), px(2.0)),
            index: index * 2,
            is_emoji: false,
        }
    }

    fn layout(run_lengths: &[usize]) -> LineLayout {
        let mut index = 0;
        let runs = run_lengths
            .iter()
            .enumerate()
            .map(|(font, length)| {
                let glyphs = (index..index + length).map(glyph).collect();
                index += length;
                ShapedRun {
                    font_id: FontId(font),
                    glyphs,
                }
            })
            .collect();
        LineLayout {
            runs,
            width: px(index as f32 * 10.0),
            font_size: px(14.0),
            ascent: px(11.0),
            descent: px(3.0),
            len: index * 2,
        }
    }

    fn replacement(range: Range<usize>, font_id: usize, delta: f32) -> Replacement {
        let mut glyph = glyph(range.start);
        glyph.id = GlyphId(999);
        glyph.position.x += px(1.0);
        glyph.position.y = px(7.0);
        glyph.is_emoji = true;
        Replacement {
            glyph_range: range,
            font_id: FontId(font_id),
            glyph,
            width_delta: px(delta),
        }
    }

    fn reference(layout: &mut LineLayout, replacements: Vec<Replacement>) {
        let original = std::mem::take(&mut layout.runs);
        let mut replacements = replacements.into_iter().peekable();
        let mut shift = Pixels::ZERO;
        let mut consumed_end = 0;
        for (index, (font_id, mut glyph)) in original
            .into_iter()
            .flat_map(|run| {
                run.glyphs
                    .into_iter()
                    .map(move |glyph| (run.font_id, glyph))
            })
            .enumerate()
        {
            if index < consumed_end {
                continue;
            }
            let font_id = if replacements
                .peek()
                .is_some_and(|replacement| replacement.glyph_range.start == index)
            {
                let replacement = replacements.next().expect("peeked replacement");
                glyph = replacement.glyph;
                glyph.position.x += shift;
                shift += replacement.width_delta;
                consumed_end = replacement.glyph_range.end;
                replacement.font_id
            } else {
                glyph.position.x += shift;
                font_id
            };
            if let Some(run) = layout.runs.last_mut().filter(|run| run.font_id == font_id) {
                run.glyphs.push(glyph);
            } else {
                layout.runs.push(ShapedRun {
                    font_id,
                    glyphs: vec![glyph],
                });
            }
        }
        layout.width += shift;
    }

    fn assert_equal(actual: &LineLayout, expected: &LineLayout) {
        let flatten = |layout: &LineLayout| {
            layout
                .runs
                .iter()
                .flat_map(|run| {
                    run.glyphs.iter().map(move |glyph| {
                        (
                            run.font_id,
                            glyph.id,
                            glyph.position,
                            glyph.index,
                            glyph.is_emoji,
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(flatten(actual), flatten(expected));
        assert_eq!(actual.width, expected.width);
        assert_eq!(actual.font_size, expected.font_size);
        assert_eq!(actual.ascent, expected.ascent);
        assert_eq!(actual.descent, expected.descent);
        assert_eq!(actual.len, expected.len);
    }

    #[test]
    fn no_replacements_preserves_even_empty_runs() {
        let mut actual = layout(&[0, 3, 0]);
        let pointer = actual.runs.as_ptr();
        let glyph_pointer = actual.runs[1].glyphs.as_ptr();
        apply_replacements(&mut actual, vec![]);
        assert_eq!(actual.runs.len(), 3);
        assert_eq!(actual.runs.as_ptr(), pointer);
        assert_eq!(actual.runs[1].glyphs.as_ptr(), glyph_pointer);
        assert_equal(&actual, &layout(&[0, 3, 0]));
        apply_replacements(&mut layout(&[]), vec![]);
        apply_replacements(&mut layout(&[0, 0]), vec![]);
    }

    #[test]
    fn whole_run_reuses_both_allocations() {
        let mut actual = layout(&[8]);
        actual.runs.reserve(4);
        actual.runs[0].glyphs.reserve(10);
        let runs = (actual.runs.as_ptr(), actual.runs.capacity());
        let glyphs = (
            actual.runs[0].glyphs.as_ptr(),
            actual.runs[0].glyphs.capacity(),
        );
        apply_replacements(&mut actual, vec![replacement(0..8, 9, -20.0)]);
        assert_eq!((actual.runs.as_ptr(), actual.runs.capacity()), runs);
        assert_eq!(
            (
                actual.runs[0].glyphs.as_ptr(),
                actual.runs[0].glyphs.capacity()
            ),
            glyphs
        );
        assert_eq!(actual.runs[0].font_id, FontId(9));
        assert_eq!(actual.runs[0].glyphs.len(), 1);
    }

    #[test]
    fn splits_reuse_outer_prefix_unaffected_and_emptied_buffers() {
        let mut actual = layout(&[4, 4, 3]);
        actual.runs.reserve(8);
        let outer = (actual.runs.as_ptr(), actual.runs.capacity());
        let prefix = actual.runs[0].glyphs.as_ptr();
        let emptied = (
            actual.runs[1].glyphs.as_ptr(),
            actual.runs[1].glyphs.capacity(),
        );
        let unaffected = actual.runs[2].glyphs.as_ptr();
        apply_replacements(&mut actual, vec![replacement(2..8, 9, -3.0)]);
        assert_eq!((actual.runs.as_ptr(), actual.runs.capacity()), outer);
        assert_eq!(actual.runs[0].glyphs.as_ptr(), prefix);
        assert_eq!(
            (
                actual.runs[1].glyphs.as_ptr(),
                actual.runs[1].glyphs.capacity()
            ),
            emptied
        );
        assert_eq!(actual.runs[2].glyphs.as_ptr(), unaffected);
        let mut expected = layout(&[4, 4, 3]);
        reference(&mut expected, vec![replacement(2..8, 9, -3.0)]);
        assert_equal(&actual, &expected);
    }

    #[test]
    fn multiple_splits_and_shifts() {
        let mut actual = layout(&[7, 2, 3]);
        actual.runs.reserve(12);
        let outer = actual.runs.as_ptr();
        let mut expected = layout(&[7, 2, 3]);
        let replacements = || {
            vec![
                replacement(1..2, 9, 3.0),
                replacement(3..5, 8, -7.0),
                replacement(6..10, 9, 2.0),
            ]
        };
        apply_replacements(&mut actual, replacements());
        reference(&mut expected, replacements());
        assert_equal(&actual, &expected);
        assert_eq!(actual.runs.as_ptr(), outer);
    }

    #[test]
    fn exhaustive_small_streams_match_reference() {
        // Base-four choices at each original glyph: keep, start a native-font
        // replacement, start a fallback replacement, or extend the previous one.
        for first in 0..=4 {
            for second in 0..=4 - first {
                let lengths = [first, 0, second, 4 - first - second];
                for choices in 0usize..4usize.pow(4) {
                    let replacements = || {
                        let mut replacements: Vec<Replacement> = Vec::new();
                        for index in 0..4 {
                            match (choices >> (2 * index)) & 3 {
                                0 => {}
                                1 => replacements.push(replacement(index..index + 1, 0, 2.0)),
                                2 => replacements.push(replacement(index..index + 1, 9, -3.0)),
                                _ => {
                                    if let Some(previous) = replacements
                                        .last_mut()
                                        .filter(|previous| previous.glyph_range.end == index)
                                    {
                                        previous.glyph_range.end += 1;
                                    }
                                }
                            }
                        }
                        replacements
                    };
                    let mut actual = layout(&lengths);
                    let mut expected = layout(&lengths);
                    apply_replacements(&mut actual, replacements());
                    reference(&mut expected, replacements());
                    assert_equal(&actual, &expected);
                }
            }
        }
    }
}
