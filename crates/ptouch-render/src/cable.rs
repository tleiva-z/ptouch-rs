// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Cable flag and wrap layouts.
//!
//! A flag is two copies of the same text with a blank middle that wraps the
//! cable. A one-pixel dotted line marks the middle of that gap so the fold
//! lines up with the cable. The far copy is rotated 180 degrees so both ends
//! read upright once the label is folded. A wrap is a single text block long
//! enough to go around the cable plus an overlap.
//!
//! Layouts are [`LabelElement`] lists, so the existing preview and print path
//! renders them without a second rasterizer.

use std::io::Cursor;

use crate::RenderError;
use crate::Result;
use crate::bitmap::LabelBitmap;
use crate::document::LabelElement;
use crate::text::{TextAlign, TextRenderer};

/// Design resolution used by the PT-D600 and the other 180 dpi heads.
pub const DESIGN_DPI: u16 = 180;

/// Extra blank millimetres added to the cable diameter on a flag.
pub const DEFAULT_SLACK_MM: f64 = 2.0;

/// Extra millimetres past the circumference on a wrap label.
pub const DEFAULT_OVERLAP_MM: f64 = 10.0;

/// Brother PT-D600 "Flag" auto-format length.
pub const BROTHER_FLAG_MM: f64 = 90.0;

/// Brother PT-D600 "Cable Wrap" auto-format length.
pub const BROTHER_WRAP_MM: f64 = 39.0;

/// Most lines that still fit on 12 mm tape. A fourth line is unreadably small.
pub const MAX_CABLE_LINES: usize = 3;

/// How a cable label is shaped along the tape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CableStyle {
    /// Two text legs and a center gap of `diameter_mm + slack_mm`.
    ///
    /// `length_mm` forces the whole label to that length (the Brother flag
    /// preset is 90 mm). `None` sizes the legs to the text.
    Flag {
        diameter_mm: f64,
        slack_mm: f64,
        length_mm: Option<f64>,
    },
    /// One text block. The span is `π × diameter + overlap`, unless
    /// `length_mm` is set (the Brother wrap preset is 39 mm).
    Wrap {
        diameter_mm: f64,
        overlap_mm: f64,
        length_mm: Option<f64>,
    },
}

impl CableStyle {
    /// Reject non-positive diameters and negative slack or overlap.
    pub fn validate(self) -> Result<()> {
        match self {
            CableStyle::Flag {
                diameter_mm,
                slack_mm,
                length_mm,
            } => {
                require_positive("diameter", diameter_mm)?;
                require_non_negative("slack", slack_mm)?;
                if let Some(length) = length_mm {
                    require_positive("length", length)?;
                    let gap = diameter_mm + slack_mm;
                    if gap >= length {
                        return Err(RenderError::Layout(format!(
                            "cable gap {gap:.1} mm does not fit in a {length:.1} mm flag"
                        )));
                    }
                }
            }
            CableStyle::Wrap {
                diameter_mm,
                overlap_mm,
                length_mm,
            } => {
                require_positive("diameter", diameter_mm)?;
                require_non_negative("overlap", overlap_mm)?;
                if let Some(length) = length_mm {
                    require_positive("length", length)?;
                }
            }
        }
        Ok(())
    }
}

/// Convert millimetres along the tape to pixels at `dpi`, rounded.
pub fn mm_to_px(mm: f64, dpi: u16) -> u32 {
    if mm <= 0.0 || dpi == 0 {
        return 0;
    }
    (mm * f64::from(dpi) / 25.4).round() as u32
}

/// Blank pixels between the two legs of a flag.
pub fn flag_gap_px(diameter_mm: f64, slack_mm: f64, dpi: u16) -> u32 {
    mm_to_px(diameter_mm + slack_mm, dpi)
}

/// Pixels needed to wrap a cable of `diameter_mm` plus `overlap_mm`.
pub fn wrap_span_px(diameter_mm: f64, overlap_mm: f64, dpi: u16) -> u32 {
    mm_to_px(std::f64::consts::PI * diameter_mm + overlap_mm, dpi)
}

/// `prefix` plus `count` numbers starting at `from`.
///
/// `digits` zero-pads the number. `0` prints the number as-is.
pub fn expand_ids(prefix: &str, from: u32, count: u32, digits: u32) -> Vec<String> {
    (0..count)
        .map(|offset| {
            let n = from.saturating_add(offset);
            if digits == 0 {
                format!("{prefix}{n}")
            } else {
                format!("{prefix}{n:0width$}", width = digits as usize)
            }
        })
        .collect()
}

