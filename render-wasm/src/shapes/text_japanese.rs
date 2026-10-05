use super::text::{add_text_with_tabs, Paragraph, TextContent, TextSpan};
use super::text_vertical::{
    distribute_ruby_tops, ruby_overhang_room, shape_segment_with_fallbacks, single_glyph_blob,
    span_font_families,
};
use crate::globals::get_resources;
use crate::math::Point;
use crate::shapes::japanese::{classify, is_japanese_text_char, shed_pair_aki, JapaneseClass};
use crate::shapes::{kinsoku, merge_fills};
use crate::utils::{get_fallback_fonts, get_font_collection};
use skia_safe::{
    self as skia,
    textlayout::{
        ParagraphBuilder, ParagraphStyle, PlaceholderAlignment, PlaceholderStyle, RectHeightStyle,
        RectWidthStyle, TextBaseline,
    },
    Canvas, Contains, Font, FontMgr, GlyphId,
};

pub const WARICHU_FONT_SCALE: f32 = 0.5;
pub const EMPHASIS_FONT_SCALE: f32 = 0.5;
const HORIZONTAL_WARICHU_BUILDER_LEN: usize = 3;
const HORIZONTAL_WARICHU_STYLE_ANCHOR: char = '\u{00A0}';
const HORIZONTAL_WARICHU_BREAK_ANCHOR: char = '\u{200B}';

pub(crate) fn layout_span_texts(paragraph: &Paragraph) -> (Vec<String>, kinsoku::OffsetMap) {
    let texts: Vec<String> = paragraph
        .children()
        .iter()
        .map(TextSpan::apply_text_transform)
        .collect();
    // Text without Japanese characters or ruby keeps plain Skia layout.
    let uses_japanese_layout = paragraph.children().iter().any(TextSpan::has_ruby)
        || texts
            .iter()
            .any(|text| text.chars().any(is_japanese_text_char));
    if uses_japanese_layout {
        let ruby_breaks: Vec<Option<Vec<usize>>> = paragraph
            .children()
            .iter()
            .map(|span| span.has_ruby().then(Vec::new))
            .collect();
        if let Some((shifted, map)) =
            kinsoku::apply_to_span_texts_with_ruby_breaks(&texts, &ruby_breaks)
        {
            return (shifted, map);
        }
    }
    (texts, kinsoku::OffsetMap::default())
}

/// Curly quotes are full-width in some fonts and proportional in others
/// (JLREQ lists them both as brackets and as Western), so their built-in aki
/// is unknown.
fn is_curly_quote(ch: char) -> bool {
    matches!(ch, '‘' | '’' | '“' | '”')
}

/// (char index, amount) per span of the layout `texts` for the characters
/// whose advance loses a half-em to the punctuation aki rules of
/// `shed_pair_aki`: a closing mark sheds half of its own em, and the
/// character before an opening bracket gives up half of the bracket's em.
/// Inserted word joiners are transparent. Spans with `palt` already set
/// punctuation proportionally, and curly quotes never shed.
pub(crate) fn horizontal_aki_sheds(
    paragraph: &Paragraph,
    texts: &[String],
) -> Vec<Vec<(usize, f32)>> {
    let mut sheds: Vec<Vec<(usize, f32)>> = vec![Vec::new(); texts.len()];
    let half_em = |span: usize| {
        paragraph
            .children()
            .get(span)
            .map_or(0.0, |span| span.font_size * 0.5)
    };
    // (span, char index in the span text, class) of every real character.
    let chars: Vec<(usize, usize, Option<JapaneseClass>)> = texts
        .iter()
        .enumerate()
        .flat_map(|(span, text)| {
            let proportional = paragraph
                .children()
                .get(span)
                .is_some_and(|span| span.font_features == FontFeatures::Palt);
            text.chars()
                .enumerate()
                .filter(|(_, ch)| *ch != kinsoku::WORD_JOINER)
                .map(move |(index, ch)| {
                    let class = (!proportional && !is_curly_quote(ch)).then(|| classify(ch));
                    (span, index, class)
                })
        })
        .collect();
    for pair in chars.windows(2) {
        let [(before_span, before_index, Some(before)), (after_span, _, Some(after))] = *pair
        else {
            continue;
        };
        match shed_pair_aki(before, after) {
            (true, _) => sheds[before_span].push((before_index, half_em(before_span))),
            (_, true) => sheds[before_span].push((before_index, half_em(after_span))),
            _ => {}
        }
    }
    sheds
}

/// Extra spacing for long horizontal ruby, per span of a paragraph.
#[derive(Debug, Clone, Default)]
pub(crate) struct HorizontalRubySpacing {
    /// Letter-spacing added to characters of the layout texts: (char index,
    /// amount) per span.
    pub adjustments: Vec<Vec<(usize, f32)>>,
    /// Gap added between the base characters of each ruby span.
    pub base_gaps: Vec<f32>,
    /// Spacing added before each ruby base, on the previous character: half
    /// a gap, or none at a paragraph start, where the base takes it after.
    pub leading_gaps: Vec<f32>,
    /// Overhang room (before, after) of each ruby span over its neighbours.
    pub rooms: Vec<(f32, f32)>,
}

/// The span that owns a character of the layout `texts`, its char index and
/// the character, searching from (span, index) in `step` direction and
/// skipping inserted word joiners.
fn layout_neighbour(texts: &[String], span: usize, forward: bool) -> Option<(usize, usize, char)> {
    let real = |(index, ch): (usize, char)| (ch != kinsoku::WORD_JOINER).then_some((index, ch));
    if forward {
        texts
            .iter()
            .enumerate()
            .skip(span + 1)
            .find_map(|(owner, text)| {
                text.chars()
                    .enumerate()
                    .find_map(real)
                    .map(|(index, ch)| (owner, index, ch))
            })
    } else {
        texts
            .iter()
            .enumerate()
            .take(span)
            .rev()
            .find_map(|(owner, text)| {
                let chars: Vec<char> = text.chars().collect();
                chars
                    .iter()
                    .copied()
                    .enumerate()
                    .rev()
                    .find_map(real)
                    .map(|(index, ch)| (owner, index, ch))
            })
    }
}

/// Spacing that makes room for horizontal ruby longer than its base. The
/// reading overhangs its neighbours within their `ruby_overhang_room`; the
/// rest spreads the base (JLREQ §3.3.8): an equal gap between base
/// characters and half a gap at each end, the leading half on the previous
/// character.
pub(crate) fn horizontal_ruby_spacing(
    paragraph: &Paragraph,
    texts: &[String],
) -> HorizontalRubySpacing {
    if !paragraph
        .children()
        .iter()
        .any(|span| span.has_ruby() && !span.is_warichu())
    {
        return HorizontalRubySpacing::empty(texts.len());
    }
    let fallback_families: Vec<String> = get_fallback_fonts().iter().cloned().collect();
    horizontal_ruby_spacing_with(
        paragraph,
        texts,
        get_resources().fonts.font_provider(),
        &fallback_families,
    )
}

impl HorizontalRubySpacing {
    fn empty(spans: usize) -> Self {
        Self {
            adjustments: vec![Vec::new(); spans],
            base_gaps: vec![0.0; spans],
            leading_gaps: vec![0.0; spans],
            rooms: vec![(0.0, 0.0); spans],
        }
    }
}

/// `horizontal_ruby_spacing` with explicit fonts.
fn horizontal_ruby_spacing_with(
    paragraph: &Paragraph,
    texts: &[String],
    font_provider: &skia::textlayout::TypefaceFontProvider,
    fallback_families: &[String],
) -> HorizontalRubySpacing {
    let spans = paragraph.children();
    let mut spacing = HorizontalRubySpacing::empty(texts.len());
    let fallback_mgr = FontMgr::from(font_provider.clone());
    let width = |span: &TextSpan, text: &str, font_size: f32| -> f32 {
        shape_segment_with_fallbacks(
            text,
            font_size,
            &span_font_families(span, fallback_families),
            font_provider,
            false,
            span.font_features,
            &fallback_mgr,
        )
        .iter()
        .flat_map(|run| run.advances.iter())
        .sum()
    };
    for (index, (span, text)) in spans.iter().zip(texts).enumerate() {
        let ruby_text = span.ruby_text();
        if ruby_text.is_empty() || span.is_warichu() {
            continue;
        }
        let base: Vec<usize> = text
            .chars()
            .enumerate()
            .filter(|(_, ch)| *ch != kinsoku::WORD_JOINER)
            .map(|(char_index, _)| char_index)
            .collect();
        let Some(&last) = base.last() else {
            continue;
        };
        let ruby_font_size = span.ruby_font_size();
        let base_width = width(span, &span.apply_text_transform(), span.font_size)
            + span.letter_spacing * base.len() as f32;
        let ruby_width = width(span, ruby_text, ruby_font_size);
        let previous = layout_neighbour(texts, index, false);
        let next = layout_neighbour(texts, index, true);
        let room = |neighbour: Option<(usize, usize, char)>| {
            neighbour.map_or(0.0, |(owner, _, ch)| {
                ruby_overhang_room(
                    span.ruby_overhang,
                    Some(ch),
                    spans[owner].has_ruby(),
                    spans[owner].font_size,
                    ruby_font_size,
                )
            })
        };
        let rooms = (room(previous), room(next));
        spacing.rooms[index] = rooms;
        let deficit = ruby_width - base_width - rooms.0 - rooms.1;
        if deficit <= 0.0 {
            continue;
        }
        let gap = deficit / base.len() as f32;
        spacing.base_gaps[index] = gap;
        for &char_index in &base {
            let amount = if char_index == last { gap / 2.0 } else { gap };
            spacing.adjustments[index].push((char_index, amount));
        }
        match previous {
            Some((owner, char_index, _)) => {
                spacing.adjustments[owner].push((char_index, gap / 2.0));
                spacing.leading_gaps[index] = gap / 2.0;
            }
            None => spacing.adjustments[index].push((last, gap / 2.0)),
        }
    }
    spacing
}

