//! A minimal, dependency-free PDF 1.4 writer: enough to lay out a printable
//! A4 document of text, lines and boxes in the PDF base fonts (Helvetica and
//! Helvetica-Bold), which every PDF reader ships, so no font is embedded.
//!
//! Used by [`super::challan`]. The base fonts only cover WinAnsi (Latin-1),
//! so [`to_win_ansi`] writes `₹` as `Rs.` and any other character outside
//! Latin-1 as `?`; Hindi/Marathi text would need an embedded font.

/// A4 portrait, in PDF points (1/72 inch).
pub const PAGE_WIDTH: f32 = 595.0;
pub const PAGE_HEIGHT: f32 = 842.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
}

impl Font {
    fn resource(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
        }
    }
}

/// One page's drawing operators, in PDF content-stream syntax. Coordinates
/// are points from the page's top-left corner (converted to PDF's
/// bottom-left origin as they are written).
#[derive(Debug, Default)]
pub struct Page {
    ops: String,
}

impl Page {
    /// Draw `text` with its baseline at `y` points from the top.
    pub fn text(&mut self, x: f32, y: f32, size: f32, font: Font, text: &str) {
        self.ops.push_str(&format!(
            "BT /{} {} Tf {} {} Td ({}) Tj ET\n",
            font.resource(),
            num(size),
            num(x),
            num(PAGE_HEIGHT - y),
            escape(&to_win_ansi(text))
        ));
    }

    /// Draw `text` right-aligned so that it ends at `right_x`.
    pub fn text_right(&mut self, right_x: f32, y: f32, size: f32, font: Font, text: &str) {
        let x = right_x - text_width(text, size, font);
        self.text(x, y, size, font, text);
    }

    /// A straight line, `width` points thick.
    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32) {
        self.ops.push_str(&format!(
            "{} w {} {} m {} {} l S\n",
            num(width),
            num(x1),
            num(PAGE_HEIGHT - y1),
            num(x2),
            num(PAGE_HEIGHT - y2)
        ));
    }

    /// An unfilled rectangle whose top-left corner is at (`x`, `y`).
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, width: f32) {
        self.ops.push_str(&format!(
            "{} w {} {} {} {} re S\n",
            num(width),
            num(x),
            num(PAGE_HEIGHT - y - h),
            num(w),
            num(h)
        ));
    }

    /// A rectangle filled light grey, for table header bands.
    pub fn shade(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.ops.push_str(&format!(
            "q 0.92 g {} {} {} {} re f Q\n",
            num(x),
            num(PAGE_HEIGHT - y - h),
            num(w),
            num(h)
        ));
    }
}

/// Assemble `pages` into a complete PDF file. `title` goes in the document
/// info dictionary (what a viewer shows in its title bar).
pub fn render(pages: &[Page], title: &str) -> Vec<u8> {
    // Object numbers: 1 catalog, 2 page tree, 3 regular font, 4 bold font,
    // 5 info, then a (page, content) pair per page.
    let page_count = pages.len().max(1);
    let first_page_obj = 6;
    let mut objects: Vec<String> = Vec::new();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
    let kids: Vec<String> = (0..page_count)
        .map(|i| format!("{} 0 R", first_page_obj + 2 * i))
        .collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        page_count
    ));
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
    );
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
            .to_string(),
    );
    objects.push(format!(
        "<< /Title ({}) /Producer (logistics-system) >>",
        escape(&to_win_ansi(title))
    ));
    let empty = Page::default();
    for i in 0..page_count {
        let page = pages.get(i).unwrap_or(&empty);
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] \
             /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
            num(PAGE_WIDTH),
            num(PAGE_HEIGHT),
            first_page_obj + 2 * i + 1
        ));
        // Written one byte per char below, so the length is the char count
        // (not the UTF-8 byte length, which differs for Latin-1 letters).
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}endstream",
            page.ops.chars().count(),
            page.ops
        ));
    }

    // Content streams hold only WinAnsi bytes (see `to_win_ansi`), written
    // one byte per char, so string lengths and byte offsets agree.
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n", i + 1).bytes());
        out.extend(body.chars().map(|c| c as u32 as u8));
        out.extend(b"\nendobj\n");
    }
    let xref_at = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
    for off in offsets {
        out.extend(format!("{:010} 00000 n \n", off).bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 5 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_at
        )
        .bytes(),
    );
    out
}

/// Map text onto what the WinAnsi-encoded base fonts can show: Latin-1
/// passes through, `₹` becomes `Rs.`, typographic quotes and dashes become
/// their ASCII forms, and anything else becomes `?`. Control characters
/// (newlines included) become spaces.
pub fn to_win_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{20B9}' => out.push_str("Rs."),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{2013}' | '\u{2014}' => out.push('-'),
            c if c.is_control() => out.push(' '),
            c if (c as u32) < 0x100 => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

