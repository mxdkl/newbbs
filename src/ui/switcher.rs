//! The `/` fuzzy switcher: one list of every channel, group and DM, filtered
//! as you type.

use std::collections::HashMap;

use super::App;
use crate::model::{ConvId, Conversation};

pub struct Switcher {
    pub query: String,
    /// (index into `App::convs`, matched character positions in the label).
    pub matches: Vec<(usize, Vec<usize>)>,
    pub selected: usize,
}

impl Switcher {
    pub fn new(app: &App) -> Switcher {
        let mut switcher = Switcher {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        switcher.refilter(&app.convs, &app.labels);
        switcher
    }

    pub fn refilter(&mut self, convs: &[Conversation], labels: &HashMap<ConvId, String>) {
        let empty = String::new();
        let mut scored: Vec<(i32, usize, Vec<usize>)> = Vec::new();
        for (index, conv) in convs.iter().enumerate() {
            let label = labels.get(&conv.id).unwrap_or(&empty);
            if self.query.is_empty() {
                scored.push((0, index, Vec::new()));
            } else if let Some((score, positions)) = score(label, &self.query) {
                scored.push((score, index, positions));
            }
        }
        // Best score first; ties keep sidebar order so the list is stable.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, i, p)| (i, p)).collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
        }
    }

    /// The index into `App::convs` that is currently highlighted.
    pub fn selection(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|(index, _)| *index)
    }
}

/// Subsequence match with bonuses for runs and for hitting the start of a word.
/// Returns the score and where the query characters landed.
fn score(haystack: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    let hay: Vec<char> = haystack.chars().collect();
    let mut positions = Vec::new();
    let mut total = 0;
    let mut at = 0usize;
    let mut last_hit: Option<usize> = None;

    for want in needle.chars().flat_map(|c| c.to_lowercase()) {
        let mut found = None;
        while at < hay.len() {
            if hay[at].to_lowercase().next() == Some(want) {
                found = Some(at);
                break;
            }
            at += 1;
        }
        let hit = found?;
        let mut points = 10;
        if last_hit == Some(hit.wrapping_sub(1)) {
            points += 8; // consecutive
        }
        let boundary = hit == 0
            || matches!(hay[hit - 1], '#' | '@' | '&' | '-' | '_' | ' ' | '.' | ',');
        if boundary {
            points += 12;
        }
        // Distance from the previous match costs a little.
        if let Some(prev) = last_hit {
            points -= ((hit - prev - 1) as i32).min(8);
        }
        total += points;
        positions.push(hit);
        last_hit = Some(hit);
        at = hit + 1;
    }
    // Shorter labels win ties: "#dev" should beat "#development".
    total -= (hay.len() as i32) / 4;
    Some((total, positions))
}
