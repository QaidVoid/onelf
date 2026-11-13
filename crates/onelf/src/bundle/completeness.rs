//! Completeness findings: what an assembly selected, left out, failed on,
//! or could not check, kept as structured records while the walk runs
//! and rendered once, sorted, when it ends.
//!
//! The walk reaches its outcomes in queue order, which depends on what
//! the tree and the host hold. Recording them here instead of printing
//! them as they happen makes the report a function of the findings
//! alone: the same assembly renders the same block whatever order its
//! work drained in, and a dry run renders it from the same records.

use std::cell::RefCell;

use super::ui;

/// What one finding says about its object, in the order a block lists
/// the outcomes of one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    /// The source or provider that answers for the object.
    Selected,
    /// Carried, but limited to what a trace observed: the members a
    /// `[bundle] trim-inferred` build keeps are the ones the trace
    /// opened, so the directory is not complete.
    TraceLimited,
    /// A pinned provider a dry run did not obtain, so it satisfies
    /// nothing yet.
    Unverified,
    /// An optional object or group the selected source does not provide.
    OptionalOmission,
    /// A dependency the policy, a trace or a host package takes out of
    /// the bundle.
    Removed,
    /// A dependency nothing in the selected source provides. A strict
    /// build fails on it unless only optional objects need it.
    Unresolved,
    /// A required object the selected source cannot provide.
    RequiredFailure,
    /// Something the assembly does not check or cannot infer.
    Unchecked,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Selected => "selected",
            Outcome::TraceLimited => "trace-limited",
            Outcome::Unverified => "unverified",
            Outcome::OptionalOmission => "optional omission",
            Outcome::Removed => "removed",
            Outcome::Unresolved => "unresolved",
            Outcome::RequiredFailure => "required failure",
            Outcome::Unchecked => "unchecked",
        }
    }
}

/// One attributed completeness finding. Findings sort by entrypoint,
/// framework, object, outcome and reason, in that field order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// The entrypoint the finding concerns, or `None` for the whole tree.
    pub entrypoint: Option<String>,
    /// The framework rule involved, or `None` for a plain dependency.
    pub framework: Option<String>,
    /// The object, group destination or path the finding is about.
    pub object: String,
    /// What the finding says.
    pub outcome: Outcome,
    /// Why, in the shared wording, naming the source it was judged
    /// against.
    pub reason: String,
    /// The soname that activated the framework's rule, when one did.
    pub activated_by: Option<String>,
}

impl Finding {
    /// A tree-wide finding about `object`, with no framework.
    pub fn new(object: impl Into<String>, outcome: Outcome, reason: impl Into<String>) -> Self {
        Finding {
            entrypoint: None,
            framework: None,
            object: object.into(),
            outcome,
            reason: reason.into(),
            activated_by: None,
        }
    }

    /// The same finding attributed to the framework rule `rule`.
    pub fn of_rule(mut self, rule: &str) -> Self {
        self.framework = Some(rule.to_string());
        self
    }

    /// The finding as one report line, without the shared indent.
    pub fn text(&self) -> String {
        let mut text = String::new();
        if let Some(entrypoint) = &self.entrypoint {
            text.push_str(&format!("entrypoint '{entrypoint}': "));
        }
        if let Some(framework) = &self.framework {
            text.push_str(framework);
            text.push_str(": ");
        }
        text.push_str(&format!(
            "{}: {}: {}",
            self.object,
            self.outcome.label(),
            self.reason
        ));
        if let Some(soname) = &self.activated_by {
            text.push_str(&format!(" (activated by {soname})"));
        }
        text
    }
}

/// The findings as a stable block: a header counting them, then one
/// line per distinct finding in sorted order. Empty when there are none.
pub fn render(findings: &[Finding]) -> Vec<String> {
    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort();
    sorted.dedup();
    if sorted.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("Completeness findings ({}):", sorted.len())];
    lines.extend(sorted.into_iter().map(|f| ui::finding_line(&f.text())));
    lines
}