/// Escape a PDF literal string's delimiters.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '(' | ')' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A number as PDF writes it: at most two decimals, no trailing zeros.
fn num(v: f32) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Approximate width of `text` in points. Uses Helvetica's real widths for
/// the common classes of character (narrow, wide, digits, the rest), which
/// is close enough to wrap and right-align text without a full metrics table.
pub fn text_width(text: &str, size: f32, font: Font) -> f32 {
    let bold_extra = if font == Font::Bold { 0.03 } else { 0.0 };
    let units: f32 = to_win_ansi(text)
        .chars()
        .map(|c| match c {
            'i' | 'j' | 'l' | '\'' | '|' | '.' | ',' | ':' | ';' | '!' | 'I' => 0.24,
            ' ' | 'f' | 't' | 'r' | '(' | ')' | '/' | '-' => 0.32,
            'm' | 'M' | 'W' | 'w' | '@' => 0.85,
            '0'..='9' => 0.556,
            c if c.is_ascii_uppercase() => 0.68,
            _ => 0.54,
        })
        .map(|w| w + bold_extra)
        .sum();
    units * size
}

/// Split `text` into lines no wider than `max_width` points, breaking at
/// spaces (a single word longer than the line is cut).
pub fn wrap(text: &str, size: f32, font: Font, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if text_width(&candidate, size, font) <= max_width {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // A word wider than the whole line: cut it into pieces that fit.
        let mut piece = String::new();
        for c in word.chars() {
            piece.push(c);
            if text_width(&piece, size, font) > max_width && piece.chars().count() > 1 {
                let last = piece.pop().unwrap();
                lines.push(std::mem::take(&mut piece));
                piece.push(last);
            }
        }
        line = piece;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PDF as text with one char per byte (non-ASCII bytes become `~`),
    /// so string indices are byte offsets into the file.
    fn as_text(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|&b| if b.is_ascii() { b as char } else { '~' })
            .collect()
    }

    #[test]
    fn test_render_produces_a_well_formed_pdf() {
        let mut page = Page::default();
        page.text(40.0, 60.0, 12.0, Font::Bold, "Delivery Challan");
        page.line(40.0, 70.0, 555.0, 70.0, 1.0);
        let pdf = render(&[page], "Challan");
        let text = as_text(&pdf);

        assert!(text.starts_with("%PDF-1.4\n"));
        assert!(text.ends_with("%%EOF\n"));
        assert!(text.contains("(Delivery Challan) Tj"));
        assert!(text.contains("/BaseFont /Helvetica-Bold"));

        // startxref points at the xref table, and every xref entry points at
        // the start of its object.
        let startxref: usize = text
            .rsplit("startxref\n")
            .next()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(text[startxref..].starts_with("xref\n"));
        let entries: Vec<usize> = text[startxref..]
            .lines()
            .skip(3) // "xref", "0 N", the free entry
            .take_while(|l| l.ends_with(" n "))
            .map(|l| l[..10].parse().unwrap())
            .collect();
        assert_eq!(entries.len(), 7, "5 shared objects + page + content");
        for (i, off) in entries.iter().enumerate() {
            assert!(
                text[*off..].starts_with(&format!("{} 0 obj\n", i + 1)),
                "xref entry {} is off",
                i + 1
            );
        }
    }

    #[test]
    fn test_stream_length_matches_its_content() {
        let mut page = Page::default();
        page.text(10.0, 10.0, 10.0, Font::Regular, "Größe (A) \\ B");
        let pdf = as_text(&render(&[page], "t"));
        let start = pdf.find("stream\n").unwrap() + "stream\n".len();
        let end = pdf.find("endstream").unwrap();
        let declared: usize = pdf[..start]
            .rsplit("/Length ")
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(declared, end - start);
    }

    #[test]
    fn test_one_page_object_per_page() {
        let pages = vec![Page::default(), Page::default(), Page::default()];
        let pdf = as_text(&render(&pages, "t"));
        assert!(pdf.contains("/Count 3"));
        assert_eq!(pdf.matches("/Type /Page ").count(), 3);
    }

    #[test]
    fn test_text_is_escaped_and_mapped_to_win_ansi() {
        assert_eq!(escape("a(b)c\\d"), "a\\(b\\)c\\\\d");
        assert_eq!(to_win_ansi("₹500 – “ok”"), "Rs.500 - \"ok\"");
        assert_eq!(to_win_ansi("नमस्ते"), "??????");
        assert_eq!(to_win_ansi("a\nb"), "a b");
        assert_eq!(to_win_ansi("Größe"), "Größe");
    }

    #[test]
    fn test_numbers_are_written_compactly() {
        assert_eq!(num(842.0), "842");
        assert_eq!(num(12.5), "12.5");
        assert_eq!(num(0.333), "0.33");
    }

    #[test]
    fn test_wrap_keeps_lines_within_width() {
        let text = "Plot 14, MIDC Industrial Area, Chakan, Pune, Maharashtra 410501";
        let lines = wrap(text, 10.0, Font::Regular, 120.0);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(
                text_width(l, 10.0, Font::Regular) <= 120.0,
                "{l:?} too wide"
            );
        }
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn test_wrap_cuts_a_word_longer_than_the_line() {
        let lines = wrap("ABCDEFGHIJKLMNOPQRSTUVWXYZ", 10.0, Font::Regular, 50.0);
        assert!(lines.len() > 1);
        assert_eq!(lines.concat(), "ABCDEFGHIJKLMNOPQRSTUVWXYZ");
    }

    #[test]
    fn test_wrap_of_empty_text_is_no_lines() {
        assert!(wrap("   ", 10.0, Font::Regular, 100.0).is_empty());
    }
}
