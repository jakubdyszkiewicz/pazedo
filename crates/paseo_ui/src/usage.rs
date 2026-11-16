//! The usage of the AI subscriptions in the title bar, as the Paseo app shows it at the bottom
//! of its sidebar: each account's icon with the percent left of its summary windows, e.g.
//! "▬ 82% 5h ▬ 67% wk". A click opens every window, balance and detail, with Refresh.
//!
//! The usage comes from the daemon whose folders are on the window's machine, since each daemon
//! reports the accounts its own machine is logged into.

use chrono::{DateTime, Local, Utc};
use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Render, SharedString, Subscription, WeakEntity,
    Window, relative,
};
use paseo_client::protocol::{UsageProblem, UsageReport, UsageReportEntry, UsageTone, UsageWindow};
use title_bar::TitleBar;
use ui::{ButtonLike, ContextMenu, ContextMenuEntry, PopoverMenu, Tooltip, prelude::*};
use workspace::Workspace;

use crate::{PaseoDaemons, PaseoStore, PaseoStoreEvent};

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|title_bar: &mut TitleBar, _window, cx| {
        let workspace = title_bar.workspace().clone();
        let indicator = cx.new(|cx| UsageIndicator::new(workspace, cx));
        title_bar.add_item(indicator.into(), cx);
    })
    .detach();
}

struct UsageIndicator {
    workspace: WeakEntity<Workspace>,
    daemons: Entity<PaseoDaemons>,
    _store_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl UsageIndicator {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let daemons = PaseoDaemons::global(cx);
        let subscriptions = vec![cx.observe(&daemons, |this, _, cx| {
            this.subscribe_to_stores(cx);
            cx.notify();
        })];
        let mut this = Self {
            workspace,
            daemons,
            _store_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.subscribe_to_stores(cx);
        this
    }

    fn subscribe_to_stores(&mut self, cx: &mut Context<Self>) {
        let stores = self.daemons.read(cx).stores().to_vec();
        self._store_subscriptions = stores
            .iter()
            .map(|store| {
                cx.subscribe(store, |_, _, event, cx| {
                    if matches!(
                        event,
                        PaseoStoreEvent::UsageChanged
                            | PaseoStoreEvent::Connected
                            | PaseoStoreEvent::Disconnected
                    ) {
                        cx.notify();
                    }
                })
            })
            .collect();
    }

    /// The daemon whose usage the window shows: the first one with usage whose folders are on
    /// the window's machine, else the first one with usage.
    fn store(&self, cx: &App) -> Option<Entity<PaseoStore>> {
        let has_usage = |store: &&Entity<PaseoStore>| !store.read(cx).usage().is_empty();
        let stores = self.daemons.read(cx).stores();
        let project = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().clone());
        project
            .and_then(|project| {
                stores
                    .iter()
                    .filter(has_usage)
                    .find(|store| store.read(cx).is_on_host(project.read(cx), cx))
            })
            .or_else(|| stores.iter().find(has_usage))
            .cloned()
    }
}

impl Render for UsageIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(store) = self.store(cx) else {
            return div().into_any_element();
        };
        let summary = summarize(store.read(cx).usage());
        if summary.is_empty() {
            return div().into_any_element();
        }
        let tooltip: SharedString = format!("AI usage on {}", store.read(cx).display_name()).into();
        let trigger = ButtonLike::new("paseo-usage").child(
            h_flex()
                .gap_3()
                .children(summary.iter().map(|source| render_source(source, cx))),
        );
        PopoverMenu::new("paseo-usage-menu")
            .trigger_with_tooltip(trigger, move |_window, cx| {
                Tooltip::with_meta(
                    tooltip.clone(),
                    None,
                    "Percent left · click for details",
                    cx,
                )
            })
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| Some(usage_menu(&store, window, cx)))
            .into_any_element()
    }
}

/// One account in the title bar: its icon, then each summary window's meter, percent left and
/// short label.
struct SummarySource {
    source_id: String,
    windows: Vec<SummaryWindow>,
}

struct SummaryWindow {
    percent_left: f64,
    short_label: SharedString,
    tone: UsageTone,
}