/// Split one label on `|`. At most [`MAX_CABLE_LINES`] fields, none of them empty.
pub fn split_cable_lines(text: &str) -> Result<Vec<String>> {
    let lines: Vec<String> = text
        .split('|')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    if lines.is_empty() {
        return Err(RenderError::Layout(
            "una etiqueta de cable no puede estar vacía".into(),
        ));
    }
    if lines.len() > MAX_CABLE_LINES {
        return Err(RenderError::Layout(format!(
            "una etiqueta admite como máximo {MAX_CABLE_LINES} líneas"
        )));
    }
    let raw_fields = text.split('|').count();
    if raw_fields != lines.len() {
        return Err(RenderError::Layout(
            "una línea de la etiqueta está vacía".into(),
        ));
    }
    Ok(lines)
}

/// Y origin of each band, centered in `tape_px`.
///
/// The bands keep the given heights. Leftover tape is split above and below,
/// with the extra pixel (if any) below.
pub fn band_origins(tape_px: u32, heights: &[u32]) -> Result<Vec<u32>> {
    if heights.is_empty() || heights.len() > MAX_CABLE_LINES {
        return Err(RenderError::Layout(format!(
            "una etiqueta admite de 1 a {MAX_CABLE_LINES} líneas"
        )));
    }
    if heights.contains(&0) {
        return Err(RenderError::Layout(
            "el alto de una línea tiene que ser mayor que 0".into(),
        ));
    }
    if tape_px == 0 {
        return Err(RenderError::Layout(
            "el alto de la cinta tiene que ser mayor que 0".into(),
        ));
    }
    let sum: u32 = heights.iter().sum();
    if sum > tape_px {
        return Err(RenderError::Layout(format!(
            "los altos suman {sum} px y la cinta tiene {tape_px} px"
        )));
    }
    let mut y = (tape_px - sum) / 2;
    let mut origins = Vec::with_capacity(heights.len());
    for height in heights {
        origins.push(y);
        y += height;
    }
    Ok(origins)
}

/// Stack already-rendered line bitmaps and center them on the tape.
pub fn stack_bitmaps(tape_px: u32, bands: &[LabelBitmap]) -> Result<LabelBitmap> {
    let heights: Vec<u32> = bands.iter().map(LabelBitmap::height).collect();
    let origins = band_origins(tape_px, &heights)?;
    let width = bands
        .iter()
        .map(LabelBitmap::width)
        .max()
        .unwrap_or(1)
        .max(1);
    let mut canvas = LabelBitmap::new(width, tape_px);
    for (band, y) in bands.iter().zip(origins) {
        let x = (width - band.width()) / 2;
        blit(&mut canvas, band, x, y);
    }
    Ok(canvas)
}

/// One label per non-empty line.
///
/// A comma-separated line keeps the first field. A first field of `id`,
/// `text`, `cable`, `label`, or `etiqueta` is treated as a header and dropped.
/// Lines starting with `#` are comments.
pub fn parse_label_list(text: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut saw_data = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let field = line.split(',').next().unwrap_or(line).trim();
        if field.is_empty() {
            continue;
        }
        if !saw_data && is_header(field) {
            saw_data = true;
            continue;
        }
        saw_data = true;
        labels.push(field.to_string());
    }
    labels
}

/// Turn spreadsheet rows into cable labels.
///
/// Each row is one label and each column is a line, joined with `|`.
/// `skip_header` drops the first non-empty row. Completely empty rows are
/// ignored. A blank cell, a row with a different width, or more than
/// [`MAX_CABLE_LINES`] columns is an error. Row numbers are 1-based.
pub fn labels_from_table(rows: &[Vec<String>], skip_header: bool) -> Result<Vec<String>> {
    let mut labels = Vec::new();
    let mut width = 0;
    let mut header_pending = skip_header;
    for (index, row) in rows.iter().enumerate() {
        let cells: Vec<String> = row.iter().map(|cell| cell.trim().to_string()).collect();
        if cells.iter().all(|cell| cell.is_empty()) {
            continue;
        }
        let row_no = index + 1;
        if header_pending {
            header_pending = false;
            continue;
        }
        if width == 0 {
            width = cells.len();
            if !(1..=MAX_CABLE_LINES).contains(&width) {
                return Err(RenderError::Layout(format!(
                    "la tabla tiene {width} columnas y una etiqueta admite como máximo {MAX_CABLE_LINES} líneas"
                )));
            }
        }
        if cells.len() != width {
            return Err(RenderError::Layout(format!(
                "la fila {row_no} tiene {} columnas y la tabla tiene {width}",
                cells.len()
            )));
        }
        if cells.iter().any(|cell| cell.is_empty()) {
            return Err(RenderError::Layout(format!(
                "la fila {row_no} tiene una celda vacía"
            )));
        }
        labels.push(cells.join("|"));
    }
    if labels.is_empty() {
        return Err(RenderError::Layout("la tabla no tiene etiquetas".into()));
    }
    Ok(labels)
}

