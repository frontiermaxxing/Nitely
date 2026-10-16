//! Whether a new issue likely duplicates another: compares titles, bodies and evidence. File paths
//! and identifiers they share count heavily, common workflow words ("deploy", "fix", "add") little,
//! and filler not at all. Every match says why it looked similar, so the agent filing can judge it.

use std::collections::BTreeMap;

/// What is compared of an issue.
pub struct Text<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub evidence: &'a str,
}

/// How alike two issues are, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Likeness {
    /// Weighted share of the title's terms (0 to 1).
    pub title: f64,
    /// Weighted share of the terms of body and evidence (0 to 1); None when either says too little
    /// to compare.
    pub body: Option<f64>,
    /// File paths and identifiers both name.
    pub shared_anchors: Vec<String>,
    /// Title words both use.
    pub shared_words: Vec<String>,
}

impl Likeness {
    /// Likely the same work: nearly the same title, or a related title over bodies that name the
    /// same files or say much the same.
    pub fn is_duplicate(&self) -> bool {
        self.title >= 0.6 || (self.title >= 0.3 && self.shared_anchors.len() >= 2) || (self.title >= 0.45 && self.body.is_some_and(|b| b >= 0.5))
    }

    /// One line on why, e.g. `title shares "inbox", "count"; both name src/Inbox.tsx`.
    pub fn why(&self) -> String {
        let quote = |w: &[String]| w.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(", ");
        let mut parts = Vec::new();
        if !self.shared_words.is_empty() {
            parts.push(format!("title shares {}", quote(&self.shared_words)));
        }
        if !self.shared_anchors.is_empty() {
            parts.push(format!("both name {}", self.shared_anchors.join(", ")));
        }
        if let Some(b) = self.body.filter(|b| *b >= 0.5) {
            parts.push(format!("bodies {:.0}% alike", b * 100.0));
        }
        if parts.is_empty() {
            parts.push("similar wording".into());
        }
        parts.join("; ")
    }
}

/// Words that say nothing about which work an issue is.
const FILLER: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "does", "for", "from", "has", "have", "how", "in",
    "into", "is", "it", "its", "no", "not", "of", "on", "one", "or", "so", "that", "the", "their", "them", "then", "there",
    "this", "to", "up", "was", "we", "when", "where", "which", "with", "without", "after", "before", "out", "all", "any",
    "our", "you", "your", "should", "would", "will", "now", "only", "also", "than", "too", "very", "via", "per", "yet",
];

/// Words almost every issue uses: they count, but little.
const COMMON: &[&str] = &[
    "add", "fix", "deploy", "make", "update", "use", "support", "show", "change", "remove", "new", "improve", "issue",
    "bug", "feature", "test", "work", "run", "handle", "allow", "create", "set", "get", "check", "move", "keep", "let",
    "need", "want", "wrong", "broken", "error", "fail", "failure", "production", "process", "document", "doc", "code",
    "file", "build", "release", "app", "user", "agent", "project", "data",
];

const ANCHOR_WEIGHT: f64 = 3.0;
const COMMON_WEIGHT: f64 = 0.3;
/// A body with fewer terms than this says too little to compare.
const MIN_BODY_TERMS: usize = 5;

/// Compares a new issue with an existing one.
pub fn compare(new: &Text, old: &Text) -> Likeness {
    let (nt, ot) = (terms(new.title), terms(old.title));
    let rest = |t: &Text| {
        let mut m = terms(t.body);
        m.extend(terms(t.evidence));
        m
    };
    let (nb, ob) = (rest(new), rest(old));
    let anchors = |a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>| -> Vec<String> {
        a.iter().filter(|(_, w)| **w == ANCHOR_WEIGHT).map(|(k, _)| k.clone()).chain(b.iter().filter(|(_, w)| **w == ANCHOR_WEIGHT).map(|(k, _)| k.clone())).collect()
    };
    let (na, oa) = (anchors(&nt, &nb), anchors(&ot, &ob));
    let mut shared_anchors: Vec<String> = na.into_iter().filter(|a| oa.contains(a)).collect();
    shared_anchors.sort();
    shared_anchors.dedup();
    let shared_words: Vec<String> = nt.iter().filter(|(k, w)| **w < ANCHOR_WEIGHT && ot.contains_key(*k)).map(|(k, _)| k.clone()).collect();
    let body = (nb.len() >= MIN_BODY_TERMS && ob.len() >= MIN_BODY_TERMS).then(|| weighted_jaccard(&nb, &ob));
    Likeness { title: weighted_jaccard(&nt, &ot), body, shared_anchors, shared_words }
}

fn weighted_jaccard(a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>) -> f64 {
    let shared: f64 = a.iter().filter(|(k, _)| b.contains_key(*k)).map(|(_, w)| w).sum();
    let union: f64 = a.values().sum::<f64>() + b.iter().filter(|(k, _)| !a.contains_key(*k)).map(|(_, w)| w).sum::<f64>();
    if union == 0.0 { 0.0 } else { shared / union }
}

