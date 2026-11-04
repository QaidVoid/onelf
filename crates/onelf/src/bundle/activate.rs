//! Deterministic descriptor activation over the bundling closure.
//!
//! Every soname the dependency walk encounters is matched against the
//! enabled framework descriptors' soname-prefix triggers. A matching rule
//! activates once, records the soname that activated it, and queues each
//! of its resource groups exactly once. A rule activated by a library
//! that was itself discovered because another rule activated is reached
//! the same way, so activation settles at a fixed point over the
//! closure's sonames. Rules the caller disabled never activate, whatever
//! their triggers match.
//!
//! Objects and activated resource groups share one work queue. Objects
//! drain in the dependency walk's established order: the sorted seed set
//! in reverse identity order, then depth-first over transitive
//! discoveries, so the walk's alias and search-path decisions are
//! unchanged. An object may be requeued when its eligibility changed
//! meanwhile (a module let through once its let-through was decided), so
//! objects are admitted to processing once, at the walk's existing
//! admission check. Groups deduplicate at enqueue: one identity is
//! queued once no matter how many sonames or rules reach it, and the
//! queue refuses to grow past its cap.
//!
//! Activation is a function of the soname set alone: rules are iterated
//! sorted by identity, the seed set is sorted, and the walk's drain
//! order is a function of that set, so equivalent inputs activate the
//! same rules in the same order and queue the same groups in the same
//! order whatever order the inputs were enumerated in.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::io;

use onelf_format::rules::{self, Descriptor, RuleId, Trigger};

/// One activated rule and what discovery queued for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleActivation {
    /// The activated rule's identity.
    pub rule: String,
    /// The soname whose prefix match activated the rule.
    pub reason: String,
    /// The rule's resource groups, queued once each, in declaration
    /// order.
    pub groups: Vec<String>,
}

/// A queued resource group: the rule that owns it and its declared
/// identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GroupKey {
    /// The rule the group belongs to.
    rule: String,
    /// The group's identity within that rule.
    id: String,
}

/// One unit of discovery work: a soname the walk resolves, or an
/// activated rule's resource group for collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkItem {
    /// A soname the dependency walk resolves and bundles.
    Object(String),
    /// An activated rule's resource group, queued once per identity.
    Group(GroupKey),
}

/// The shared discovery work queue.
pub(crate) struct WorkQueue {
    items: VecDeque<WorkItem>,
    queued_groups: HashSet<GroupKey>,
    cap: usize,
}

impl WorkQueue {
    /// A queue admitting at most `cap` distinct groups.
    fn new(cap: usize) -> Self {
        Self {
            items: VecDeque::new(),
            queued_groups: HashSet::new(),
            cap,
        }
    }

    /// Queue the walk's initial sonames. The caller passes them sorted,
    /// and they are pushed so draining starts at the last name, the
    /// order the dependency walk has always used.
    fn seed(&mut self, sorted: &[String]) {
        for soname in sorted {
            self.items.push_front(WorkItem::Object(soname.clone()));
        }
    }

    /// Queue a soname ahead of the remaining work, the way a transitive
    /// dependency enters the walk.
    fn push_object(&mut self, soname: String) {
        self.items.push_front(WorkItem::Object(soname));
    }

    /// Queue a resource group once per identity. Beyond the cap the
    /// queue answers with an error instead of growing unbounded, so a
    /// hostile or cyclic rule set stops the build with a diagnosis.
    fn push_group(&mut self, group: GroupKey) -> io::Result<bool> {
        if self.queued_groups.contains(&group) {
            return Ok(false);
        }
        if self.queued_groups.len() >= self.cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "rule {} queued more than {} resource groups; refusing \
                     unbounded discovery",
                    group.rule, self.cap
                ),
            ));
        }
        self.queued_groups.insert(group.clone());
        self.items.push_back(WorkItem::Group(group));
        Ok(true)
    }

    /// Take the next unit of work: objects before queued groups.
    fn pop(&mut self) -> Option<WorkItem> {
        self.items.pop_front()
    }
}