/// Add a span to a horizontal paragraph builder. A warichu span becomes one
/// inline placeholder so its two lines wrap as a unit; its glyphs are painted
/// after layout. The characters at `sheds` (from `horizontal_aki_sheds`)
/// set narrower by their amounts; `extra` (from `horizontal_ruby_spacing`)
/// adds letter-spacing to characters.
pub(crate) fn add_horizontal_span(
    builder: &mut ParagraphBuilder,
    span: &TextSpan,
    builder_text: &str,
    sheds: &[(usize, f32)],
    extra: &[(usize, f32)],
    text_style: &skia::textlayout::TextStyle,
    fonts: &skia::textlayout::FontCollection,
) {
    if span.is_warichu() {
        let text = span.apply_text_transform();
        let (first, second) = warichu_text_lines(&text);
        let mut mini_style = text_style.clone();
        mini_style.set_font_size(span.font_size * WARICHU_FONT_SCALE);
        mini_style.set_height(1.0);
        mini_style.set_height_override(true);
        mini_style.set_letter_spacing(span.letter_spacing * WARICHU_FONT_SCALE);
        let measure = |line: &str| mini_paragraph(line, &mini_style, fonts).longest_line();
        let width = measure(first).max(measure(second)).max(0.01);
        let height = span.font_size.max(0.01);
        builder.add_placeholder(&PlaceholderStyle::new(
            width,
            height,
            PlaceholderAlignment::Middle,
            TextBaseline::Alphabetic,
            height,
        ));
        // SkParagraph does not expose a placeholder's TextStyle through line
        // metrics. A near-zero, inkless NBSP carries the span style for the
        // paint pass; the zero-width space after it allows a line break after
        // the atomic placeholder.
        let mut anchor_style = text_style.clone();
        anchor_style.set_font_size(0.01);
        anchor_style.set_height(0.01);
        anchor_style.set_height_override(true);
        anchor_style.set_letter_spacing(0.0);
        builder.push_style(&anchor_style);
        builder.add_text(HORIZONTAL_WARICHU_STYLE_ANCHOR.to_string());
        builder.add_text(HORIZONTAL_WARICHU_BREAK_ANCHOR.to_string());
    } else {
        add_text_with_sheds(builder, span, builder_text, sheds, extra, text_style);
    }
}

/// Add `text`, adjusting the letter-spacing of single characters so the
/// builder text stays unchanged: those at `sheds` lose their amount and
/// those in `extra` take its added spacing. Skia tracks every cluster,
/// including the zero-width word joiners the kinsoku pass inserts, so those
/// take no letter-spacing.
fn add_text_with_sheds(
    builder: &mut ParagraphBuilder,
    span: &TextSpan,
    text: &str,
    sheds: &[(usize, f32)],
    extra: &[(usize, f32)],
    text_style: &skia::textlayout::TextStyle,
) {
    let tracking = text_style.letter_spacing();
    let mut piece_start = 0;
    for (index, (byte, ch)) in text.char_indices().enumerate() {
        let amount_at = |adjustments: &[(usize, f32)]| -> f32 {
            adjustments
                .iter()
                .filter(|(char_index, _)| *char_index == index)
                .map(|(_, amount)| amount)
                .sum()
        };
        let added = amount_at(extra);
        let shed = amount_at(sheds);
        let letter_spacing = if ch == kinsoku::WORD_JOINER {
            0.0
        } else {
            tracking + added - shed
        };
        if letter_spacing == tracking {
            continue;
        }
        let mut style = text_style.clone();
        style.set_letter_spacing(letter_spacing);
        let end = byte + ch.len_utf8();
        add_text_with_tabs(builder, &text[piece_start..byte], span.font_size);
        builder.push_style(&style);
        builder.add_text(&text[byte..end]);
        builder.pop();
        piece_start = end;
    }
    add_text_with_tabs(builder, &text[piece_start..], span.font_size);
}

#[derive(Debug, Clone)]
pub(crate) struct HorizontalSpanRange {
    pub span: usize,
    pub builder_start: usize,
    pub builder_end: usize,
    pub shifted_start: usize,
    pub source_start: usize,
    pub source_end: usize,
    pub warichu: bool,
    pub style_anchor_start: usize,
}

/// Offset maps of one horizontal paragraph between source characters, the
/// kinsoku-adjusted layout text and the builder text. Building it runs the
/// kinsoku pass; reuse it for every lookup in the paragraph.
pub(crate) struct HorizontalOffsets {
    /// Map between the transformed text and the kinsoku-adjusted text.
    pub(crate) offset_map: kinsoku::OffsetMap,
    pub(crate) ranges: Vec<HorizontalSpanRange>,
    /// Transformed-text UTF-16 offset of every source character boundary.
    boundaries: Vec<usize>,
}

impl HorizontalOffsets {
    pub(crate) fn new(paragraph: &Paragraph) -> Self {
        let (span_texts, offset_map) = paragraph.layout_span_texts();
        let ranges = span_ranges(paragraph, span_texts, &offset_map);
        Self {
            offset_map,
            ranges,
            boundaries: source_char_boundaries(paragraph),
        }
    }

    /// Builder-text UTF-16 offset of a paragraph source character offset.
    pub(crate) fn source_to_builder(&self, source_char_offset: usize) -> usize {
        let boundaries = &self.boundaries;
        let source_utf16 = boundaries
            .get(source_char_offset)
            .copied()
            .unwrap_or_else(|| boundaries.last().copied().unwrap_or(0));
        let Some(range) = self
            .ranges
            .iter()
            .find(|range| source_utf16 >= range.source_start && source_utf16 <= range.source_end)
        else {
            return self
                .ranges
                .last()
                .map(|range| range.builder_end)
                .unwrap_or(0);
        };
        if range.warichu {
            return if source_utf16 >= range.source_end {
                range.builder_end
            } else {
                range.builder_start
            };
        }
        let shifted = self.offset_map.to_shifted(source_utf16);
        range.builder_start + shifted.saturating_sub(range.shifted_start)
    }

    /// Paragraph source character offset of a builder-text UTF-16 offset.
    pub(crate) fn builder_to_source(&self, builder_offset: usize) -> usize {
        let source_utf16 = self
            .ranges
            .iter()
            .find(|range| {
                builder_offset >= range.builder_start && builder_offset <= range.builder_end
            })
            .map(|range| {
                if range.warichu {
                    if builder_offset > range.builder_start {
                        range.source_end
                    } else {
                        range.source_start
                    }
                } else {
                    let within = builder_offset
                        .saturating_sub(range.builder_start)
                        .min(range.builder_end - range.builder_start);
                    self.offset_map.to_original(range.shifted_start + within)
                }
            })
            .unwrap_or_else(|| {
                self.ranges
                    .last()
                    .map(|range| range.source_end)
                    .unwrap_or(0)
            });
        let boundaries = &self.boundaries;
        boundaries
            .partition_point(|boundary| *boundary < source_utf16)
            .min(boundaries.len().saturating_sub(1))
    }

    /// Builder-text ranges of the selected source characters
    /// `[source_start, source_end)` outside warichu spans.
    pub(crate) fn normal_selection_ranges(
        &self,
        paragraph: &Paragraph,
        source_start: usize,
        source_end: usize,
    ) -> Vec<std::ops::Range<usize>> {
        let mut span_start = 0usize;
        paragraph
            .children()
            .iter()
            .filter_map(|span| {
                let span_end = span_start + span.text.chars().count();
                let selected_start = source_start.max(span_start);
                let selected_end = source_end.min(span_end);
                span_start = span_end;
                if span.is_warichu() || selected_start >= selected_end {
                    return None;
                }
                Some(self.source_to_builder(selected_start)..self.source_to_builder(selected_end))
            })
            .collect()
    }
}

pub(crate) fn horizontal_span_ranges(paragraph: &Paragraph) -> Vec<HorizontalSpanRange> {
    HorizontalOffsets::new(paragraph).ranges
}

pub(crate) fn horizontal_builder_to_source(paragraph: &Paragraph, builder_offset: usize) -> usize {
    HorizontalOffsets::new(paragraph).builder_to_source(builder_offset)
}

