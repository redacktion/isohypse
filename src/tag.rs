
pub const FULL_TAG_LENGTH: usize = 64;
pub const DISPLAY_TAG_LENGTH: usize = 7;
pub const MIN_TAG_LENGTH: usize = 4;

fn normalize_for_tag(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches([' ', '\t', '\r']));
    }
    out
}

pub fn full_tag(text: &str) -> String {
    let normalized = normalize_for_tag(text);
    blake3::hash(normalized.as_bytes()).to_hex().to_string()
}

pub fn display_tag(text: &str) -> String {
    full_tag(text)[..DISPLAY_TAG_LENGTH].to_string()
}

pub fn tag_matches(cited: &str, full: &str) -> bool {
    cited.len() >= MIN_TAG_LENGTH && full.starts_with(&cited.to_ascii_lowercase())
}

pub fn is_valid_tag(tag: &str) -> bool {
    (MIN_TAG_LENGTH..=FULL_TAG_LENGTH).contains(&tag.len())
        && tag.bytes().all(|b| b.is_ascii_hexdigit())
}