/// The accounts with summary windows, in the daemon's order, as the Paseo app's sidebar shows
/// them; accounts without any are left out.
fn summarize(reports: &[UsageReportEntry]) -> Vec<SummarySource> {
    reports
        .iter()
        .filter_map(|entry| {
            let windows: Vec<_> = entry
                .available()?
                .windows
                .iter()
                .filter(|window| window.summary)
                .filter_map(|window| {
                    Some(SummaryWindow {
                        percent_left: percent_left(window)?,
                        short_label: window
                            .short_label
                            .clone()
                            .unwrap_or_else(|| window.label.clone())
                            .into(),
                        tone: window.tone,
                    })
                })
                .collect();
            (!windows.is_empty()).then(|| SummarySource {
                source_id: entry.source_id.clone(),
                windows,
            })
        })
        .collect()
}

fn percent_left(window: &UsageWindow) -> Option<f64> {
    window
        .remaining_pct
        .or_else(|| window.used_pct.map(|used| 100. - used))
        .map(|percent| percent.clamp(0., 100.))
}

fn source_icon(source_id: &str) -> IconName {
    crate::agent_item::provider_icon(&[source_id])
}

fn tone_color(tone: UsageTone) -> Color {
    match tone {
        UsageTone::Warning => Color::Warning,
        UsageTone::Danger => Color::Error,
        UsageTone::Ok | UsageTone::Default => Color::Muted,
    }
}

fn render_meter(percent_left: f64, tone: UsageTone, width: Pixels, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    div()
        .w(width)
        .h(px(8.))
        .rounded_full()
        .bg(colors.border_variant)
        .child(
            div()
                .h_full()
                .w(relative((percent_left / 100.) as f32))
                .rounded_full()
                .bg(tone_color(tone).color(cx)),
        )
        .into_any_element()
}

