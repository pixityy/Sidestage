//! Which sources a new list keeps, stops and starts ([`crate::Session::set_sources`]), and what
//! Linux makes of their hide methods.

use crate::{AppKey, HideMethod, SourceConfig, SourceId};

/// What a source is identified by across [`crate::Session::set_sources`] calls: same ID, same
/// app ([`AppKey::same_app`]) and same hide method means the same, untouched source.
#[derive(Clone, Copy, Debug)]
pub struct Identity<'a> {
    pub id: SourceId,
    pub app: &'a AppKey,
    pub hide: &'a HideMethod,
}

impl<'a> Identity<'a> {
    pub fn of(config: &'a SourceConfig) -> Self {
        Self { id: config.id, app: &config.app, hide: &config.hide }
    }

    pub fn is(&self, other: &Identity<'_>) -> bool {
        self.id == other.id && self.app.same_app(other.app) && self.hide == other.hide
    }
}

/// The changes from the running sources to a new list. Sources in neither list keep running.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Running sources to stop: missing from the new list, or with a changed app or hide method.
    /// In their current order.
    pub remove: Vec<SourceId>,
    /// Positions in the new list of the sources to start, in list order.
    pub add: Vec<usize>,
}

/// `wanted` must be a valid list ([`crate::SessionConfig::duplicate_error`]).
pub fn diff(current: &[Identity<'_>], wanted: &[Identity<'_>]) -> Diff {
    Diff {
        remove: current.iter().filter(|c| !wanted.iter().any(|w| w.is(c))).map(|c| c.id).collect(),
        add: wanted.iter().enumerate().filter(|(_, w)| !current.iter().any(|c| c.is(w))).map(|(i, _)| i).collect(),
    }
}

/// Lays out per-source entries for a new list: an entry whose identity is unchanged is carried
/// over as it is (its status and live counters), every other source gets `new(config)`.
pub fn carry_over<T: Clone>(
    old: &[T],
    identity: impl Fn(&T) -> Identity<'_>,
    wanted: &[SourceConfig],
    new: impl Fn(&SourceConfig) -> T,
) -> Vec<T> {
    wanted
        .iter()
        .map(|w| old.iter().find(|o| identity(o).is(&Identity::of(w))).cloned().unwrap_or_else(|| new(w)))
        .collect()
}

/// Linux hides an app one way only (moving it to its capture sink); this explains what happens
/// to the other choices instead.
pub fn hide_warning(hide: &HideMethod) -> Option<&'static str> {
    match hide {
        HideMethod::Auto => None,
        HideMethod::KeepOriginal => {
            Some("\u{201c}Keep original\u{201d} is not available on Linux; the original is hidden instead.")
        }
        HideMethod::ParkOn(_) => Some(
            "Parking the app on another output is a Windows option; on Linux the app is moved to \
             Sidestage's hidden capture output instead.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::Processor;

    struct Silence;
    impl Processor for Silence {
        fn process(&mut self, _input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            output.fill(0.0);
        }
    }

    fn config(id: SourceId, app: &str, hide: HideMethod) -> SourceConfig {
        SourceConfig {
            id,
            app: AppKey(app.into()),
            hide,
            processor: Arc::new(|_| Box::new(Silence) as Box<dyn Processor>),
        }
    }

    fn auto(id: SourceId, app: &str) -> SourceConfig {
        config(id, app, HideMethod::Auto)
    }

    fn ids(list: &[SourceConfig]) -> Vec<Identity<'_>> {
        list.iter().map(Identity::of).collect()
    }

    #[test]
    fn unchanged_sources_are_kept_whatever_their_order() {
        let now = [auto(1, "spotify"), auto(2, "podcast"), auto(3, "discord")];
        let next = [auto(3, "discord"), auto(1, "spotify"), auto(2, "podcast")];
        assert_eq!(diff(&ids(&now), &ids(&next)), Diff::default());
        assert_eq!(diff(&[], &[]), Diff::default());
    }

    #[test]
    fn new_ids_start_and_missing_ids_stop() {
        let now = [auto(1, "spotify"), auto(2, "podcast")];
        let next = [auto(2, "podcast"), auto(3, "discord")];
        assert_eq!(diff(&ids(&now), &ids(&next)), Diff { remove: vec![1], add: vec![1] });
        assert_eq!(diff(&ids(&now), &[]), Diff { remove: vec![1, 2], add: vec![] });
        assert_eq!(diff(&[], &ids(&now)), Diff { remove: vec![], add: vec![0, 1] });
    }

    #[test]
    fn a_changed_app_or_hide_method_is_remove_plus_add() {
        let now = [auto(1, "spotify"), auto(2, "podcast"), auto(3, "discord")];
        let next = [auto(1, "rhythmbox"), config(2, "podcast", HideMethod::KeepOriginal), auto(3, "discord")];
        assert_eq!(diff(&ids(&now), &ids(&next)), Diff { remove: vec![1, 2], add: vec![0, 1] });
        let moved_app = [auto(7, "spotify")];
        assert_eq!(
            diff(&ids(&now[..1]), &ids(&moved_app)),
            Diff { remove: vec![1], add: vec![0] },
            "the same app under a new ID is a new source"
        );
    }

    #[test]
    fn a_source_is_rebuilt_by_removing_it_then_adding_it_back() {
        // How the app restarts a failed source with its current renderer: two calls in a row.
        let all = [auto(1, "spotify"), auto(2, "podcast"), auto(3, "discord")];
        let interim = [auto(1, "spotify"), auto(3, "discord")];
        assert_eq!(diff(&ids(&all), &ids(&interim)), Diff { remove: vec![2], add: vec![] });
        assert_eq!(diff(&ids(&interim), &ids(&all)), Diff { remove: vec![], add: vec![1] });
    }

    #[derive(Clone)]
    struct Entry {
        id: SourceId,
        app: AppKey,
        hide: HideMethod,
        tag: &'static str,
    }

    #[test]
    fn entries_carry_over_by_identity() {
        let old: Vec<Entry> = [auto(1, "spotify"), auto(2, "podcast")]
            .iter()
            .map(|c| Entry { id: c.id, app: c.app.clone(), hide: c.hide.clone(), tag: "old" })
            .collect();
        let next = [auto(2, "podcast"), auto(1, "rhythmbox"), auto(4, "discord")];
        let entries = carry_over(
            &old,
            |e| Identity { id: e.id, app: &e.app, hide: &e.hide },
            &next,
            |c| Entry { id: c.id, app: c.app.clone(), hide: c.hide.clone(), tag: "new" },
        );
        let got: Vec<(SourceId, &str)> = entries.iter().map(|e| (e.id, e.tag)).collect();
        assert_eq!(got, [(2, "old"), (1, "new"), (4, "new")], "podcast keeps its entry, in the new order");
    }

    #[test]
    fn linux_explains_the_hide_methods_it_does_not_have() {
        assert_eq!(hide_warning(&HideMethod::Auto), None);
        assert!(hide_warning(&HideMethod::KeepOriginal).unwrap().contains("not available on Linux"));
        assert!(hide_warning(&HideMethod::ParkOn("x".into())).unwrap().contains("Windows option"));
    }
}
