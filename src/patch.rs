
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Gap {
    Bof,
    Eof,
    Before(usize),
    After(usize),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Hunk {
    Replace { range: Range, body: Vec<String> },
    ReplaceBlock { anchor: usize, body: Vec<String> },
    Insert { gap: Gap, body: Vec<String> },
    Cut { range: Range },
    CutBlock { anchor: usize },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileOp {
    Rem,
    Mv(String),
}

#[derive(Clone, Debug)]
pub struct SourcedHunk {
    pub hunk: Hunk,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub path: String,
    pub tag: Option<String>,
    pub hunks: Vec<SourcedHunk>,
    pub file_op: Option<FileOp>,
    pub header_line: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Patch {
    pub sections: Vec<Section>,
    pub warnings: Vec<String>,
}