fn render_source(source: &SummarySource, cx: &App) -> AnyElement {
    h_flex()
        .gap_1p5()
        .child(
            Icon::new(source_icon(&source.source_id))
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .children(source.windows.iter().map(|window| {
            let percent_color = match window.tone {
                UsageTone::Warning | UsageTone::Danger => tone_color(window.tone),
                UsageTone::Ok | UsageTone::Default => Color::Default,
            };
            h_flex()
                .gap_1()
                .child(render_meter(window.percent_left, window.tone, px(22.), cx))
                .child(
                    Label::new(format!("{:.0}%", window.percent_left))
                        .size(LabelSize::Small)
                        .color(percent_color),
                )
                .when(!window.short_label.is_empty(), |this| {
                    this.child(
                        Label::new(window.short_label.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
        }))
        .into_any_element()
}

/// Every account's windows, balances and details, then where they come from and Refresh.
fn usage_menu(
    store: &Entity<PaseoStore>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let store_ref = store.read(cx);
    let reports = store_ref.usage().to_vec();
    let daemon_name = store_ref.display_name();
    let error = store_ref.usage_error().cloned();
    let fetched_at = reports
        .iter()
        .filter_map(|entry| parse_time(&entry.fetched_at))
        .max();
    let store = store.downgrade();
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        let now = Utc::now();
        for (index, entry) in reports.iter().enumerate() {
            if index > 0 {
                menu = menu.separator();
            }
            let plan = entry.available().and_then(|usage| usage.plan_label.clone());
            menu = menu.header(match plan {
                Some(plan) => format!("{} · {plan}", entry.source_label),
                None => entry.source_label.clone(),
            });
            match &entry.report {
                UsageReport::Unavailable { problem } => {
                    menu = menu.custom_row(muted_row(problem_text(problem)));
                }
                UsageReport::Error { error } => {
                    menu = menu.custom_row(muted_row(format!("Could not read it: {error}")));
                }
                UsageReport::Available(_) | UsageReport::Unknown => {}
            }
            let Some(usage) = entry.available() else {
                continue;
            };
            for usage_window in &usage.windows {
                let Some(percent_left) = percent_left(usage_window) else {
                    continue;
                };
                let label = usage_window.label.clone();
                let tone = usage_window.tone;
                let reset = usage_window
                    .resets_at
                    .as_deref()
                    .and_then(|resets_at| describe_reset(resets_at, now));
                menu = menu.custom_row(move |_, cx| {
                    h_flex()
                        .w_full()
                        .gap_2()
                        // A fixed width lines the meters up.
                        .child(
                            div()
                                .w(px(120.))
                                .child(Label::new(label.clone()).truncate()),
                        )
                        .child(render_meter(percent_left, tone, px(48.), cx))
                        .child(div().w(px(80.)).child(
                            Label::new(format!("{percent_left:.0}% left")).color(tone_color(tone)),
                        ))
                        .child(div().w(px(120.)).children(reset.clone().map(|reset| {
                            Label::new(reset).size(LabelSize::Small).color(Color::Muted)
                        })))
                        .into_any_element()
                });
            }
            for balance in &usage.balances {
                let value = match (balance.remaining, balance.limit) {
                    (Some(remaining), Some(limit)) => {
                        format!(
                            "{} of {} left",
                            amount(remaining, &balance.unit),
                            amount(limit, &balance.unit)
                        )
                    }
                    (Some(remaining), None) => format!("{} left", amount(remaining, &balance.unit)),
                    (None, _) => match balance.used {
                        Some(used) => format!("{} used", amount(used, &balance.unit)),
                        None => continue,
                    },
                };
                menu = menu.custom_row(value_row(balance.label.clone(), value, balance.tone));
            }
            for detail in &usage.details {
                menu = menu.custom_row(value_row(
                    detail.label.clone(),
                    detail.value.clone(),
                    detail.tone,
                ));
            }
        }
        menu = menu.separator();
        if let Some(error) = &error {
            menu = menu.custom_row(muted_row(format!("Last read failed: {error}")));
        }
        let source = match fetched_at {
            Some(fetched_at) => format!("From {daemon_name}, {}", describe_age(fetched_at, now)),
            None => format!("From {daemon_name}"),
        };
        menu = menu.custom_row(muted_row(source));
        menu.item(
            ContextMenuEntry::new("Refresh")
                .icon(IconName::RotateCw)
                .handler(move |_, cx| {
                    store
                        .update(cx, |store, cx| store.read_usage(true, cx))
                        .ok();
                }),
        )
    })
}

fn muted_row(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyElement {
    let text = text.into();
    move |_, _| {
        Label::new(text.clone())
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element()
    }
}

fn value_row(
    label: String,
    value: String,
    tone: UsageTone,
) -> impl Fn(&mut Window, &mut App) -> AnyElement {
    move |_, _| {
        h_flex()
            .w_full()
            .gap_2()
            .child(div().flex_1().child(Label::new(label.clone())))
            .child(Label::new(value.clone()).color(match tone {
                UsageTone::Warning | UsageTone::Danger => tone_color(tone),
                UsageTone::Ok | UsageTone::Default => Color::Default,
            }))
            .into_any_element()
    }
}

fn problem_text(problem: &UsageProblem) -> String {
    match problem {
        UsageProblem::Expired {} => "The login expired".into(),
        UsageProblem::Rejected {} => "The provider refused the login".into(),
        UsageProblem::NoQuota { detail } if !detail.is_empty() => detail.clone(),
        UsageProblem::NoQuota { .. } => "No quota to report".into(),
        UsageProblem::Unknown => "Not available".into(),
    }
}

fn amount(value: f64, unit: &str) -> String {
    match unit {
        "usd" => format!("${value:.2}"),
        unit => format!("{value:.0} {unit}"),
    }
}

fn parse_time(time: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// "resets in 18 min", "resets in 2 h 5 min", or the local day and time when it is a day or
/// more away.
fn describe_reset(resets_at: &str, now: DateTime<Utc>) -> Option<String> {
    let resets_at = parse_time(resets_at)?;
    let minutes = (resets_at - now).num_minutes();
    Some(match minutes {
        ..=0 => "resets now".into(),
        1..60 => format!("resets in {minutes} min"),
        60..1440 => match minutes % 60 {
            0 => format!("resets in {} h", minutes / 60),
            rest => format!("resets in {} h {rest} min", minutes / 60),
        },
        _ => format!(
            "resets {}",
            resets_at.with_timezone(&Local).format("%a %H:%M")
        ),
    })
}

fn describe_age(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match (now - fetched_at).num_minutes() {
        ..=0 => "just now".into(),
        1 => "1 min ago".into(),
        minutes @ 2..60 => format!("{minutes} min ago"),
        minutes => format!("{} h ago", minutes / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(json: serde_json::Value) -> UsageReportEntry {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn summarizes_the_windows_marked_for_the_summary() {
        let reports = [
            report(serde_json::json!({
                "id": "claude", "fetchedAt": "2026-11-16T10:32:01Z", "sourceId": "claude",
                "sourceLabel": "Claude", "report": {"status": "available", "windows": [
                    {"id": "five_hour", "label": "Session", "shortLabel": "5h", "summary": true,
                     "remainingPct": 76, "tone": "ok"},
                    {"id": "weekly", "label": "Weekly", "summary": true, "usedPct": 95,
                     "tone": "danger"},
                    {"id": "weekly_model_fable", "label": "Weekly · Fable", "remainingPct": 100},
                ]},
            })),
            report(serde_json::json!({
                "id": "copilot", "fetchedAt": "2026-11-16T10:32:01Z", "sourceId": "copilot",
                "sourceLabel": "GitHub Copilot", "report": {"status": "available", "windows": []},
            })),
            report(serde_json::json!({
                "id": "codex", "fetchedAt": "2026-11-16T10:32:01Z", "sourceId": "codex",
                "sourceLabel": "Codex",
                "report": {"status": "error", "error": "timed out"},
                "lastSuccessful": {"fetchedAt": "2026-11-16T10:00:00Z", "report": {
                    "status": "available", "windows": [{"id": "five_hour", "label": "5-hour",
                    "shortLabel": "", "summary": true, "remainingPct": 82}]}},
            })),
        ];

        let summary = summarize(&reports);
        let described: Vec<_> = summary
            .iter()
            .map(|source| {
                let windows: Vec<_> = source
                    .windows
                    .iter()
                    .map(|window| {
                        format!(
                            "{:.0}% {} {:?}",
                            window.percent_left, window.short_label, window.tone
                        )
                    })
                    .collect();
                (source.source_id.as_str(), windows)
            })
            .collect();
        assert_eq!(
            described,
            [
                (
                    "claude",
                    vec!["76% 5h Ok".to_string(), "5% Weekly Danger".to_string()]
                ),
                ("codex", vec!["82%  Default".to_string()]),
            ],
            "Copilot has no summary window; Codex keeps its last data while it errs"
        );
        assert_eq!(source_icon("claude"), IconName::AiClaude);
        assert_eq!(source_icon("codex"), IconName::AiOpenAi);
    }

    #[test]
    fn describes_resets_and_ages() {
        let now = parse_time("2026-11-16T10:00:00Z").unwrap();
        assert_eq!(
            describe_reset("2026-11-16T10:18:30Z", now).as_deref(),
            Some("resets in 18 min")
        );
        assert_eq!(
            describe_reset("2026-11-16T12:05:00.991636+00:00", now).as_deref(),
            Some("resets in 2 h 5 min")
        );
        assert_eq!(
            describe_reset("2026-11-16T13:00:00Z", now).as_deref(),
            Some("resets in 3 h")
        );
        assert_eq!(
            describe_reset("2026-11-16T09:00:00Z", now).as_deref(),
            Some("resets now")
        );
        assert!(
            describe_reset("2026-11-18T21:00:00Z", now)
                .is_some_and(|reset| reset.starts_with("resets ") && !reset.contains(" in "))
        );
        assert_eq!(describe_reset("soon", now), None);
        assert_eq!(
            describe_age(parse_time("2026-11-16T09:57:00Z").unwrap(), now),
            "3 min ago"
        );
        assert_eq!(describe_age(now, now), "just now");
    }
}