/// Parse a pasted or saved table.
///
/// A tab marks text copied from a spreadsheet. Otherwise a semicolon is used
/// when it is more common than a comma, which is how Excel writes CSV in
/// Spanish. Quoted fields keep the delimiter and doubled quotes.
pub fn parse_label_table(text: &str, skip_header: bool) -> Result<Vec<String>> {
    let rows = split_delimited(text, detect_delimiter(text))?;
    labels_from_table(&rows, skip_header)
}

/// Write rows with tabs so a spreadsheet paste round-trips through
/// [`parse_label_table`].
pub fn table_as_text(rows: &[Vec<String>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|cell| {
                    if cell.contains(['\t', '\n', '\r', '"']) {
                        format!("\"{}\"", cell.replace('"', "\"\""))
                    } else {
                        cell.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whole spreadsheet numbers stay integers (`1`, not `1.0`).
pub fn format_sheet_number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value.trunc() as i64)
    } else {
        format!("{value}")
    }
}

fn detect_delimiter(text: &str) -> char {
    let mut tabs = 0;
    let mut semis = 0;
    let mut commas = 0;
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            '\t' => tabs += 1,
            ';' => semis += 1,
            ',' => commas += 1,
            _ => {}
        }
    }
    if tabs > 0 {
        '\t'
    } else if semis > commas {
        ';'
    } else {
        ','
    }
}

fn split_delimited(text: &str, delimiter: char) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    cell.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cell.push(ch);
            }
            continue;
        }
        if ch == '"' {
            in_quotes = true;
            continue;
        }
        if ch == delimiter {
            row.push(std::mem::take(&mut cell));
            continue;
        }
        if ch == '\n' || ch == '\r' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut cell));
            rows.push(std::mem::take(&mut row));
            continue;
        }
        cell.push(ch);
    }
    if in_quotes {
        return Err(RenderError::Layout(
            "hay comillas sin cerrar en la tabla".into(),
        ));
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    Ok(rows)
}

/// Build one flag from a known text width.
///
/// `text_width_px` is the rendered width of `text` at 0 degrees. A 180 degree
/// turn keeps that width, so the same value pads both legs. When `total_px` is
/// `None` the legs are exactly the text. When it is set, each leg is padded so
/// the label (text + pads + gap) equals that length, unless the text is already
/// wider than a leg. The gap keeps a dotted fold line at its center.
pub fn flag_elements(
    text: &str,
    text_width_px: u32,
    gap_px: u32,
    total_px: Option<u32>,
) -> Vec<LabelElement> {
    flag_with(&text_element(text, 0.0), text_width_px, gap_px, total_px)
}

/// Build one wrap label, padding so the span is at least `span_px`.
pub fn wrap_elements(text: &str, text_width_px: u32, span_px: u32) -> Vec<LabelElement> {
    wrap_with(&text_element(text, 0.0), text_width_px, span_px)
}

/// Concatenate labels and insert a cut mark between them.
pub fn join_with_cutmarks(labels: Vec<Vec<LabelElement>>) -> Vec<LabelElement> {
    let mut out = Vec::new();
    for (index, label) in labels.into_iter().enumerate() {
        if index > 0 {
            out.push(LabelElement::CutMark);
        }
        out.extend(label);
    }
    out
}