/// Ranges shared by layout, position data and editor mapping. `shifted_*`
/// addresses the kinsoku-adjusted text; `builder_*` addresses the builder
/// text, where each warichu span collapses to one placeholder.
fn span_ranges(
    paragraph: &Paragraph,
    span_texts: Vec<String>,
    offset_map: &kinsoku::OffsetMap,
) -> Vec<HorizontalSpanRange> {
    let mut builder_cursor = 0usize;
    let mut builder_byte_cursor = 0usize;
    let mut shifted_cursor = 0usize;
    paragraph
        .children()
        .iter()
        .zip(span_texts)
        .enumerate()
        .map(|(span_index, (span, text))| {
            let shifted_start = shifted_cursor;
            shifted_cursor += text.encode_utf16().count();
            let shifted_end = shifted_cursor;
            let source_start = offset_map.to_original(shifted_start);
            let source_end = offset_map.to_original(shifted_end);
            let warichu = span.is_warichu();
            let builder_start = builder_cursor;
            let style_anchor_start = if warichu {
                builder_byte_cursor + '\u{FFFC}'.len_utf8()
            } else {
                builder_byte_cursor
            };
            builder_cursor += if warichu {
                HORIZONTAL_WARICHU_BUILDER_LEN
            } else {
                shifted_end - shifted_start
            };
            builder_byte_cursor += if warichu {
                '\u{FFFC}'.len_utf8()
                    + HORIZONTAL_WARICHU_STYLE_ANCHOR.len_utf8()
                    + HORIZONTAL_WARICHU_BREAK_ANCHOR.len_utf8()
            } else {
                // `add_text_with_tabs` stores every tab as a U+FFFC placeholder.
                text.len() + text.matches('\t').count() * ('\u{FFFC}'.len_utf8() - 1)
            };
            HorizontalSpanRange {
                span: span_index,
                builder_start,
                builder_end: builder_cursor,
                shifted_start,
                source_start,
                source_end,
                warichu,
                style_anchor_start,
            }
        })
        .collect()
}

/// Transformed-text UTF-16 offset of every source character boundary: the
/// space the kinsoku offset map reports as "original". Differs from the
/// source UTF-16 when a transform changes the text length (`ß` -> `SS`).
fn source_char_boundaries(paragraph: &Paragraph) -> Vec<usize> {
    let mut boundaries = vec![0usize];
    let mut span_base = 0usize;
    for span in paragraph.children() {
        let applied = span.apply_text_transform_with_source_ranges();
        let mut ranges = applied.source_ranges.iter().peekable();
        let mut source_utf16 = 0usize;
        let mut output_end = 0usize;
        for character in span.text.chars() {
            source_utf16 += character.len_utf16();
            while let Some((output, _)) = ranges.next_if(|(_, source)| source.end <= source_utf16) {
                output_end = output.end;
            }
            boundaries.push(span_base + output_end);
        }
        span_base += applied.text.encode_utf16().count();
    }
    boundaries
}

/// Placeholder rect of each horizontal warichu span, keyed by span index.
/// Tabs in normal spans are placeholders too, so they are skipped.
pub(crate) fn horizontal_warichu_placeholders(
    paragraph: &Paragraph,
    laid_out: &skia::textlayout::Paragraph,
) -> Vec<(usize, skia::Rect)> {
    let mut placeholders = laid_out.get_rects_for_placeholders().into_iter();
    let mut result = Vec::new();
    for (index, span) in paragraph.children().iter().enumerate() {
        if span.is_warichu() {
            if let Some(textbox) = placeholders.next() {
                result.push((index, textbox.rect));
            }
        } else {
            for _ in span.text.matches('\t') {
                placeholders.next();
            }
        }
    }
    result
}

fn horizontal_warichu_boxes<'a>(
    paragraph: &'a Paragraph,
    laid_out: &skia::textlayout::Paragraph,
) -> Vec<(&'a TextSpan, usize, usize, skia::Rect)> {
    let mut source_start = 0usize;
    let mut placeholders = horizontal_warichu_placeholders(paragraph, laid_out)
        .into_iter()
        .peekable();
    let mut boxes = Vec::new();
    for (index, span) in paragraph.children().iter().enumerate() {
        let length = span.text.chars().count();
        if let Some((_, rect)) = placeholders.next_if(|(span_index, _)| *span_index == index) {
            boxes.push((span, source_start, source_start + length, rect));
        }
        source_start += length;
    }
    boxes
}

pub(crate) fn horizontal_warichu_hit_test(
    paragraph: &Paragraph,
    laid_out: &skia::textlayout::Paragraph,
    point: Point,
) -> Option<usize> {
    for (span, source_start, _, rect) in horizontal_warichu_boxes(paragraph, laid_out) {
        if !rect.contains(&point) {
            continue;
        }
        let split = warichu_split_chars(&span.apply_text_transform());
        let total = span.text.chars().count();
        let second = point.y >= rect.top() + rect.height() / 2.0;
        let (line_start, line_len) = if second {
            (split, total - split)
        } else {
            (0, split)
        };
        let fraction = ((point.x - rect.left()) / rect.width().max(0.01)).clamp(0.0, 1.0);
        let within = (fraction * line_len as f32).round() as usize;
        return Some(source_start + line_start + within.min(line_len));
    }
    None
}

pub(crate) fn horizontal_warichu_caret_rect(
    paragraph: &Paragraph,
    laid_out: &skia::textlayout::Paragraph,
    source_offset: usize,
) -> Option<skia::Rect> {
    for (span, source_start, source_end, rect) in horizontal_warichu_boxes(paragraph, laid_out) {
        if source_offset < source_start || source_offset > source_end {
            continue;
        }
        let split = warichu_split_chars(&span.apply_text_transform());
        let local = source_offset - source_start;
        let total = source_end - source_start;
        let (line_start, line_len, top) = if local >= split {
            (split, total - split, rect.top() + rect.height() / 2.0)
        } else {
            (0, split, rect.top())
        };
        let within = local.saturating_sub(line_start).min(line_len);
        let char_width = rect.width() / line_len.max(1) as f32;
        return Some(skia::Rect::from_xywh(
            rect.left() + within as f32 * char_width,
            top,
            char_width,
            rect.height() / 2.0,
        ));
    }
    None
}

pub(crate) fn horizontal_warichu_range_rects(
    paragraph: &Paragraph,
    laid_out: &skia::textlayout::Paragraph,
    source_start: usize,
    source_end: usize,
) -> Vec<skia::Rect> {
    let mut rects = Vec::new();
    for (span, span_start, span_end, rect) in horizontal_warichu_boxes(paragraph, laid_out) {
        let selected_start = source_start.max(span_start);
        let selected_end = source_end.min(span_end);
        if selected_start >= selected_end {
            continue;
        }
        let split = warichu_split_chars(&span.apply_text_transform());
        for (line_start, line_end, top) in [
            (span_start, span_start + split, rect.top()),
            (
                span_start + split,
                span_end,
                rect.top() + rect.height() / 2.0,
            ),
        ] {
            let start = selected_start.max(line_start);
            let end = selected_end.min(line_end);
            if start >= end {
                continue;
            }
            let line_len = line_end - line_start;
            let char_width = rect.width() / line_len.max(1) as f32;
            rects.push(skia::Rect::from_xywh(
                rect.left() + (start - line_start) as f32 * char_width,
                top,
                (end - start) as f32 * char_width,
                rect.height() / 2.0,
            ));
        }
    }
    rects
}

/// Char index where a warichu run splits into its two sub-lines: the
/// midpoint (first line longer), moved to the nearest split that keeps
/// kinsoku. At equal distance a forward move wins, pulling the mark up into
/// the first sub-line (jlreq). Falls back to the midpoint.
pub(crate) fn warichu_split_chars(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mid = n.div_ceil(2);
    let valid = |split: usize| {
        split >= 1
            && split < n
            && !kinsoku::forbidden_at_line_start(chars[split])
            && !kinsoku::forbidden_at_line_end(chars[split - 1])
    };
    if valid(mid) {
        return mid;
    }
    for distance in 1..n {
        if valid(mid + distance) {
            return mid + distance;
        }
        if mid > distance && valid(mid - distance) {
            return mid - distance;
        }
    }
    mid
}

/// The two warichu sub-lines of `text`, split at `warichu_split_chars`.
pub(crate) fn warichu_text_lines(text: &str) -> (&str, &str) {
    let split = warichu_split_chars(text);
    let split_byte = text
        .char_indices()
        .nth(split)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.split_at(split_byte)
}

/// Single-line paragraph for an annotation run. Laid out unbounded: a
/// re-layout at its own `longest_line()` can wrap the last glyph.
fn mini_paragraph(
    text: &str,
    style: &skia::textlayout::TextStyle,
    fonts: &skia::textlayout::FontCollection,
) -> skia::textlayout::Paragraph {
    let mut builder = ParagraphBuilder::new(&ParagraphStyle::default(), fonts);
    builder.push_style(style);
    builder.add_text(text);
    let mut paragraph = builder.build();
    paragraph.layout(f32::MAX);
    paragraph
}

/// Paint the two horizontal warichu sub-lines into SkParagraph's inline
/// placeholder boxes. Reading order is top line then bottom line.
pub(crate) fn paint_horizontal_warichu(
    canvas: &skia::Canvas,
    paragraph: &Paragraph,
    laid_out: &skia::textlayout::Paragraph,
    x: f32,
    y: f32,
) {
    let ranges = horizontal_span_ranges(paragraph);
    let placeholders = horizontal_warichu_placeholders(paragraph, laid_out);
    let warichu_ranges: Vec<_> = ranges.iter().filter(|range| range.warichu).collect();
    if placeholders.len() != warichu_ranges.len() {
        return;
    }

    for (range, (_, rect)) in warichu_ranges.into_iter().zip(placeholders) {
        let Some(span) = paragraph.children().get(range.span) else {
            continue;
        };
        let Some(mut style) = horizontal_span_style(laid_out, range) else {
            continue;
        };
        style.set_font_size(span.font_size * WARICHU_FONT_SCALE);
        style.set_height(1.0);
        style.set_height_override(true);
        style.set_letter_spacing(span.letter_spacing * WARICHU_FONT_SCALE);
        let transformed = span.apply_text_transform();
        let (first, second) = warichu_text_lines(&transformed);
        let first_para = mini_paragraph(first, &style, get_font_collection());
        let second_para = mini_paragraph(second, &style, get_font_collection());
        let half_height = rect.height() / 2.0;
        first_para.paint(canvas, (x + rect.left(), y + rect.top()));
        second_para.paint(canvas, (x + rect.left(), y + rect.top() + half_height));
    }
}