/// The terms of a text with their weights: anchors (paths, file names, identifiers) and stemmed
/// words. Filler is left out.
fn terms(text: &str) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for chunk in text.split_whitespace() {
        let chunk = chunk.trim_matches(|c: char| "()[]{}<>,;:\"'`!?*".contains(c)).trim_end_matches('.');
        if let Some(anchor) = anchor(chunk) {
            out.insert(anchor, ANCHOR_WEIGHT);
            continue;
        }
        for word in chunk.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() > 1) {
            let word = stem(&word.to_ascii_lowercase());
            if FILLER.contains(&word.as_str()) {
                continue;
            }
            let weight = if COMMON.contains(&word.as_str()) { COMMON_WEIGHT } else { 1.0 };
            out.entry(word).or_insert(weight);
        }
    }
    out
}

/// A path, file name or identifier, normalized; None for a plain word.
fn anchor(chunk: &str) -> Option<String> {
    if chunk.len() < 3 || chunk.starts_with("http") {
        return None;
    }
    let has = |c: char| chunk.contains(c);
    let path = (has('/') || has('\\')) && chunk.chars().any(|c| c.is_ascii_alphabetic());
    // A file name: letters, a dot, then a short extension of letters (not "e.g" or a version).
    let file = chunk.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && stem.chars().any(|c| c.is_ascii_alphabetic()) && (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphabetic()) && ext.len() + stem.len() > 3 && stem.len() > 1
    });
    let snake = has('_') && chunk.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let camel = chunk.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && chunk.chars().skip(1).any(|c| c.is_ascii_uppercase())
        && chunk.chars().all(|c| c.is_ascii_alphanumeric());
    let path_sep = has(':') && chunk.contains("::");
    if !(path || file || snake || camel || path_sep) {
        return None;
    }
    let normal = chunk.replace('\\', "/").to_ascii_lowercase();
    let normal = normal.trim_start_matches("./").trim_matches('/').to_string();
    (!normal.is_empty()).then_some(normal)
}

/// Folds plurals and -ing/-ed forms together: "answered", "answering" and "answers" are "answer".
fn stem(word: &str) -> String {
    for (suffix, min) in [("ing", 6), ("ed", 5), ("s", 4)] {
        if word.len() >= min && word.ends_with(suffix) && !word.ends_with("ss") {
            return word[..word.len() - suffix.len()].to_string();
        }
    }
    word.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t<'a>(title: &'a str, body: &'a str) -> Text<'a> {
        Text { title, body, evidence: "" }
    }

    #[test]
    fn a_shared_common_word_is_not_a_duplicate() {
        // A real false positive from a board sweep.
        let l = compare(&t("Deploy /me/entitlements/ to production", "The endpoint is merged but not deployed."), &t("Document and codify the deploy process", "Write down how a release goes out."));
        assert!(!l.is_duplicate(), "{l:?}");
    }

    #[test]
    fn the_same_work_in_other_words_is_a_duplicate() {
        let l = compare(&t("Inbox count is wrong after answering an ask", ""), &t("Inbox badge count stays wrong after an ask is answered", ""));
        assert!(l.is_duplicate(), "{l:?}");
        assert!(l.why().contains("\"inbox\""), "{}", l.why());

        // Different titles, but both bodies name the same two places in the code.
        let a = t("Tray badge ignores stuck worktrees", "src-tauri/src/tray.rs builds the count in count_needs and skips rows with a rebase in progress.");
        let b = t("Stuck worktrees missing from the tray count", "count_needs in src-tauri/src/tray.rs only looks at sessions.");
        let l = compare(&a, &b);
        assert!(l.is_duplicate(), "{l:?}");
        assert_eq!(l.shared_anchors, ["count_needs", "src-tauri/src/tray.rs"]);
        assert!(l.why().contains("both name count_needs, src-tauri/src/tray.rs"), "{}", l.why());
    }

    #[test]
    fn bodies_count_only_when_they_say_enough() {
        // Related titles, the same one-word body: not enough to call them the same work.
        assert!(!compare(&t("Show invoices in the account view", "why"), &t("Invoice endpoint for the account", "why")).is_duplicate());
        let body = "Customers see last month's invoice total twice when the billing period rolls over at midnight";
        let l = compare(&t("Invoice totals double at month end", body), &t("Invoice shows the total twice at the end of a month", body));
        assert!(l.is_duplicate() && l.why().contains("bodies"), "{l:?}");
    }

    #[test]
    fn titles_naming_different_files_differ() {
        assert!(!compare(&t("Fix the crash in src/a.rs", ""), &t("Fix the crash in src/b.rs", "")).is_duplicate());
        assert!(compare(&t("Wire the settings page", ""), &t("Wire up the settings pages", "")).is_duplicate());
        assert!(!compare(&t("Settings page keyboard shortcuts", ""), &t("Wire the settings page", "")).is_duplicate());
    }

    #[test]
    fn terms_find_anchors_and_fold_words() {
        let terms = terms("Edit crates\\nitely-core\\src\\issues.rs: title_similarity in Db::create_issue (see issues.rs, e.g. titleSimilarity) v1.2");
        for anchor in ["crates/nitely-core/src/issues.rs", "title_similarity", "db::create_issue", "issues.rs", "titlesimilarity"] {
            assert_eq!(terms.get(anchor), Some(&ANCHOR_WEIGHT), "{anchor} in {terms:?}");
        }
        assert!(!terms.contains_key("e.g") && !terms.contains_key("v1.2"));
        assert_eq!(stem("answered"), "answer");
        assert_eq!(stem("answering"), "answer");
        assert_eq!(stem("class"), "class");
        assert_eq!(stem("worktrees"), stem("worktree"));
    }
}
