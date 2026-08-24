#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineEnding {
    Lf,
    CrLf,
}

pub fn detect_line_ending(content: &str) -> LineEnding {
    let lf = content.find('\n');
    let crlf = content.find("\r\n");
    match (lf, crlf) {
        (Some(l), Some(c)) if c < l => LineEnding::CrLf,
        (Some(_), _) => LineEnding::Lf,
        _ => LineEnding::Lf,
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    match normalize_to_lf_cow(text) {
        std::borrow::Cow::Borrowed(s) => s.to_string(),
        std::borrow::Cow::Owned(s) => s,
    }
}

pub fn normalize_to_lf_cow(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

pub fn borrowed_lines(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let trailing_newline = text.ends_with('\n');
    let body = if trailing_newline { &text[..text.len() - 1] } else { text };
    (body.split('\n').collect(), trailing_newline)
}

pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    }
}

pub struct BomSplit<'a> {
    pub bom: &'a str,
    pub text: &'a str,
}

pub fn strip_bom(content: &str) -> BomSplit<'_> {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => BomSplit { bom: "\u{FEFF}", text: rest },
        None => BomSplit { bom: "", text: content },
    }
}

pub struct FileLines {
    pub lines: Vec<String>,
    pub trailing_newline: bool,
}

pub fn split_lines(text: &str) -> FileLines {
    if text.is_empty() {
        return FileLines { lines: Vec::new(), trailing_newline: false };
    }
    let trailing_newline = text.ends_with('\n');
    let body = if trailing_newline { &text[..text.len() - 1] } else { text };
    FileLines { lines: body.split('\n').map(str::to_string).collect(), trailing_newline }
}

pub fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        out.push('\n');
    }
    out
}
