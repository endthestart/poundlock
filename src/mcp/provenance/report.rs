//! What a folder's record says: the applied patches and the changes made outside them.

use super::*;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    pub directory: String,
    pub applied: Vec<Applied>,
    pub outside: Vec<OutsideChange>,
    /// Records whose manifest was missing, so a gap could not be inspected.
    pub unreadable: usize,
}

impl Report {
    /// Whether every change since the first patch came from job patches.
    pub fn clean(&self) -> bool {
        self.outside.is_empty() && self.unreadable == 0
    }

    /// Applied patches whose job's checks had not passed.
    pub fn without_evidence(&self) -> Vec<&Applied> {
        self.applied
            .iter()
            .filter(|entry| entry.verification.as_deref() != Some("passed"))
            .collect()
    }

    /// The report for a person or an agent to read.
    pub fn text(&self) -> String {
        if self.applied.is_empty() {
            return format!(
                "No job patches have been applied to {} through Poundlock.",
                self.directory
            );
        }
        let mut lines = vec![format!(
            "{} job patch(es) applied to {}: {}.",
            self.applied.len(),
            self.directory,
            self.applied
                .iter()
                .map(|entry| entry.job_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )];
        if self.clean() {
            lines.push("Every change since the first one came from those patches.".to_owned());
        }
        let unproven = self.without_evidence();
        if unproven.is_empty() {
            lines.push("Each patch's job passed its checks.".to_owned());
        } else {
            lines.push(format!(
                "Applied without passing checks: {}.",
                unproven
                    .iter()
                    .map(|entry| format!(
                        "job {} ({})",
                        entry.job_id,
                        entry.verification.as_deref().unwrap_or("not known")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for gap in &self.outside {
            let when = match &gap.before_job {
                Some(next) => format!("between job {} and job {next}", gap.after_job),
                None => format!("since job {}", gap.after_job),
            };
            let mut parts = Vec::new();
            for (label, files) in [
                ("added", &gap.added),
                ("changed", &gap.changed),
                ("removed", &gap.removed),
            ] {
                if !files.is_empty() {
                    parts.push(format!("{label} {}", list(files)));
                }
            }
            lines.push(format!(
                "Changed outside jobs {when}: {}.",
                parts.join("; ")
            ));
        }
        if self.unreadable > 0 {
            lines.push(format!(
                "{} record(s) could not be compared because their file list is missing.",
                self.unreadable
            ));
        }
        lines.join("\n")
    }
}

fn list(files: &[String]) -> String {
    const SHOWN: usize = 20;
    let mut text = files
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if files.len() > SHOWN {
        text.push_str(&format!(" and {} more", files.len() - SHOWN));
    }
    text
}
