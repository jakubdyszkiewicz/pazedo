//! Type-to-find for lists: typing while a list has focus moves its selection to the best
//! match of what was typed, without filtering the list. The typed text shows in a strip at
//! the list's bottom; backspace shortens it, and the list ends the search on escape or when the
//! selection moves otherwise.

use gpui::{AnyElement, App, Keystroke};
use ui::{Color, Icon, IconName, IconSize, Label, LabelSize, prelude::*};

#[derive(Debug, Default)]
pub struct TypeAhead {
    query: Option<String>,
}

impl TypeAhead {
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    pub fn is_active(&self) -> bool {
        self.query.is_some()
    }

    /// Ends the search. Returns whether there was one, for escape to end it before doing
    /// anything else.
    pub fn end(&mut self) -> bool {
        self.query.take().is_some()
    }

    /// Takes a key that the list's bindings left alone: a typed character extends the query and
    /// backspace shortens it, ending the search once it is empty. Returns whether the key was
    /// used. Keys with modifiers other than shift, and a leading space, are not.
    pub fn handle_key(&mut self, keystroke: &Keystroke) -> bool {
        let modifiers = &keystroke.modifiers;
        if modifiers.control || modifiers.platform || modifiers.function || modifiers.alt {
            return false;
        }
        let mut query = self.query.take().unwrap_or_default();
        if keystroke.key == "backspace" {
            if query.is_empty() {
                return false;
            }
            query.pop();
        } else {
            let typed = keystroke.key_char.as_deref().unwrap_or_default();
            if typed.is_empty()
                || typed.chars().any(char::is_control)
                || (typed == " " && query.is_empty())
            {
                self.query = (!query.is_empty()).then_some(query);
                return false;
            }
            query.push_str(typed);
        }
        self.query = (!query.is_empty()).then_some(query);
        true
    }

    /// The position of the candidate that matches the query best, the first of equals. `None`
    /// candidates (rows that aren't searched) and non-matching ones are skipped.
    pub fn best_match<'a>(
        &self,
        candidates: impl IntoIterator<Item = Option<&'a str>>,
    ) -> Option<usize> {
        let query = self.query.as_deref()?;
        candidates
            .into_iter()
            .enumerate()
            .filter_map(|(position, text)| Some((score(query, text?)?, position)))
            .max_by_key(|(score, position)| (*score, std::cmp::Reverse(*position)))
            .map(|(_, position)| position)
    }

    /// The strip with the query, in red when nothing matches, and what enter does.
    pub fn render(
        &self,
        has_match: bool,
        enter_hint: &'static str,
        cx: &App,
    ) -> Option<AnyElement> {
        let query = self.query.clone()?;
        let colors = cx.theme().colors();
        Some(
            h_flex()
                .flex_none()
                .gap_1p5()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(colors.border)
                .bg(colors.panel_background)
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(query)
                        .size(LabelSize::Small)
                        .color(if has_match {
                            Color::Default
                        } else {
                            Color::Error
                        }),
                )
                .child(div().flex_1())
                .child(
                    Label::new(if has_match { enter_hint } else { "No match" })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }
}

/// How well `text` matches `query`, or `None` if it doesn't: the query's characters must appear
/// in order (ignoring case). Runs of consecutive characters, characters at word starts and an
/// early first match score higher.
pub fn score(query: &str, text: &str) -> Option<i64> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return None;
    }
    let mut score = 0;
    let mut position = 0;
    let mut previous_match: Option<usize> = None;
    for wanted in query {
        let found = (position..text.len()).find(|&index| text[index] == wanted)?;
        let at_word_start = found == 0 || !text[found - 1].is_alphanumeric();
        score += match previous_match {
            Some(previous) if previous + 1 == found => 8,
            _ if at_word_start => 6,
            _ => 1,
        };
        if previous_match.is_none() {
            score -= found.min(20) as i64;
        }
        previous_match = Some(found);
        position = found + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_runs_and_word_starts() {
        assert_eq!(score("xyz", "Dark mode"), None);
        assert_eq!(score("", "Dark mode"), None);
        assert!(score("dm", "Dark mode").is_some(), "word starts match");
        assert!(
            score("dark", "Dark mode") > score("dark", "Did a rename kit"),
            "a run beats scattered characters"
        );
        assert!(
            score("mo", "Dark mode") > score("mo", "Promotion"),
            "a word start beats the middle of a word"
        );
        assert!(
            score("say", "Say hello") > score("say", "Essay draft"),
            "an earlier match wins"
        );
        assert_eq!(score("SAY", "say hello"), score("say", "Say hello"));
    }

    #[test]
    fn keys_build_the_query() {
        let mut type_ahead = TypeAhead::default();
        fn key(text: &'static str) -> Keystroke {
            Keystroke::parse(text).unwrap()
        }
        let typed = |text: &str| Keystroke {
            key: if text == " " {
                "space".into()
            } else {
                text.into()
            },
            key_char: Some(text.into()),
            ..Keystroke::default()
        };

        assert!(!type_ahead.handle_key(&typed(" ")), "a leading space");
        assert!(!type_ahead.handle_key(&key("backspace")));
        assert!(!type_ahead.handle_key(&key("cmd-m")));
        assert!(type_ahead.handle_key(&typed("m")));
        assert!(type_ahead.handle_key(&typed("a")));
        assert!(type_ahead.handle_key(&typed(" ")));
        assert_eq!(type_ahead.query(), Some("ma "));
        assert_eq!(
            type_ahead.best_match([None, Some("README.md"), Some("main.rs"), Some("ma in")]),
            Some(3)
        );
        assert!(type_ahead.handle_key(&key("backspace")));
        assert_eq!(
            type_ahead.best_match([None, Some("README.md"), Some("main.rs")]),
            Some(2)
        );
        assert!(type_ahead.handle_key(&key("backspace")));
        assert!(type_ahead.handle_key(&key("backspace")));
        assert!(!type_ahead.is_active(), "empty again");
        assert!(!type_ahead.end());
    }
}