pub(crate) fn emphasis_char_allowed(character: char) -> bool {
    !character.is_whitespace()
        && !crate::shapes::japanese::classify(character).is_emphasis_prohibited()
}

#[derive(Debug, Clone)]
pub(crate) struct HorizontalEmphasisPlacement {
    pub(crate) span: usize,
    /// UTF-16 range of the base character in the span's transformed text.
    pub(crate) range: std::ops::Range<usize>,
    pub(crate) mark: char,
    /// Base character rect in the laid-out paragraph.
    pub(crate) rect: skia::Rect,
}

/// Place one emphasis mark above each eligible character, from the laid-out
/// rect of each character so marks follow SkParagraph's wrapping and bidi.
pub(crate) fn horizontal_emphasis_placements(
    paragraph: &Paragraph,
    offsets: &HorizontalOffsets,
    laid_out: &skia::textlayout::Paragraph,
) -> Vec<HorizontalEmphasisPlacement> {
    let offset_map = &offsets.offset_map;
    let mut placements = Vec::new();

    for range in offsets.ranges.iter().filter(|range| !range.warichu) {
        let Some(span) = paragraph.children().get(range.span) else {
            continue;
        };
        let Some(mark) = span.text_emphasis.mark_char() else {
            continue;
        };
        let transformed = span.apply_text_transform();
        let mut local_utf16 = 0usize;
        for character in transformed.chars() {
            let next_utf16 = local_utf16 + character.len_utf16();
            if emphasis_char_allowed(character) {
                let shifted_start = offset_map.to_shifted(range.source_start + local_utf16);
                let shifted_end = offset_map.to_shifted(range.source_start + next_utf16);
                let builder_start =
                    range.builder_start + shifted_start.saturating_sub(range.shifted_start);
                let builder_end =
                    range.builder_start + shifted_end.saturating_sub(range.shifted_start);
                let scalar_rect = laid_out
                    .get_rects_for_range(
                        builder_start..builder_end,
                        RectHeightStyle::Tight,
                        RectWidthStyle::Tight,
                    )
                    .into_iter()
                    .map(|textbox| textbox.rect)
                    .reduce(|mut rect, next| {
                        rect.join(next);
                        rect
                    });
                if let Some(rect) = scalar_rect {
                    placements.push(HorizontalEmphasisPlacement {
                        span: range.span,
                        range: local_utf16..next_utf16,
                        mark,
                        rect,
                    });
                }
            }
            local_utf16 = next_utf16;
        }
    }
    placements
}

pub(crate) fn horizontal_span_style(
    laid_out: &skia::textlayout::Paragraph,
    range: &HorizontalSpanRange,
) -> Option<skia::textlayout::TextStyle> {
    // Indexed style metrics use builder UTF-8 byte positions, unlike the
    // UTF-16 offsets of glyph and line ranges.
    let anchor = range.style_anchor_start..range.style_anchor_start + 1;
    laid_out.get_line_metrics().iter().find_map(|line| {
        line.get_style_metrics(anchor.clone())
            .into_iter()
            .next()
            .map(|(_, metric)| metric.text_style.clone())
    })
}

/// Top of the horizontal base em inside SkParagraph's typographic rect, less
/// a 3/14-em clearance (12 px at 56 px). The tight rect includes ascender-side
/// padding, so anchoring an over annotation to its top leaves a gap above CJK
/// ink; the em is bottom-aligned to the rect.
pub(crate) fn horizontal_annotation_over_top(rect: skia::Rect, font_size: f32) -> f32 {
    let font_size = font_size.max(0.0);
    rect.top.max(rect.bottom - font_size) - font_size * (3.0 / 14.0)
}

/// Em box of a horizontal emphasis mark in the laid-out paragraph: centred
/// over its base character, outside any stacked ruby layer.
pub(crate) fn horizontal_emphasis_mark_box(
    span: &TextSpan,
    placement: &HorizontalEmphasisPlacement,
) -> skia::Rect {
    let size = span.font_size * EMPHASIS_FONT_SCALE;
    let bottom = horizontal_annotation_over_top(placement.rect, span.font_size)
        - span.emphasis_ruby_offset();
    skia::Rect::from_xywh(
        placement.rect.center_x() - size / 2.0,
        bottom - size,
        size,
        size,
    )
}

/// Paint horizontal emphasis marks (圏点 / bouten) above their base glyphs.
/// Draw-only: the base paragraph keeps its own metrics.
pub(crate) fn paint_horizontal_emphasis(
    canvas: &skia::Canvas,
    paragraph: &Paragraph,
    laid_out: &skia::textlayout::Paragraph,
    x: f32,
    y: f32,
) {
    let offsets = HorizontalOffsets::new(paragraph);
    let placements = horizontal_emphasis_placements(paragraph, &offsets, laid_out);
    for range in offsets.ranges.iter().filter(|range| !range.warichu) {
        let Some(span) = paragraph.children().get(range.span) else {
            continue;
        };
        let Some(mark) = span.text_emphasis.mark_char() else {
            continue;
        };
        let Some(mut style) = horizontal_span_style(laid_out, range) else {
            continue;
        };
        style.set_font_size(span.font_size * EMPHASIS_FONT_SCALE);
        style.set_height(1.0);
        style.set_height_override(true);
        style.set_letter_spacing(0.0);
        let mark_paragraph = mini_paragraph(&mark.to_string(), &style, get_font_collection());
        let mark_ink = mark_paragraph
            .get_rects_for_range(
                0..mark.len_utf16(),
                RectHeightStyle::Tight,
                RectWidthStyle::Tight,
            )
            .into_iter()
            .map(|textbox| textbox.rect)
            .reduce(|mut rect, next| {
                rect.join(next);
                rect
            });
        let mark_width = mark_ink
            .as_ref()
            .map(|rect| rect.width())
            .unwrap_or_else(|| mark_paragraph.longest_line());
        let mark_ink_bottom = mark_ink
            .as_ref()
            .map(|rect| rect.bottom())
            .unwrap_or_else(|| mark_paragraph.height());
        for placement in placements
            .iter()
            .filter(|placement| placement.span == range.span && placement.mark == mark)
        {
            let mark_x = x + placement.rect.center_x() - mark_width / 2.0;
            let mark_y = y + horizontal_annotation_over_top(placement.rect, span.font_size)
                - mark_ink_bottom
                - span.emphasis_ruby_offset();
            mark_paragraph.paint(canvas, (mark_x, mark_y));
        }
    }
}

/// Split `total` glyphs across segments proportionally to their extents
/// (rounded per segment, remainder to the last) so no glyph is dropped.
fn split_counts_by_extent(extents: &[f32], total: usize) -> Vec<usize> {
    let mut counts = vec![0usize; extents.len()];
    if extents.is_empty() || total == 0 {
        return counts;
    }
    let sum: f32 = extents.iter().sum();
    if sum <= 0.0 {
        counts[extents.len() - 1] = total;
        return counts;
    }
    let mut assigned = 0usize;
    let last = extents.len() - 1;
    for (index, extent) in extents.iter().enumerate() {
        let count = if index == last {
            total - assigned
        } else {
            (((extent / sum) * total as f32).round() as usize).min(total - assigned)
        };
        counts[index] = count;
        assigned += count;
    }
    counts
}

/// (span index, builder-text range) of every span whose ruby is painted:
/// visible ruby on a base that is not warichu.
fn horizontal_ruby_targets(
    paragraph: &Paragraph,
    offsets: &HorizontalOffsets,
) -> Vec<(usize, std::ops::Range<usize>)> {
    paragraph
        .children()
        .iter()
        .zip(&offsets.ranges)
        .filter(|(span, range)| {
            !span.ruby_text().is_empty()
                && !range.warichu
                && range.builder_start < range.builder_end
        })
        .map(|(_, range)| (range.span, range.builder_start..range.builder_end))
        .collect()
}

/// Paint of the pass that laid out the base: fill, stroke, shadow or mask.
/// `fallback` covers a base Skia kept no style metrics for.
fn horizontal_ruby_paint(
    laid_out: &skia::textlayout::Paragraph,
    range: &HorizontalSpanRange,
    fallback: skia::Paint,
) -> skia::Paint {
    horizontal_span_style(laid_out, range).map_or(fallback, |style| style.foreground())
}

/// Ink edge of `glyph` (bottom when `over`, else top), used to attach ruby
/// ink to its base. Font-wide ascent/descent include leading, which leaves
/// ruby detached in many Japanese faces.
fn horizontal_ruby_ink_edge(font: &Font, glyph: GlyphId, fallback: f32, over: bool) -> f32 {
    let mut bounds = [skia::Rect::default()];
    font.get_bounds(&[glyph], &mut bounds, None);
    let bound = bounds[0];
    if bound.right > bound.left && bound.bottom > bound.top {
        if over {
            bound.bottom
        } else {
            bound.top
        }
    } else {
        fallback
    }
}