/// One enabled rule with its triggers, its groups, and the soname that
/// activated it once discovery matched one.
struct RuleState {
    id: String,
    triggers: Vec<Trigger>,
    groups: Vec<GroupKey>,
    reason: Option<String>,
}

impl RuleState {
    fn of(descriptor: &Descriptor) -> Self {
        let id = descriptor.id.as_str().to_string();
        let mut groups = Vec::new();
        for group in descriptor
            .resources
            .plugins
            .iter()
            .chain(&descriptor.resources.data)
        {
            groups.push(GroupKey {
                rule: id.clone(),
                id: group.id.as_str().to_string(),
            });
        }
        for helper in &descriptor.resources.helpers {
            groups.push(GroupKey {
                rule: id.clone(),
                id: helper.id.as_str().to_string(),
            });
        }
        Self {
            id,
            triggers: descriptor.triggers.clone(),
            groups,
            reason: None,
        }
    }
}

/// Deterministic rule activation over the discovery closure.
pub(crate) struct Discovery {
    rules: Vec<RuleState>,
    queue: WorkQueue,
}

impl Discovery {
    /// Build the activator from the builtin and local descriptors,
    /// keeping the enabled ones sorted by identity and refusing unknown
    /// or repeated disabled identities. The local descriptors are the
    /// ones a recipe's `[bundle] rules` loaded.
    pub(crate) fn new(
        builtins: &[Descriptor],
        local: &[Descriptor],
        disabled: &[RuleId],
    ) -> io::Result<Self> {
        rules::validate_catalog(builtins, local, disabled)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let off: HashSet<&str> = disabled.iter().map(RuleId::as_str).collect();
        let mut enabled: Vec<&Descriptor> = builtins
            .iter()
            .chain(local)
            .filter(|d| !off.contains(d.id.as_str()))
            .collect();
        enabled.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Self {
            rules: enabled.into_iter().map(RuleState::of).collect(),
            queue: WorkQueue::new(rules::MAX_RECORDS),
        })
    }

    /// Match a soname against every not-yet-active rule and queue the
    /// groups of each rule it activates. Idempotent per soname and per
    /// rule, so the fixed point needs no special casing here.
    pub(crate) fn observe(&mut self, soname: &str) -> io::Result<()> {
        for state in &mut self.rules {
            if state.reason.is_some() {
                continue;
            }
            if !state.triggers.iter().any(|t| t.matches_soname(soname)) {
                continue;
            }
            state.reason = Some(soname.to_string());
            for group in &state.groups {
                self.queue.push_group(group.clone())?;
            }
        }
        Ok(())
    }

    /// Whether any enabled rule's trigger matches `soname`, active yet
    /// or not.
    pub(crate) fn triggers(&self, soname: &str) -> bool {
        self.rules
            .iter()
            .any(|state| state.triggers.iter().any(|t| t.matches_soname(soname)))
    }

    /// Observe a soname and queue it for resolution: a seed of the walk
    /// or a dependency discovered along the way.
    pub(crate) fn enqueue(&mut self, soname: &str) -> io::Result<()> {
        self.observe(soname)?;
        self.queue.push_object(soname.to_string());
        Ok(())
    }

    /// Seed the walk's initial sonames, already sorted by the caller.
    pub(crate) fn seed(&mut self, sorted: &[String]) {
        self.queue.seed(sorted);
    }

    /// Take the next unit of work.
    pub(crate) fn pop(&mut self) -> Option<WorkItem> {
        self.queue.pop()
    }

    /// The activations in catalog order, each with its queued groups.
    pub(crate) fn activations(&self) -> Vec<RuleActivation> {
        self.rules
            .iter()
            .filter_map(|state| {
                state.reason.clone().map(|reason| RuleActivation {
                    rule: state.id.clone(),
                    reason,
                    groups: state.groups.iter().map(|g| g.id.clone()).collect(),
                })
            })
            .collect()
    }
}