/// Lay out every label. `text_widths` must have one entry per text.
pub fn layout_labels(
    texts: &[String],
    style: CableStyle,
    dpi: u16,
    text_widths: &[u32],
) -> Result<Vec<LabelElement>> {
    style.validate()?;
    if texts.is_empty() {
        return Err(RenderError::Layout("no cable labels to build".to_string()));
    }
    if texts.len() != text_widths.len() {
        return Err(RenderError::Layout(
            "text width count does not match the label count".to_string(),
        ));
    }

    let mut labels = Vec::with_capacity(texts.len());
    for (text, width) in texts.iter().zip(text_widths) {
        if text.is_empty() {
            return Err(RenderError::Layout(
                "a cable label cannot be empty".to_string(),
            ));
        }
        let elements = match style {
            CableStyle::Flag {
                diameter_mm,
                slack_mm,
                length_mm,
            } => flag_elements(
                text,
                *width,
                flag_gap_px(diameter_mm, slack_mm, dpi),
                length_mm.map(|mm| mm_to_px(mm, dpi)),
            ),
            CableStyle::Wrap {
                diameter_mm,
                overlap_mm,
                length_mm,
            } => {
                let span = match length_mm {
                    Some(mm) => mm_to_px(mm, dpi),
                    None => wrap_span_px(diameter_mm, overlap_mm, dpi),
                };
                wrap_elements(text, *width, span)
            }
        };
        labels.push(elements);
    }
    Ok(join_with_cutmarks(labels))
}

/// Measure each label with `renderer` and lay the strip out.
///
/// `line_heights` is the pixel height of each line. `None` shares the tape
/// height equally. When it is set, every label must have that many `|` fields.
pub fn layout_rendered(
    texts: &[String],
    style: CableStyle,
    dpi: u16,
    tape_width_px: u32,
    font_name: &str,
    renderer: &mut TextRenderer,
    line_heights: Option<&[u32]>,
) -> Result<Vec<LabelElement>> {
    if tape_width_px == 0 {
        return Err(RenderError::Text(
            "tape width must be greater than 0".into(),
        ));
    }
    style.validate()?;
    if texts.is_empty() {
        return Err(RenderError::Layout("no cable labels to build".to_string()));
    }

    if let Some(heights) = line_heights {
        band_origins(tape_width_px, heights)?;
        let mut labels = Vec::with_capacity(texts.len());
        for text in texts {
            let lines = split_cable_lines(text)?;
            if lines.len() != heights.len() {
                return Err(RenderError::Layout(format!(
                    "se indicaron {} altos pero la etiqueta tiene {} línea(s)",
                    heights.len(),
                    lines.len()
                )));
            }
            let bitmap = render_stacked(renderer, &lines, heights, tape_width_px, font_name)?;
            let element = image_element(&bitmap)?;
            labels.push(layout_block(&element, bitmap.width(), style, dpi)?);
        }
        return Ok(join_with_cutmarks(labels));
    }

    let mut normalized = Vec::with_capacity(texts.len());
    for text in texts {
        normalized.push(split_cable_lines(text)?.join("\n"));
    }
    let mut widths = Vec::with_capacity(normalized.len());
    for text in &normalized {
        widths.push(measure_text(renderer, text, tape_width_px, font_name)?);
    }
    layout_labels(&normalized, style, dpi, &widths)
}

fn measure_text(
    renderer: &mut TextRenderer,
    text: &str,
    tape_width_px: u32,
    font_name: &str,
) -> Result<u32> {
    let lines: Vec<&str> = text.lines().collect();
    let lines = if lines.is_empty() { vec![text] } else { lines };
    let bitmap =
        renderer.render_text(&lines, tape_width_px, font_name, None, 0, TextAlign::Center)?;
    Ok(bitmap.width())
}

fn flag_with(
    block: &LabelElement,
    width: u32,
    gap_px: u32,
    total_px: Option<u32>,
) -> Vec<LabelElement> {
    let mut out = Vec::new();
    match total_px {
        None => {
            out.push(with_rotation(block, 0.0));
            push_flag_gap(&mut out, gap_px);
            out.push(with_rotation(block, 180.0));
        }
        Some(total) => {
            let body = total.saturating_sub(gap_px);
            let near = body / 2;
            let far = body - near;
            push_oriented(&mut out, block, 0.0, width, near);
            push_flag_gap(&mut out, gap_px);
            push_oriented(&mut out, block, 180.0, width, far);
        }
    }
    out
}

fn wrap_with(block: &LabelElement, width: u32, span_px: u32) -> Vec<LabelElement> {
    let extra = span_px.saturating_sub(width);
    let left = extra / 2;
    let right = extra - left;
    let mut out = Vec::new();
    push_pad(&mut out, left);
    out.push(with_rotation(block, 0.0));
    push_pad(&mut out, right);
    out
}

