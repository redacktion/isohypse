pub enum Recovery {
    Merged(String),
    Conflict(String),
}

pub fn replay_onto_live(base: &str, live: &str, patched_base: &str) -> Recovery {
    match diffy::merge(base, live, patched_base) {
        Ok(merged) => Recovery::Merged(merged),
        Err(conflicted) => {
            let mut region = String::new();
            let mut inside = false;
            for line in conflicted.lines() {
                if line.starts_with("<<<<<<<") {
                    inside = true;
                }
                if inside {
                    region.push_str(line);
                    region.push('\n');
                }
                if line.starts_with(">>>>>>>") {
                    break;
                }
            }
            Recovery::Conflict(region)
        }
    }
}