thread_local! {
    static LAST_ACTIVATIONS: RefCell<Vec<RuleActivation>> = const { RefCell::new(Vec::new()) };
}

/// Publish what one bundle run activated, for the assembly report.
pub(crate) fn publish_activations(activations: Vec<RuleActivation>) {
    LAST_ACTIVATIONS.with(|cell| *cell.borrow_mut() = activations);
}

/// Take the activations of the last completed bundle run on this
/// thread. A run that failed activation leaves nothing behind.
pub fn take_activations() -> Vec<RuleActivation> {
    LAST_ACTIVATIONS.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use onelf_format::catalog;
    use onelf_format::rules::{
        RelativePath, Requirement, ResourceGroup, ResourceKind, SourceBase, SourceCandidate,
    };

    /// A synthetic descriptor with one trigger and directory plugin
    /// groups, the shape a project-local rule file produces.
    fn descriptor(id: &str, prefix: &str, groups: &[&str]) -> Descriptor {
        let mut d = Descriptor {
            schema: rules::SCHEMA_VERSION,
            id: RuleId::new(id).unwrap(),
            triggers: vec![Trigger {
                soname_prefix: prefix.to_string(),
            }],
            resources: Default::default(),
            build: Default::default(),
            runtime: Default::default(),
        };
        for group in groups {
            d.resources.plugins.push(ResourceGroup {
                id: RuleId::new(group).unwrap(),
                kind: ResourceKind::Directory,
                candidates: vec![SourceCandidate {
                    base: SourceBase::LibraryDir,
                    path: RelativePath::new("plugins").unwrap(),
                }],
                destination: RelativePath::new(group).unwrap(),
                requirement: Requirement::Optional,
                members: Vec::new(),
            });
        }
        d
    }

    #[test]
    fn builtin_rules_reach_the_activator_sorted_by_identity() {
        let ids: Vec<&str> = catalog::builtins().iter().map(|d| d.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "the catalog must hand over sorted rules");
    }

    #[test]
    fn a_seed_soname_activates_its_rule() {
        let mut d = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
        d.observe("libasound.so.2").unwrap();
        let report = d.activations();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].rule, "alsa");
        assert_eq!(report[0].reason, "libasound.so.2");
        assert_eq!(report[0].groups, vec!["plugins", "configuration"]);
    }

    #[test]
    fn a_prefix_is_never_a_substring_or_case_folded_match() {
        let mut d = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
        d.observe("not-libasound.so.2").unwrap();
        d.observe("LIBASOUND.SO.2").unwrap();
        d.observe("libasound_module_pcm_pulse.so").unwrap();
        assert!(d.activations().is_empty());
    }

    #[test]
    fn a_disabled_rule_stays_disabled() {
        let disabled = vec![RuleId::new("alsa").unwrap()];
        let mut d = Discovery::new(catalog::builtins(), &[], &disabled).unwrap();
        d.observe("libasound.so.2").unwrap();
        assert!(d.activations().is_empty());
        assert!(d.pop().is_none(), "a disabled rule queues no group");
    }

    #[test]
    fn an_unknown_disabled_identity_is_refused() {
        let disabled = vec![RuleId::new("not-a-rule").unwrap()];
        let err = Discovery::new(catalog::builtins(), &[], &disabled)
            .err()
            .expect("an unknown disabled identity must be refused");
        assert!(err.to_string().contains("not-a-rule"), "{err}");
    }

    #[test]
    fn a_discovered_soname_activates_a_second_rule_to_the_fixed_point() {
        let mut d = Discovery::new(
            &[],
            &[
                descriptor("aaa", "liba.so", &["one"]),
                descriptor("bbb", "libb.so", &["two"]),
            ],
            &[],
        )
        .unwrap();
        d.observe("liba.so.1").unwrap();
        assert_eq!(d.activations().len(), 1);
        // A library the first rule's collection discovered.
        d.observe("libb.so.0").unwrap();
        let report = d.activations();
        assert_eq!(report.len(), 2, "discovery continues to the fixed point");
        assert_eq!(report[0].rule, "aaa");
        assert_eq!(report[1].rule, "bbb");
    }

    #[test]
    fn reversed_observation_order_leaves_the_report_identical() {
        let sonames = [
            "libpulse.so.0",
            "libasound.so.2",
            "libX11.so.6",
            "libgstreamer-1.0.so.0",
        ];
        let run = |order: &[&str]| {
            let mut d = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
            for soname in order {
                d.observe(soname).unwrap();
            }
            d.activations()
        };
        let forward = run(&sonames);
        let reversed = run(&sonames.iter().rev().cloned().collect::<Vec<_>>());
        assert!(forward.len() >= 3, "{forward:?}");
        assert_eq!(
            forward, reversed,
            "catalog order must not depend on input order"
        );
    }

    #[test]
    fn a_group_is_queued_once_per_identity() {
        let mut d = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
        d.observe("libasound.so.2").unwrap();
        d.observe("libasound.so.2").unwrap();
        d.observe("libasound.so.3").unwrap();
        let mut seen = Vec::new();
        while let Some(item) = d.pop() {
            match item {
                WorkItem::Group(group) => {
                    assert_eq!(group.rule, "alsa");
                    seen.push(group.id);
                }
                WorkItem::Object(_) => panic!("an activation queues no object"),
            }
        }
        assert_eq!(seen, ["plugins", "configuration"]);
    }

    #[test]
    fn objects_drain_in_the_walks_established_order() {
        let mut d = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
        d.seed(&[
            "a.so.1".to_string(),
            "b.so.1".to_string(),
            "c.so.1".to_string(),
        ]);
        d.enqueue("d.so.1").unwrap();
        let order: Vec<String> = std::iter::from_fn(|| d.pop())
            .filter_map(|item| match item {
                WorkItem::Object(soname) => Some(soname),
                WorkItem::Group(_) => None,
            })
            .collect();
        assert_eq!(order, ["d.so.1", "c.so.1", "b.so.1", "a.so.1"]);
    }

    #[test]
    fn sorted_seeds_queue_groups_in_a_fixed_order() {
        let group_sequence = |seed: &[&str]| {
            let mut d = Discovery::new(catalog::builtins(), &[], &[]).unwrap();
            let sorted: Vec<String> = {
                let mut s = seed.iter().map(|s| s.to_string()).collect::<Vec<_>>();
                s.sort();
                s
            };
            for soname in &sorted {
                d.observe(soname).unwrap();
            }
            d.seed(&sorted);
            std::iter::from_fn(|| d.pop())
                .filter_map(|item| match item {
                    WorkItem::Group(g) => Some(g.id),
                    WorkItem::Object(_) => None,
                })
                .collect::<Vec<_>>()
        };
        let forward = group_sequence(&["libasound.so.2", "libgstreamer-1.0.so.0"]);
        let reversed_input = group_sequence(&["libgstreamer-1.0.so.0", "libasound.so.2"]);
        assert_eq!(
            forward, reversed_input,
            "the sorted seed set must fix the queue order"
        );
        assert_eq!(forward, ["plugins", "configuration", "plugins", "scanner"]);
    }

    #[test]
    fn the_group_queue_is_bounded() {
        let mut q = WorkQueue::new(2);
        let group = |n: &str| GroupKey {
            rule: "r".to_string(),
            id: n.to_string(),
        };
        assert!(q.push_group(group("1")).unwrap());
        assert!(q.push_group(group("2")).unwrap());
        assert!(
            !q.push_group(group("1")).unwrap(),
            "a queued identity stays queued once"
        );
        let err = q.push_group(group("3")).unwrap_err();
        assert!(
            err.to_string().contains("refusing unbounded discovery"),
            "{err}"
        );
    }
}