fn layout_block(
    block: &LabelElement,
    width: u32,
    style: CableStyle,
    dpi: u16,
) -> Result<Vec<LabelElement>> {
    Ok(match style {
        CableStyle::Flag {
            diameter_mm,
            slack_mm,
            length_mm,
        } => flag_with(
            block,
            width,
            flag_gap_px(diameter_mm, slack_mm, dpi),
            length_mm.map(|mm| mm_to_px(mm, dpi)),
        ),
        CableStyle::Wrap {
            diameter_mm,
            overlap_mm,
            length_mm,
        } => {
            let span = match length_mm {
                Some(mm) => mm_to_px(mm, dpi),
                None => wrap_span_px(diameter_mm, overlap_mm, dpi),
            };
            wrap_with(block, width, span)
        }
    })
}

fn render_stacked(
    renderer: &mut TextRenderer,
    lines: &[String],
    heights: &[u32],
    tape_width_px: u32,
    font_name: &str,
) -> Result<LabelBitmap> {
    let mut bands = Vec::with_capacity(lines.len());
    for (line, height) in lines.iter().zip(heights) {
        bands.push(renderer.render_text(
            &[line.as_str()],
            *height,
            font_name,
            None,
            0,
            TextAlign::Center,
        )?);
    }
    stack_bitmaps(tape_width_px, &bands)
}

fn image_element(bitmap: &LabelBitmap) -> Result<LabelElement> {
    let rgba = bitmap.to_rgba_image();
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba).write_to(&mut png, image::ImageFormat::Png)?;
    let mut element = LabelElement::image_from_bytes(None, png.into_inner());
    element.set_image_bitmap(Some(bitmap.clone()));
    if let LabelElement::Image { target_height, .. } = &mut element {
        *target_height = Some(bitmap.height());
    }
    Ok(element)
}

fn push_oriented(
    out: &mut Vec<LabelElement>,
    block: &LabelElement,
    rotation: f32,
    text_width_px: u32,
    leg_px: u32,
) {
    let spare = leg_px.saturating_sub(text_width_px);
    let before = spare / 2;
    let after = spare - before;
    push_pad(out, before);
    out.push(with_rotation(block, rotation));
    push_pad(out, after);
}

fn with_rotation(block: &LabelElement, rotation: f32) -> LabelElement {
    let mut block = block.clone();
    match &mut block {
        LabelElement::Text {
            rotation: angle, ..
        }
        | LabelElement::Image {
            rotation: angle, ..
        } => *angle = rotation,
        LabelElement::CutMark | LabelElement::FoldMark | LabelElement::Padding { .. } => {}
    }
    block
}

fn blit(dst: &mut LabelBitmap, src: &LabelBitmap, x: u32, y: u32) {
    for row in 0..src.height() {
        for col in 0..src.width() {
            if src.get_pixel(col, row) {
                dst.set_pixel(x + col, y + row, true);
            }
        }
    }
}

fn push_pad(out: &mut Vec<LabelElement>, pixels: u32) {
    if pixels > 0 {
        out.push(LabelElement::Padding { pixels });
    }
}

/// Split the cable gap around a one-pixel fold mark.
///
/// The mark replaces one pixel of the gap, so the label stays the same length.
/// An even gap leaves the extra blank pixel on the far side.
fn push_flag_gap(out: &mut Vec<LabelElement>, gap_px: u32) {
    if gap_px == 0 {
        return;
    }
    let left = (gap_px - 1) / 2;
    let right = gap_px - 1 - left;
    push_pad(out, left);
    out.push(LabelElement::FoldMark);
    push_pad(out, right);
}

fn text_element(content: &str, rotation: f32) -> LabelElement {
    LabelElement::Text {
        content: content.to_string(),
        font_size: None,
        align: TextAlign::Center,
        rotation,
        flip_h: false,
        flip_v: false,
    }
}

fn require_positive(name: &str, value: f64) -> Result<()> {
    if value > 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(RenderError::Layout(format!(
            "{name} must be greater than 0"
        )))
    }
}

fn require_non_negative(name: &str, value: f64) -> Result<()> {
    if value >= 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(RenderError::Layout(format!(
            "{name} must be zero or greater"
        )))
    }
}

