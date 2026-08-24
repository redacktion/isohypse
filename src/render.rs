use crate::patcher::ApplyReport;

pub fn apply_report(report: &ApplyReport, quiet: bool) -> String {
    let mut out = String::new();
    for section in &report.sections {
        match (&section.op[..], &section.dest, &section.new_tag) {
            ("delete", _, _) => out.push_str(&format!("deleted [{}]\n", section.path)),
            ("create", _, Some(tag)) => {
                out.push_str(&format!("created [{}#{tag}]\n", section.path));
            }
            ("move", Some(dest), Some(tag)) => {
                out.push_str(&format!("moved [{}] -> [{dest}#{tag}]\n", section.path))
            }
            (_, _, Some(tag)) => {
                out.push_str(&format!("updated [{}#{tag}]", section.path));
                match section.first_changed_line {
                    Some(line) => out.push_str(&format!(
                        "  (chain the next edit on this tag; first change at line {line})\n"
                    )),
                    None => out.push('\n'),
                }
                if !section.shifts.is_empty() {
                    let rendered = section
                        .shifts
                        .iter()
                        .map(|(after, delta)| format!("{delta:+} after line {after}"))
                        .collect::<Vec<String>>()
                        .join(", ");
                    out.push_str(&format!("  (line shift, old numbering: {rendered})\n"));
                }
            }
            _ => out.push_str(&format!("updated [{}]\n", section.path)),
        }
        if section.recovered {
            out.push_str("(recovered: replayed onto the stored snapshot and 3-way merged)\n");
        }
        for resolution in &section.block_resolutions {
            out.push_str(&format!(
                "block at {} resolved to {}..{}\n",
                resolution.anchor_line, resolution.start, resolution.end
            ));
        }
        for change in &section.structural {
            out.push_str(&format!("{change}\n"));
        }
        if !quiet {
            out.push_str(&section.preview);
        }
    }
    for warning in &report.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out
}

pub fn check_report(report: &ApplyReport) -> String {
    let mut out = String::from("check only — nothing was written\n");
    out.push_str(&apply_report(report, false));
    out
}