/// Paint ruby for one laid-out horizontal paragraph. Draw-only: base rects
/// come from `get_rects_for_range`, and the ruby is shaped at
/// `ruby_font_size` and spread over each line's rect with the vertical path's
/// jlreq distribution (`distribute_ruby_tops`). Lines are not reflowed; ruby
/// draws in the line's leading.
pub(crate) fn paint_horizontal_ruby(
    canvas: &Canvas,
    text_content: &TextContent,
    paragraph_index: usize,
    laid_out: &skia::textlayout::Paragraph,
    x: f32,
    y: f32,
) {
    let Some(paragraph) = text_content.paragraphs().get(paragraph_index) else {
        return;
    };
    if !paragraph.children().iter().any(TextSpan::has_ruby) {
        return;
    }
    let font_provider = get_resources().fonts.font_provider();
    let fallback_mgr = FontMgr::from(font_provider.clone());
    let fallback_families: Vec<String> = get_fallback_fonts().iter().cloned().collect();
    let bounds = text_content.bounds();
    let (layout_texts, _) = paragraph.layout_span_texts();
    let spacing = horizontal_ruby_spacing(paragraph, &layout_texts);
    let offsets = HorizontalOffsets::new(paragraph);
    let lines = laid_out.get_line_metrics();

    for (span_index, span_range) in horizontal_ruby_targets(paragraph, &offsets) {
        let span = &paragraph.children()[span_index];
        let ruby_text = span.ruby_text();
        let ruby_font_size = span.ruby_font_size();
        let families = span_font_families(span, &fallback_families);
        let paint = horizontal_ruby_paint(
            laid_out,
            &offsets.ranges[span_index],
            merge_fills(&span.fills, bounds),
        );
        let rects = laid_out.get_rects_for_range(
            span_range,
            skia::textlayout::RectHeightStyle::Tight,
            skia::textlayout::RectWidthStyle::Tight,
        );
        let shaped = shape_segment_with_fallbacks(
            ruby_text,
            ruby_font_size,
            &families,
            font_provider,
            false,
            span.font_features,
            &fallback_mgr,
        );
        let glyphs: Vec<(usize, usize, f32)> = shaped
            .iter()
            .enumerate()
            .flat_map(|(run_index, run)| {
                (0..run.glyphs.len()).map(move |glyph| {
                    (
                        run_index,
                        glyph,
                        run.advances.get(glyph).copied().unwrap_or(ruby_font_size),
                    )
                })
            })
            .collect();
        let extents: Vec<f32> = rects.iter().map(|rect| rect.rect.width()).collect();
        let counts = split_counts_by_extent(&extents, glyphs.len());
        let mut assigned = 0usize;
        for (rect_box, count) in rects.iter().zip(counts) {
            if count == 0 {
                continue;
            }
            let slice = &glyphs[assigned..assigned + count];
            assigned += count;
            let advance = slice
                .iter()
                .map(|(_, _, advance)| *advance)
                .fold(0.0f32, f32::max)
                .max(1.0);
            // The base rect ends with the spacing after its last glyph; the
            // spacing before it sits on the previous character.
            let leading = spacing.leading_gaps[span_index];
            let left = rect_box.rect.left() - leading;
            let width = rect_box.rect.width() + leading;
            let room = room_inside_line(
                &lines,
                rect_box.rect,
                left,
                width,
                spacing.rooms[span_index],
            );
            let lefts = distribute_ruby_tops(left, width, count, advance, span.ruby_align, room);
            for ((run_index, glyph, glyph_advance), left) in slice.iter().zip(lefts) {
                let run = &shaped[*run_index];
                let (_, metrics) = run.font.metrics();
                let baseline = match span.ruby_side {
                    RubySide::Over => {
                        y + horizontal_annotation_over_top(rect_box.rect, span.font_size)
                            - horizontal_ruby_ink_edge(
                                &run.font,
                                run.glyphs[*glyph],
                                metrics.descent,
                                true,
                            )
                    }
                    RubySide::Under => {
                        y + rect_box.rect.bottom()
                            - horizontal_ruby_ink_edge(
                                &run.font,
                                run.glyphs[*glyph],
                                metrics.ascent,
                                false,
                            )
                    }
                };
                if let Some(blob) = single_glyph_blob(&run.font, run.glyphs[*glyph]) {
                    let gx = x + left + (advance - glyph_advance) / 2.0;
                    canvas.draw_text_blob(&blob, (gx, baseline), &paint);
                }
            }
        }
    }
}

/// Overhang room of a horizontal ruby base, without the sides at its line's
/// edges: ruby never sticks out past the first or last character.
fn room_inside_line(
    lines: &[skia::textlayout::LineMetrics],
    rect: skia::Rect,
    left: f32,
    width: f32,
    room: (f32, f32),
) -> (f32, f32) {
    let middle = rect.center_y();
    let Some(line) = lines.iter().find(|line| {
        let baseline = line.baseline as f32;
        middle >= baseline - line.ascent as f32 && middle <= baseline + line.descent as f32
    }) else {
        return room;
    };
    let line_left = line.left as f32;
    let line_right = line_left + line.width as f32;
    (
        if left <= line_left + 0.5 { 0.0 } else { room.0 },
        if left + width >= line_right - 0.5 {
            0.0
        } else {
            room.1
        },
    )
}

/// Block flow direction of a paragraph. Horizontal uses skparagraph;
/// vertical-rl uses the custom vertical pass: columns top to bottom,
/// advancing right to left.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum WritingMode {
    #[default]
    HorizontalTb,
    VerticalRl,
}

/// Glyph orientation inside vertical flow: `Mixed` rotates non-CJK runs
/// sideways, `Upright` keeps every character upright. Ignored in
/// horizontal writing.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum TextOrientation {
    #[default]
    Mixed,
    Upright,
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum TextCombineUpright {
    #[default]
    None,
    All,
    /// Runs of 2-4 ASCII or full-width digits combine into one upright cell.
    Digits,
    /// Like `Digits`, for runs of exactly 2 (CSS `digits 2`).
    Digits2,
    /// Like `Digits`, for runs of 2-3.
    Digits3,
}

impl TextCombineUpright {
    /// Longest digit run that combines, when digits mode is active.
    pub fn digits_max(self) -> Option<usize> {
        match self {
            TextCombineUpright::Digits => Some(4),
            TextCombineUpright::Digits2 => Some(2),
            TextCombineUpright::Digits3 => Some(3),
            _ => None,
        }
    }
}

/// Emphasis mark (圏点 / bouten) applied per span, mirroring CSS
/// `text-emphasis-style`. The mark is drawn above each eligible horizontal
/// base character or to the right of its vertical column.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum TextEmphasis {
    #[default]
    None,
    FilledDot,
    OpenDot,
    FilledCircle,
    OpenCircle,
    FilledSesame,
    OpenSesame,
}

impl TextEmphasis {
    pub fn is_none(self) -> bool {
        matches!(self, TextEmphasis::None)
    }