fn is_header(field: &str) -> bool {
    matches!(
        field.to_ascii_lowercase().as_str(),
        "id" | "text" | "cable" | "label" | "etiqueta"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag_style(diameter: f64, length_mm: Option<f64>) -> CableStyle {
        CableStyle::Flag {
            diameter_mm: diameter,
            slack_mm: DEFAULT_SLACK_MM,
            length_mm,
        }
    }

    #[test]
    fn flag_gap_is_diameter_plus_slack() {
        let gap = flag_gap_px(6.0, DEFAULT_SLACK_MM, DESIGN_DPI);
        assert_eq!(gap, mm_to_px(8.0, DESIGN_DPI));
        assert_eq!(gap, 57);

        let elements = flag_elements("CBL-1", 40, gap, None);
        assert!(matches!(
            &elements[0],
            LabelElement::Text { rotation, .. } if *rotation == 0.0
        ));
        let (left, right) = flag_gap_pads(&elements);
        assert_eq!(left + 1 + right, gap);
        assert_eq!(left, (gap - 1) / 2);
        assert!(
            elements
                .iter()
                .any(|element| matches!(element, LabelElement::FoldMark))
        );
        assert!(matches!(
            elements.last(),
            Some(LabelElement::Text { rotation, content, .. })
                if *rotation == 180.0 && content == "CBL-1"
        ));
    }

    #[test]
    fn wrap_span_is_circumference_plus_overlap() {
        let span = wrap_span_px(8.0, DEFAULT_OVERLAP_MM, DESIGN_DPI);
        let expected = mm_to_px(std::f64::consts::PI * 8.0 + 10.0, DESIGN_DPI);
        assert_eq!(span, expected);

        let text_width = 100;
        let elements = wrap_elements("PWR", text_width, span);
        let pads: u32 = elements
            .iter()
            .filter_map(|element| match element {
                LabelElement::Padding { pixels } => Some(*pixels),
                _ => None,
            })
            .sum();
        assert_eq!(pads + text_width, span);
        assert!(matches!(
            elements.iter().find(|element| matches!(element, LabelElement::Text { .. })),
            Some(LabelElement::Text { rotation, .. }) if *rotation == 0.0
        ));
    }

    #[test]
    fn fixed_flag_pads_out_to_brother_length() {
        let total = mm_to_px(BROTHER_FLAG_MM, DESIGN_DPI);
        assert_eq!(total, 638);
        let gap = flag_gap_px(6.0, DEFAULT_SLACK_MM, DESIGN_DPI);
        let text_width = 80;
        let elements = flag_elements("A1", text_width, gap, Some(total));
        let pads: u32 = elements
            .iter()
            .filter_map(|element| match element {
                LabelElement::Padding { pixels } => Some(*pixels),
                _ => None,
            })
            .sum();
        let texts = elements
            .iter()
            .filter(|element| matches!(element, LabelElement::Text { .. }))
            .count();
        let folds = elements
            .iter()
            .filter(|element| matches!(element, LabelElement::FoldMark))
            .count();
        assert_eq!(texts, 2);
        assert_eq!(folds, 1);
        assert_eq!(pads + text_width * 2 + folds as u32, total);
    }

    #[test]
    fn batch_of_three_has_a_cutmark_between_labels() {
        let style = flag_style(6.0, None);
        let texts = vec!["A".into(), "B".into(), "C".into()];
        let elements = layout_labels(&texts, style, DESIGN_DPI, &[20, 20, 20]).unwrap();
        let cuts = elements
            .iter()
            .filter(|element| matches!(element, LabelElement::CutMark))
            .count();
        assert_eq!(cuts, 2);
        assert!(!matches!(elements.first(), Some(LabelElement::CutMark)));
        assert!(!matches!(elements.last(), Some(LabelElement::CutMark)));
    }

    #[test]
    fn sequence_and_list_parsing() {
        assert_eq!(
            expand_ids("CBL-", 1, 3, 3),
            vec!["CBL-001", "CBL-002", "CBL-003"]
        );
        let parsed = parse_label_list("id,note\nLAN-1,sala\n\n# skip\nPWR-2\n");
        assert_eq!(parsed, vec!["LAN-1", "PWR-2"]);
    }

    #[test]
    fn pasted_table_uses_columns_as_lines() {
        let text = "nombre0\taddr\nB1-PR-AF\tADDR: 1\nB1-PR-DV\tADDR: 2\n";
        let labels = parse_label_table(text, true).unwrap();
        assert_eq!(labels, vec!["B1-PR-AF|ADDR: 1", "B1-PR-DV|ADDR: 2"]);
        assert_eq!(format_sheet_number(1.0), "1");
        assert_eq!(format_sheet_number(1.5), "1.5");
    }

    #[test]
    fn semicolon_csv_keeps_quoted_delimiters() {
        let text = "nombre;addr\n\"B1;PR\";\"ADDR: 1\"\n";
        let labels = parse_label_table(text, true).unwrap();
        assert_eq!(labels, vec!["B1;PR|ADDR: 1"]);
        let plain = parse_label_table("CBL-001\nCBL-002\n", false).unwrap();
        assert_eq!(plain, vec!["CBL-001", "CBL-002"]);
    }

    #[test]
    fn four_columns_and_empty_cells_are_rejected() {
        let wide = labels_from_table(
            &[vec!["a".into(), "b".into(), "c".into(), "d".into()]],
            false,
        );
        assert!(wide.is_err());
        let hole = labels_from_table(
            &[
                vec!["nombre0".into(), "addr".into()],
                vec!["B1".into(), "".into()],
            ],
            true,
        )
        .unwrap_err();
        assert!(hole.to_string().contains("fila 2"));
        let short = labels_from_table(
            &[
                vec!["nombre0".into(), "addr".into()],
                vec!["B1-PR-AF".into(), "ADDR: 1".into()],
                vec!["B1-PR-DV".into()],
            ],
            true,
        )
        .unwrap_err();
        assert!(short.to_string().contains("fila 3"));
    }

    #[test]
    fn at_most_three_lines_and_equal_join() {
        assert!(split_cable_lines("a|b|c|d").is_err());
        assert!(split_cable_lines("a|").is_err());
        assert_eq!(
            split_cable_lines("LAN|CBL-001").unwrap().join("\n"),
            "LAN\nCBL-001"
        );
        let elements = flag_elements("LAN\nCBL-001", 40, 10, None);
        assert!(matches!(
            &elements[0],
            LabelElement::Text { content, .. } if content == "LAN\nCBL-001"
        ));
        assert!(matches!(
            elements.last(),
            Some(LabelElement::Text { rotation, content, .. })
                if *rotation == 180.0 && content == "LAN\nCBL-001"
        ));
    }

    fn flag_gap_pads(elements: &[LabelElement]) -> (u32, u32) {
        let mut pads = elements.iter().filter_map(|element| match element {
            LabelElement::Padding { pixels } => Some(*pixels),
            _ => None,
        });
        (pads.next().unwrap_or(0), pads.next().unwrap_or(0))
    }

    #[test]
    fn explicit_heights_are_centered_on_the_tape() {
        let tape = 76;
        let origins = band_origins(tape, &[16, 30]).unwrap();
        assert_eq!(origins, vec![15, 31]);
        assert!(band_origins(tape, &[40, 40]).is_err());

        let stacked = stack_bitmaps(tape, &[solid(10, 16), solid(20, 30)]).unwrap();
        assert_eq!(stacked.width(), 20);
        assert_eq!(stacked.height(), tape);
        assert!(row_is_white(&stacked, 0));
        assert!(row_is_white(&stacked, 14));
        assert!(stacked.get_pixel(5, 15));
        assert!(!stacked.get_pixel(0, 15));
        assert!(stacked.get_pixel(0, 31));
        assert!(stacked.get_pixel(19, 60));
        assert!(row_is_white(&stacked, 61));
        assert!(row_is_white(&stacked, 75));
    }

    fn solid(width: u32, height: u32) -> LabelBitmap {
        let mut bitmap = LabelBitmap::new(width, height);
        for y in 0..height {
            for x in 0..width {
                bitmap.set_pixel(x, y, true);
            }
        }
        bitmap
    }

    fn row_is_white(bitmap: &LabelBitmap, y: u32) -> bool {
        (0..bitmap.width()).all(|x| !bitmap.get_pixel(x, y))
    }

    #[test]
    fn gap_wider_than_fixed_flag_is_rejected() {
        let style = CableStyle::Flag {
            diameter_mm: 80.0,
            slack_mm: 20.0,
            length_mm: Some(BROTHER_FLAG_MM),
        };
        let err = layout_labels(&["X".into()], style, DESIGN_DPI, &[10]).unwrap_err();
        assert!(err.to_string().contains("does not fit"));
    }
}
