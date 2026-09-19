//! Fuzzy ranking over arbitrary items via nucleo-matcher.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

pub struct Fuzzy {
    matcher: Matcher,
}

impl Default for Fuzzy {
    fn default() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }
}

impl Fuzzy {
    /// Returns the items matching `query`, best first. An empty query keeps every item in order.
    pub fn rank<T, F>(&mut self, query: &str, items: Vec<T>, haystack: F) -> Vec<T>
    where
        F: Fn(&T) -> String,
    {
        let query = query.trim();
        if query.is_empty() {
            return items;
        }
        let pattern = Pattern::new(
            query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize, T)> = items
            .into_iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let hay = haystack(&item);
                let score = pattern.score(Utf32Str::new(&hay, &mut buf), &mut self.matcher)?;
                Some((score, i, item))
            })
            .collect();
        // Higher score first; ties keep the original (alphabetical) order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, item)| item).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_and_filters() {
        let mut f = Fuzzy::default();
        let items = vec![
            "Bed Lamp light.bed",
            "Kitchen Light light.kitchen",
            "Temperature sensor.temp",
        ];
        let r = f.rank("kit", items.clone(), |s| s.to_string());
        assert_eq!(r, vec!["Kitchen Light light.kitchen"]);
        let r = f.rank("light", items.clone(), |s| s.to_string());
        assert_eq!(r.len(), 2);
        assert!(!r.contains(&"Temperature sensor.temp"));
        assert_eq!(f.rank("", items.clone(), |s| s.to_string()), items);
        // Multi-word queries match each word independently.
        assert_eq!(f.rank("temp sens", items, |s| s.to_string()).len(), 1);
    }
}