    /// The glyph drawn as the emphasis mark, following the CSS
    /// `text-emphasis-style` character mapping.
    pub fn mark_char(self) -> Option<char> {
        match self {
            TextEmphasis::None => None,
            TextEmphasis::FilledDot => Some('•'),
            TextEmphasis::OpenDot => Some('◦'),
            TextEmphasis::FilledCircle => Some('●'),
            TextEmphasis::OpenCircle => Some('○'),
            TextEmphasis::FilledSesame => Some('﹅'),
            TextEmphasis::OpenSesame => Some('﹆'),
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum FontFeatures {
    #[default]
    None,
    Palt,
    Vpal,
}

/// Whether ruby and emphasis on the same side stack. `Auto` places emphasis
/// outside over-side ruby and reserves room for both; `None` lets them share
/// one layer.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum AnnotationClearance {
    #[default]
    None,
    Auto,
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum RubySize {
    #[default]
    Half,
    Third,
    Quarter,
}

impl RubySize {
    pub fn scale(self) -> f32 {
        match self {
            Self::Half => 0.5,
            Self::Third => 1.0 / 3.0,
            Self::Quarter => 0.25,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum RubyAlign {
    #[default]
    SpaceAround,
    Center,
    Start,
    SpaceBetween,
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum RubyOverhang {
    #[default]
    Auto,
    None,
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum RubySide {
    #[default]
    Over,
    Under,
}

impl AnnotationClearance {
    pub fn is_auto(self) -> bool {
        matches!(self, AnnotationClearance::Auto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::{FontFamily, FontStyle, TextAlign, TextDirection, TextTransform};
    use crate::Uuid;

    #[test]
    fn warichu_split_balances_and_respects_kinsoku() {
        // Balanced midpoint when nothing forbids it, first line longer.
        assert_eq!(warichu_split_chars("あいうえおか"), 3);
        assert_eq!(warichu_split_chars("あいうえお"), 3);
        // A comma at the midpoint may end the first sub-line...
        assert_eq!(warichu_split_chars("あい、うえ"), 3);
        // ...but must not start the second one: the split moves forward.
        assert_eq!(warichu_split_chars("あいう、えお"), 4);
        // An opening bracket must not end the first sub-line.
        assert_eq!(warichu_split_chars("あい「うえお"), 4);
        // Pathological all-forbidden text keeps the midpoint.
        assert_eq!(warichu_split_chars("、、、、"), 2);
    }

    #[test]
    fn horizontal_ruby_ranges_use_utf16_across_spans() {
        init_state();
        let mut astral = make_span("𠀀", 0.0);
        astral.ruby = "あ".to_string();
        let mut kanji = make_span("漢", 0.0);
        kanji.ruby = "かん".to_string();
        let paragraph = make_paragraph(vec![astral, kanji], 0.0);
        let offsets = HorizontalOffsets::new(&paragraph);

        assert_eq!(
            horizontal_ruby_targets(&paragraph, &offsets),
            vec![(0, 0..2), (1, 2..3)]
        );
    }

    #[test]
    fn split_counts_by_extent_drops_no_glyph() {
        assert_eq!(split_counts_by_extent(&[60.0, 40.0], 5), vec![3, 2]);
        assert_eq!(split_counts_by_extent(&[100.0], 4), vec![4]);
        assert_eq!(split_counts_by_extent(&[0.0, 0.0], 3), vec![0, 3]);
        assert_eq!(
            split_counts_by_extent(&[1.0, 1.0, 1.0], 2)
                .iter()
                .sum::<usize>(),
            2
        );
        assert!(split_counts_by_extent(&[], 3).is_empty());
    }

    // apply_text_transform reads the browser from the design state.
    fn init_state() {
        crate::globals::design_init();
    }

    fn make_span(text: &str, letter_spacing: f32) -> TextSpan {
        TextSpan {
            text: text.to_string(),
            font_family: FontFamily::new(Uuid::nil(), 400, FontStyle::Normal),
            font_size: 16.0,
            line_height: 1.0,
            letter_spacing,
            font_weight: 400,
            font_variant_id: Uuid::nil(),
            text_decoration: None,
            text_transform: None,
            text_direction: TextDirection::LTR,
            text_orientation: TextOrientation::default(),
            text_combine_upright: TextCombineUpright::default(),
            text_emphasis: TextEmphasis::default(),
            ruby: String::default(),
            warichu: false,
            font_features: FontFeatures::default(),
            annotation_clearance: AnnotationClearance::default(),
            ruby_size: RubySize::default(),
            ruby_align: RubyAlign::default(),
            ruby_overhang: RubyOverhang::default(),
            ruby_side: RubySide::default(),
            paragraph_position: u32::MAX,
            span_position: u32::MAX,
            fills: vec![],
        }
    }

    fn make_paragraph(spans: Vec<TextSpan>, letter_spacing: f32) -> Paragraph {
        Paragraph::new(
            TextAlign::default(),
            TextDirection::LTR,
            None,
            None,
            1.0,
            letter_spacing,
            spans,
        )
    }

    #[test]
    fn layout_span_texts_applies_kinsoku() {
        init_state();
        let paragraph = make_paragraph(vec![make_span("雪国", 0.0), make_span("。です", 0.0)], 0.0);
        let (texts, map) = paragraph.layout_span_texts();
        assert_eq!(
            texts,
            vec!["雪国".to_string(), "\u{2060}。です".to_string()]
        );
        assert!(!map.is_empty());
        assert_eq!(map.to_original(3), 2);
    }

    #[test]
    fn layout_span_texts_applies_kinsoku_under_paragraph_letter_spacing() {
        init_state();
        let paragraph = make_paragraph(vec![make_span("雪国。", 0.0)], 2.0);
        let (texts, _) = paragraph.layout_span_texts();
        assert_eq!(texts, vec!["雪国\u{2060}。".to_string()]);
    }

    #[test]
    fn layout_span_texts_applies_kinsoku_under_span_letter_spacing() {
        init_state();
        let paragraph = make_paragraph(vec![make_span("雪国。", 1.5)], 0.0);
        let (texts, _) = paragraph.layout_span_texts();
        assert_eq!(texts, vec!["雪国\u{2060}。".to_string()]);
    }

    #[test]
    fn inserted_joiners_take_no_letter_spacing() {
        init_state();
        let measure = |text: &str| {
            let span = make_span(text, 5.0);
            let mut style = skia::textlayout::TextStyle::default();
            style.set_font_size(span.font_size);
            style.set_letter_spacing(span.letter_spacing);
            let mut fonts = skia::textlayout::FontCollection::new();
            fonts.set_default_font_manager(skia::FontMgr::new(), None);
            let mut builder = ParagraphBuilder::new(&ParagraphStyle::default(), &fonts);
            builder.push_style(&style);
            add_horizontal_span(&mut builder, &span, text, &[], &[], &style, &fonts);
            let mut laid_out = builder.build();
            laid_out.layout(f32::MAX);
            laid_out.longest_line()
        };
        assert!((measure("A\u{2060}B") - measure("AB")).abs() < 0.01);
    }

    #[test]
    fn layout_span_texts_respects_text_transform() {
        init_state();
        let mut span = make_span("hello。", 0.0);
        span.text_transform = Some(TextTransform::Uppercase);
        let paragraph = make_paragraph(vec![span], 0.0);
        let (texts, _) = paragraph.layout_span_texts();
        assert_eq!(texts, vec!["HELLO\u{2060}。".to_string()]);
    }

    #[test]
    fn layout_span_texts_identity_map_for_plain_text() {
        init_state();
        let paragraph = make_paragraph(vec![make_span("helloworld", 0.0)], 0.0);
        let (texts, map) = paragraph.layout_span_texts();
        assert_eq!(texts, vec!["helloworld".to_string()]);
        assert!(map.is_empty());
        assert_eq!(map.to_original(5), 5);
        assert_eq!(map.to_shifted(5), 5);
    }

    #[test]
    fn layout_span_texts_leaves_text_without_japanese_unchanged() {
        init_state();
        let paragraph = make_paragraph(vec![make_span("hello world foo", 0.0)], 0.0);
        let (texts, map) = paragraph.layout_span_texts();
        assert_eq!(texts, vec!["hello world foo".to_string()]);
        assert!(map.is_empty());
    }

    #[test]
    fn layout_span_texts_sets_western_spaces_inside_japanese_text() {
        init_state();
        let paragraph = make_paragraph(vec![make_span("日本 hello world", 0.0)], 0.0);
        let (texts, _) = paragraph.layout_span_texts();
        assert_eq!(
            texts,
            vec![format!(
                "日本{}hello{}world",
                kinsoku::WESTERN_WORD_SPACE,
                kinsoku::WESTERN_WORD_SPACE
            )]
        );
    }

    #[test]
    fn layout_span_texts_keeps_ruby_atomic_without_japanese_text() {
        init_state();
        let mut group = make_span("ab", 0.0);
        group.ruby = "x".to_string();
        let paragraph = make_paragraph(vec![group], 0.0);
        assert_eq!(
            paragraph.layout_span_texts().0,
            vec!["a\u{2060}b".to_string()]
        );
    }

    fn test_ruby_spacing(paragraph: &Paragraph, texts: &[String]) -> HorizontalRubySpacing {
        let provider = skia::textlayout::TypefaceFontProvider::new();
        let typeface = FontMgr::new()
            .new_from_data(include_bytes!("../fonts/sourcesanspro-regular.ttf"), None)
            .expect("test font");
        let mut provider = provider;
        provider.register_typeface(typeface, Some("sourcesanspro"));
        horizontal_ruby_spacing_with(paragraph, texts, &provider, &["sourcesanspro".to_string()])
    }

    #[test]
    fn horizontal_ruby_overhang_room_covers_kana_neighbours_only() {
        init_state();
        let mut base = make_span("字", 0.0);
        base.ruby = "じじじ".to_string();
        let paragraph = make_paragraph(vec![make_span("か", 0.0), base, make_span("漢", 0.0)], 0.0);
        let (texts, _) = paragraph.layout_span_texts();
        let spacing = test_ruby_spacing(&paragraph, &texts);
        assert_eq!(spacing.rooms[1], (8.0, 0.0));
    }

    #[test]
    fn horizontal_long_ruby_spreads_its_base_without_overhang() {
        init_state();
        let mut base = make_span("i", 0.0);
        base.ruby = "MMMMMMMM".to_string();
        base.ruby_overhang = RubyOverhang::None;
        let paragraph = make_paragraph(vec![make_span("a", 0.0), base, make_span("b", 0.0)], 0.0);
        let (texts, _) = paragraph.layout_span_texts();
        let spacing = test_ruby_spacing(&paragraph, &texts);
        let gap = spacing.base_gaps[1];
        assert!(gap > 0.0, "the base grows under a long reading");
        assert_eq!(spacing.adjustments[1], vec![(0, gap / 2.0)]);
        assert_eq!(spacing.adjustments[0], vec![(0, gap / 2.0)]);
        assert_eq!(spacing.leading_gaps[1], gap / 2.0);
    }

    #[test]
    fn horizontal_ruby_base_at_paragraph_start_takes_all_spacing_after() {
        init_state();
        let mut base = make_span("i", 0.0);
        base.ruby = "MMMMMMMM".to_string();
        base.ruby_overhang = RubyOverhang::None;
        let paragraph = make_paragraph(vec![base, make_span("b", 0.0)], 0.0);
        let (texts, _) = paragraph.layout_span_texts();
        let spacing = test_ruby_spacing(&paragraph, &texts);
        let gap = spacing.base_gaps[0];
        assert!(gap > 0.0);
        assert_eq!(
            spacing.leading_gaps[0], 0.0,
            "nothing sticks out before the line start"
        );
        assert_eq!(spacing.adjustments[0], vec![(0, gap / 2.0), (0, gap / 2.0)]);
    }

    #[test]
    fn horizontal_short_ruby_adds_no_spacing() {
        init_state();
        let mut base = make_span("MMMM", 0.0);
        base.ruby = "i".to_string();
        let paragraph = make_paragraph(vec![base], 0.0);
        let (texts, _) = paragraph.layout_span_texts();
        let spacing = test_ruby_spacing(&paragraph, &texts);
        assert_eq!(spacing.base_gaps, vec![0.0]);
        assert!(spacing.adjustments[0].is_empty());
    }

    #[test]
    fn annotation_room_follows_ruby_size_and_clearance() {
        let mut span = make_span("漢字", 0.0);
        span.ruby = "かんじ".to_string();
        assert_eq!(span.annotation_room_em(), 0.0, "none keeps the line height");
        span.annotation_clearance = AnnotationClearance::Auto;
        span.ruby_size = RubySize::Quarter;
        assert_eq!(span.annotation_room_em(), 0.25);
        span.text_emphasis = TextEmphasis::FilledDot;
        assert_eq!(span.annotation_room_em(), 0.75);
    }

    #[test]
    fn horizontal_ruby_is_atomic() {
        init_state();
        let mut group = make_span("日本", 0.0);
        group.ruby = "にほん".to_string();
        let paragraph = make_paragraph(vec![group], 0.0);
        assert_eq!(
            paragraph.layout_span_texts().0,
            vec!["日\u{2060}本".to_string()]
        );
    }

    #[test]
    fn horizontal_annotation_uses_the_base_em_not_typographic_leading() {
        let rect = skia::Rect::from_xywh(0.0, 66.0, 56.0, 90.0);

        assert_eq!(horizontal_annotation_over_top(rect, 56.0), 88.0);
    }

    #[test]
    fn horizontal_warichu_collapses_to_one_builder_position() {
        init_state();
        let mut warichu = make_span("割注入り", 0.0);
        warichu.warichu = true;
        let paragraph = make_paragraph(vec![warichu, make_span("後", 0.0)], 0.0);

        let ranges = horizontal_span_ranges(&paragraph);
        assert_eq!(ranges[0].builder_start..ranges[0].builder_end, 0..3);
        assert_eq!(ranges[1].builder_start..ranges[1].builder_end, 3..4);
        assert_eq!(HorizontalOffsets::new(&paragraph).source_to_builder(2), 0);
        assert_eq!(HorizontalOffsets::new(&paragraph).source_to_builder(4), 3);
        assert_eq!(HorizontalOffsets::new(&paragraph).source_to_builder(5), 4);
        assert_eq!(horizontal_builder_to_source(&paragraph, 1), 4);
        assert_eq!(horizontal_builder_to_source(&paragraph, 2), 4);
        assert_eq!(horizontal_builder_to_source(&paragraph, 3), 4);
        assert_eq!(horizontal_builder_to_source(&paragraph, 4), 5);
        assert_eq!(
            HorizontalOffsets::new(&paragraph).normal_selection_ranges(&paragraph, 1, 5),
            vec![3..4]
        );
    }

    #[test]
    fn horizontal_builder_mapping_preserves_non_bmp_boundaries() {
        init_state();
        let paragraph = make_paragraph(vec![make_span("😀A", 0.0)], 0.0);

        assert_eq!(HorizontalOffsets::new(&paragraph).source_to_builder(1), 2);
        assert_eq!(horizontal_builder_to_source(&paragraph, 2), 1);
        assert_eq!(HorizontalOffsets::new(&paragraph).source_to_builder(2), 3);
        assert_eq!(horizontal_builder_to_source(&paragraph, 3), 2);
    }

    #[test]
    fn horizontal_warichu_builder_emits_one_styled_placeholder() {
        init_state();
        let mut span = make_span("割注入り", 0.0);
        span.warichu = true;
        let mut style = skia::textlayout::TextStyle::default();
        style.set_font_size(span.font_size);
        let mut fonts = skia::textlayout::FontCollection::new();
        fonts.set_default_font_manager(skia::FontMgr::new(), None);
        let mut builder = ParagraphBuilder::new(&ParagraphStyle::default(), &fonts);
        builder.push_style(&style);
        add_horizontal_span(&mut builder, &span, &span.text, &[], &[], &style, &fonts);
        let mut laid_out = builder.build();
        laid_out.layout(200.0);

        let placeholders = laid_out.get_rects_for_placeholders();
        assert_eq!(placeholders.len(), 1);
        assert!(placeholders[0].rect.width() > 0.0);
        assert!(placeholders[0].rect.height() > 0.0);
        let has_style = laid_out
            .get_line_metrics()
            .iter()
            .any(|line| !line.get_style_metrics(3..5).is_empty());
        assert!(
            has_style,
            "the paint pass must recover the placeholder style"
        );
    }

    #[test]
    fn horizontal_warichu_allows_wrapping_after_the_atomic_box() {
        init_state();
        let mut span = make_span("割注入り", 0.0);
        span.warichu = true;
        let following = make_span("A", 0.0);
        let mut style = skia::textlayout::TextStyle::default();
        style.set_font_size(span.font_size);
        let mut fonts = skia::textlayout::FontCollection::new();
        fonts.set_default_font_manager(skia::FontMgr::new(), None);
        let mut builder = ParagraphBuilder::new(&ParagraphStyle::default(), &fonts);
        builder.push_style(&style);
        add_horizontal_span(&mut builder, &span, &span.text, &[], &[], &style, &fonts);
        builder.push_style(&style);
        builder.add_text(&following.text);

        let mut laid_out = builder.build();
        laid_out.layout(16.1);

        assert_eq!(laid_out.get_rects_for_placeholders().len(), 1);
        assert_eq!(laid_out.get_line_metrics().len(), 2);
    }

    #[test]
    fn warichu_sub_lines_paint_on_one_line() {
        let mut fonts = skia::textlayout::FontCollection::new();
        fonts.set_default_font_manager(skia::FontMgr::new(), None);
        for (text, size, letter_spacing) in [
            ("書き下ろし", 13.0, 0.75),
            ("浅煎り・", 13.0, 0.0),
            ("税込・", 53.0, -1.5),
            ("架空の", 7.5, 0.0),
        ] {
            let mut style = skia::textlayout::TextStyle::default();
            style.set_font_size(size);
            style.set_height(1.0);
            style.set_height_override(true);
            style.set_letter_spacing(letter_spacing);
            let paragraph = mini_paragraph(text, &style, &fonts);
            assert_eq!(paragraph.line_number(), 1, "{text:?} must not wrap");
        }
    }

    #[test]
    fn emphasis_excludes_whitespace_and_japanese_punctuation() {
        for character in " \t\n、。，．「」『』（）［］【】〔〕〈〉《》‘’“”".chars()
        {
            assert!(
                !emphasis_char_allowed(character),
                "emphasis must skip {character:?}"
            );
        }
        for character in "漢あA1・！？".chars() {
            assert!(
                emphasis_char_allowed(character),
                "emphasis should mark {character:?}"
            );
        }
    }

    #[test]
    fn horizontal_aki_sheds_follow_punctuation_pairs() {
        init_state();
        let texts = vec!["あ。」い、".to_string(), "「「う".to_string()];
        let paragraph =
            make_paragraph(texts.iter().map(|text| make_span(text, 0.0)).collect(), 0.0);
        assert_eq!(
            horizontal_aki_sheds(&paragraph, &texts),
            vec![vec![(1, 8.0), (4, 8.0)], vec![(0, 8.0)]],
            "。 before 」, 、 before 「 across spans, and 「 before 「"
        );
    }

    #[test]
    fn leading_aki_shed_uses_the_bracket_font_size() {
        init_state();
        let texts = vec!["「".to_string(), "「う".to_string()];
        let mut large = make_span("「う", 0.0);
        large.font_size = 32.0;
        let paragraph = make_paragraph(vec![make_span("「", 0.0), large], 0.0);
        assert_eq!(
            horizontal_aki_sheds(&paragraph, &texts),
            vec![vec![(0, 16.0)], vec![]],
            "the 16px 「 gives up half of the next 32px bracket's em"
        );
    }

    #[test]
    fn horizontal_aki_sheds_skip_curly_quotes() {
        init_state();
        for text in ["“‘Hi’”", "あ“い”。", "あ‘い’、"] {
            let texts = vec![text.to_string()];
            let paragraph = make_paragraph(vec![make_span(text, 0.0)], 0.0);
            assert_eq!(
                horizontal_aki_sheds(&paragraph, &texts),
                vec![Vec::<(usize, f32)>::new()],
                "no aki shed in {text:?}"
            );
        }
    }

    #[test]
    fn horizontal_aki_sheds_skip_word_joiners_and_palt() {
        init_state();
        let joined = vec!["。\u{2060}」".to_string()];
        let paragraph = make_paragraph(vec![make_span(&joined[0], 0.0)], 0.0);
        assert_eq!(
            horizontal_aki_sheds(&paragraph, &joined),
            vec![vec![(0, 8.0)]]
        );

        let mut palt = make_span("。」", 0.0);
        palt.font_features = FontFeatures::Palt;
        let paragraph = make_paragraph(vec![palt], 0.0);
        assert_eq!(
            horizontal_aki_sheds(&paragraph, &["。」".to_string()]),
            vec![Vec::<(usize, f32)>::new()]
        );
    }

    #[test]
    fn horizontal_shed_sets_the_closing_mark_half_an_em_narrower() {
        init_state();
        let mut resources =
            crate::render::RenderResources::try_new_headless().expect("headless resources");
        let _guard = crate::globals::TestRenderResourcesGuard::install(&mut resources);
        let width = |span: TextSpan| {
            let mut content = super::super::text::TextContent::new(
                crate::math::Rect::from_xywh(0.0, 0.0, 400.0, 100.0),
                crate::shapes::GrowType::Fixed,
            );
            content.add_paragraph(make_paragraph(vec![span], 0.0));
            let mut groups = content.paragraph_builder_group_from_text(None);
            let mut paragraph = groups[0][0].build();
            paragraph.layout(1000.0);
            paragraph.max_intrinsic_width()
        };
        let mut palt = make_span("。」", 0.0);
        palt.font_features = FontFeatures::Palt;
        let solid = width(make_span("。」", 0.0));
        let spaced = width(palt);
        assert!(
            (spaced - solid - 8.0).abs() < 0.01,
            "the period sheds half of its 16px em: {spaced} vs {solid}"
        );
    }

    #[test]
    fn horizontal_ruby_targets_use_builder_offsets() {
        init_state();
        let mut note = make_span("割注入り", 0.0);
        note.warichu = true;
        let mut base = make_span("漢字", 0.0);
        base.ruby = "かんじ".to_string();
        let paragraph = make_paragraph(vec![note, base], 0.0);
        let offsets = HorizontalOffsets::new(&paragraph);
        assert_eq!(
            horizontal_ruby_targets(&paragraph, &offsets),
            vec![(1, 3..6)],
            "the warichu span takes three builder units; 漢\u{2060}字 carries a joiner"
        );
    }

    #[test]
    fn horizontal_ruby_paint_follows_the_pass_paint() {
        init_state();
        let mut base = make_span("漢字", 0.0);
        base.ruby = "かんじ".to_string();
        let paragraph = make_paragraph(vec![base.clone()], 0.0);
        let mut stroke = skia::Paint::default();
        stroke.set_style(skia::PaintStyle::Stroke);
        stroke.set_color(skia::Color::RED);
        let mut style = skia::textlayout::TextStyle::default();
        style.set_font_size(base.font_size);
        style.set_foreground_paint(&stroke);
        let mut fonts = skia::textlayout::FontCollection::new();
        fonts.set_default_font_manager(skia::FontMgr::new(), None);
        let mut builder = ParagraphBuilder::new(&ParagraphStyle::default(), &fonts);
        builder.push_style(&style);
        add_horizontal_span(&mut builder, &base, &base.text, &[], &[], &style, &fonts);
        let mut laid_out = builder.build();
        laid_out.layout(400.0);

        let offsets = HorizontalOffsets::new(&paragraph);
        let paint = horizontal_ruby_paint(&laid_out, &offsets.ranges[0], skia::Paint::default());
        assert_eq!(paint.style(), skia::PaintStyle::Stroke);
        assert_eq!(paint.color(), skia::Color::RED);
    }

    #[test]
    fn horizontal_position_data_skips_tab_placeholders_before_warichu() {
        init_state();
        let mut warichu = make_span("割注入り", 0.0);
        warichu.warichu = true;
        let mut content = super::super::text::TextContent::new(
            crate::math::Rect::from_xywh(0.0, 0.0, 400.0, 100.0),
            crate::shapes::GrowType::Fixed,
        );
        content.add_paragraph(make_paragraph(vec![make_span("A\tB", 0.0), warichu], 0.0));
        let mut shape = crate::shapes::Shape::new(Uuid::nil());
        shape.set_selrect(0.0, 0.0, 400.0, 100.0);
        let mut resources =
            crate::render::RenderResources::try_new_headless().expect("headless resources");
        let _guard = crate::globals::TestRenderResourcesGuard::install(&mut resources);

        let data = super::super::text::calculate_position_data(&shape, &content, false);

        let text_right = data
            .iter()
            .filter(|entry| entry.span == 0)
            .map(|entry| entry.x + entry.width)
            .fold(0.0f32, f32::max);
        let warichu_left = data
            .iter()
            .filter(|entry| entry.span == 1)
            .map(|entry| entry.x)
            .fold(f32::MAX, f32::min);
        assert!(
            warichu_left >= text_right - 0.5,
            "the warichu strips start at {warichu_left}, inside A\tB which ends at {text_right}"
        );
    }

    #[test]
    fn horizontal_position_data_carries_warichu_lines_and_emphasis_marks() {
        init_state();
        let mut emphasized = make_span("A、B", 0.0);
        emphasized.text_emphasis = TextEmphasis::FilledDot;
        let mut warichu = make_span("割注入り", 0.0);
        warichu.warichu = true;
        let mut content = super::super::text::TextContent::new(
            crate::math::Rect::from_xywh(0.0, 0.0, 400.0, 100.0),
            crate::shapes::GrowType::Fixed,
        );
        content.add_paragraph(make_paragraph(vec![emphasized, warichu], 0.0));
        let mut shape = crate::shapes::Shape::new(Uuid::nil());
        shape.set_selrect(0.0, 0.0, 400.0, 100.0);
        let mut resources =
            crate::render::RenderResources::try_new_headless().expect("headless resources");
        let _guard = crate::globals::TestRenderResourcesGuard::install(&mut resources);

        let data = super::super::text::calculate_position_data(&shape, &content, false);

        let warichu_lines: Vec<(u32, u32)> = data
            .iter()
            .filter(|entry| entry.span == 1)
            .map(|entry| (entry.start_pos, entry.end_pos))
            .collect();
        assert_eq!(warichu_lines, vec![(0, 2), (2, 4)]);
        let marks: Vec<(u32, u32)> = data
            .iter()
            .filter(|entry| entry.direction == super::super::text_vertical::DIRECTION_EMPHASIS_MARK)
            .map(|entry| (entry.start_pos, entry.end_pos))
            .collect();
        assert_eq!(marks, vec![(0, 1), (2, 3)], "A and B are marked; 、 is not");
    }

    #[test]
    fn horizontal_emphasis_mark_stacks_outside_auto_clearance_ruby() {
        let placement = HorizontalEmphasisPlacement {
            span: 0,
            range: 0..1,
            mark: '•',
            rect: skia::Rect::from_xywh(10.0, 40.0, 16.0, 20.0),
        };
        let plain = make_span("漢", 0.0);
        let mut stacked = make_span("漢", 0.0);
        stacked.ruby = "かん".to_string();
        stacked.annotation_clearance = AnnotationClearance::Auto;

        let plain_box = horizontal_emphasis_mark_box(&plain, &placement);
        let stacked_box = horizontal_emphasis_mark_box(&stacked, &placement);

        assert_eq!(plain_box.width(), plain.font_size * EMPHASIS_FONT_SCALE);
        assert_eq!(plain_box.center_x(), placement.rect.center_x());
        assert_eq!(
            stacked_box.bottom,
            plain_box.bottom - stacked.ruby_font_size(),
            "the mark sits outside the ruby layer"
        );
    }

    #[test]
    fn horizontal_emphasis_tracks_eligible_unicode_characters() {
        init_state();
        let mut span = make_span("A😀。 B", 0.0);
        span.text_emphasis = TextEmphasis::FilledDot;
        let paragraph = make_paragraph(vec![span], 0.0);
        let mut style = skia::textlayout::TextStyle::default();
        style.set_font_size(16.0);
        let mut fonts = skia::textlayout::FontCollection::new();
        fonts.set_default_font_manager(skia::FontMgr::new(), None);
        let mut builder = ParagraphBuilder::new(&ParagraphStyle::default(), &fonts);
        let (texts, _) = paragraph.layout_span_texts();
        for (span, text) in paragraph.children().iter().zip(texts) {
            builder.push_style(&style);
            add_horizontal_span(&mut builder, span, &text, &[], &[], &style, &fonts);
        }
        let mut laid_out = builder.build();
        laid_out.layout(200.0);

        let placements = horizontal_emphasis_placements(
            &paragraph,
            &HorizontalOffsets::new(&paragraph),
            &laid_out,
        );
        assert_eq!(placements.len(), 3, "A, emoji and B receive one mark each");
        assert!(placements
            .iter()
            .all(|placement| placement.rect.width() > 0.0));
        assert!(horizontal_span_style(&laid_out, &horizontal_span_ranges(&paragraph)[0]).is_some());
    }

    #[test]
    fn horizontal_emphasis_recovers_each_non_ascii_span_style() {
        init_state();
        let mut first = make_span("漢", 0.0);
        first.text_emphasis = TextEmphasis::FilledDot;
        let mut second = make_span("字", 0.0);
        second.text_emphasis = TextEmphasis::OpenCircle;
        let paragraph = make_paragraph(vec![first, second], 0.0);
        let mut fonts = skia::textlayout::FontCollection::new();
        fonts.set_default_font_manager(skia::FontMgr::new(), None);
        let mut builder = ParagraphBuilder::new(&ParagraphStyle::default(), &fonts);
        let (texts, _) = paragraph.layout_span_texts();
        for (index, (span, text)) in paragraph.children().iter().zip(texts).enumerate() {
            let mut style = skia::textlayout::TextStyle::default();
            style.set_font_size(if index == 0 { 16.0 } else { 24.0 });
            builder.push_style(&style);
            add_horizontal_span(&mut builder, span, &text, &[], &[], &style, &fonts);
        }
        let mut laid_out = builder.build();
        laid_out.layout(200.0);

        let ranges = horizontal_span_ranges(&paragraph);
        assert_eq!(
            horizontal_span_style(&laid_out, &ranges[0])
                .unwrap()
                .font_size(),
            16.0
        );
        assert_eq!(
            horizontal_span_style(&laid_out, &ranges[1])
                .unwrap()
                .font_size(),
            24.0
        );
    }
}