thread_local! {
    /// The findings of the assembly in progress on this thread.
    static PENDING: RefCell<Vec<Finding>> = const { RefCell::new(Vec::new()) };
    /// The findings the last assembly on this thread reported.
    static REPORTED: RefCell<Vec<Finding>> = const { RefCell::new(Vec::new()) };
}

/// Record one finding of the assembly in progress.
pub(crate) fn record(finding: Finding) {
    PENDING.with(|p| p.borrow_mut().push(finding));
}

/// Replace `marker` in the reason of every pending finding about
/// `object`, or of every pending finding when `object` is `None`.
pub(crate) fn settle(object: Option<&str>, marker: &str, replacement: &str) {
    PENDING.with(|p| {
        for finding in p.borrow_mut().iter_mut() {
            if object.is_none_or(|object| finding.object == object) {
                finding.reason = finding.reason.replace(marker, replacement);
            }
        }
    });
}

/// End the assembly in progress: attribute each framework finding to
/// the soname that activated its rule, print the sorted block to
/// stderr, and keep the findings for [`take_reported`].
pub(crate) fn report(activations: &[super::RuleActivation]) {
    let mut findings = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    for finding in &mut findings {
        if finding.activated_by.is_none()
            && let Some(rule) = &finding.framework
        {
            finding.activated_by = activations
                .iter()
                .find(|a| &a.rule == rule)
                .map(|a| a.reason.clone());
        }
    }
    findings.sort();
    findings.dedup();
    let lines = render(&findings);
    if !lines.is_empty() {
        eprintln!("\n{}", lines.join("\n"));
    }
    REPORTED.with(|r| *r.borrow_mut() = findings);
}

/// The findings the last assembly on this thread reported, sorted.
pub fn take_reported() -> Vec<Finding> {
    REPORTED.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed set of findings recorded in walk order renders one block
    /// sorted by entrypoint, framework, object and reason, with the
    /// activating soname attached and duplicates collapsed.
    #[test]
    fn the_block_is_sorted_and_attributed_whatever_the_walk_order() {
        let walk = vec![
            Finding::new(
                "share/gst/registry",
                Outcome::RequiredFailure,
                "no declared location in the selected source",
            )
            .of_rule("gstreamer"),
            Finding::new("libabsent.so.7", Outcome::Unresolved, "needed by bin/app"),
            Finding::new("lib/libz.so.1", Outcome::Selected, "host scan /usr/lib"),
            Finding {
                entrypoint: Some("gui".to_string()),
                ..Finding::new(
                    "set gtk",
                    Outcome::Unverified,
                    "not cached; a dry run does not fetch it",
                )
            },
            Finding::new(
                "lib/gstreamer-1.0",
                Outcome::OptionalOmission,
                "no usable plugin group",
            )
            .of_rule("gstreamer"),
            Finding::new("libabsent.so.7", Outcome::Unresolved, "needed by bin/app"),
            Finding::new(
                "lib/libx.so",
                Outcome::Unchecked,
                "reads /usr/share/x; carry share/x and the runtime sets X_DIR",
            ),
        ];
        let activations = vec![super::super::RuleActivation {
            rule: "gstreamer".to_string(),
            reason: "libgstreamer-1.0.so.0".to_string(),
            groups: Vec::new(),
        }];
        for order in [walk.clone(), walk.iter().rev().cloned().collect()] {
            for finding in order {
                record(finding);
            }
            report(&activations);
            let lines = render(&take_reported());
            assert_eq!(
                lines,
                vec![
                    "Completeness findings (6):",
                    "  - lib/libx.so: unchecked: reads /usr/share/x; carry share/x and the \
                     runtime sets X_DIR",
                    "  - lib/libz.so.1: selected: host scan /usr/lib",
                    "  - libabsent.so.7: unresolved: needed by bin/app",
                    "  - gstreamer: lib/gstreamer-1.0: optional omission: no usable plugin \
                     group (activated by libgstreamer-1.0.so.0)",
                    "  - gstreamer: share/gst/registry: required failure: no declared location \
                     in the selected source (activated by libgstreamer-1.0.so.0)",
                    "  - entrypoint 'gui': set gtk: unverified: not cached; a dry run does not \
                     fetch it",
                ]
            );
        }
        assert!(render(&[]).is_empty());
    }
}
