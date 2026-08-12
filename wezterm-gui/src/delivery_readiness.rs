use mux::pane::{CachePolicy, Pane};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryReadiness {
    Ready,
    Busy,
    Blocked,
    Unknown,
}

fn contains(lines: &[&str], needle: &str) -> bool {
    let needle = needle.to_lowercase();
    lines
        .iter()
        .any(|line| line.to_lowercase().contains(&needle))
}

fn recent_non_empty<'a>(screen: &'a str, count: usize) -> Vec<&'a str> {
    let lines = screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    lines[lines.len().saturating_sub(count)..].to_vec()
}

fn spinner_gerund(line: &str) -> bool {
    let mut braille = false;
    for word in line.split_whitespace() {
        braille |= word
            .chars()
            .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch));
        if braille && word.trim_matches(['-', ':', '.', ',']).ends_with("ing") {
            return true;
        }
    }
    false
}

// Delivery readiness is intentionally independent of Fleet activity.
// @fdb:delivery-readiness-is-separate
pub(crate) fn detect(harness: &str, screen: &str) -> DeliveryReadiness {
    use DeliveryReadiness::*;
    let harness = harness
        .strip_suffix(".exe")
        .or_else(|| harness.strip_suffix(".js"))
        .or_else(|| harness.strip_suffix(".py"))
        .unwrap_or(harness);
    let harness = match harness {
        "oh-my-pi" => "omp",
        "claude-code" => "claude",
        "open-code" | "herdr:opencode" => "opencode",
        "herdr:pi" => "pi",
        "antigravity" | "antigravity-cli" => "agy",
        other => other,
    };
    let lines = screen.lines().collect::<Vec<_>>();
    for line in &lines {
        let clean = line.trim();
        let lower = clean.to_lowercase();
        if lower.contains("requesting permission for:")
            || lower.contains("press enter to confirm or esc to go back")
            || lower.contains("press enter to confirm or esc to cancel")
            || lower.contains("allow command?")
        {
            return Blocked;
        }
        let progress = harness == "opencode"
            && clean
                .chars()
                .collect::<Vec<_>>()
                .windows(4)
                .any(|cells| cells.iter().all(|cell| matches!(cell, '■' | '⬝')));
        let agy_tasks = harness == "agy" && {
            clean.split_whitespace().collect::<Vec<_>>().windows(3).any(|fields| {
                fields[0] == "·"
                    && fields[1].parse::<usize>().is_ok()
                    && fields[2].starts_with("task")
            })
        };
        if lower.contains("esc to interrupt")
            || (lower.contains("ctrl+c") && lower.contains("interrupt"))
            || lower.contains("esc to cancel")
            || lower.contains("✳ thinking")
            || spinner_gerund(&lower)
            || progress
            || agy_tasks
        {
            return Busy;
        }
    }
    match harness {
        "omp" => {
            let lines = recent_non_empty(screen, 8);
            if contains(&lines, "allow tool:")
                && contains(&lines, "approve")
                && contains(&lines, "deny")
            {
                return Blocked;
            }
            if let [.., composer, border] = lines.as_slice() {
                if composer.contains('╭') && composer.contains('π') && border.contains('╰') {
                    return Ready;
                }
            }
            return Unknown;
        }
        "reasonix" => {
            let lines = recent_non_empty(screen, 10);
            if contains(&lines, "permission required") && contains(&lines, "will call tool") {
                return Blocked;
            }
            if contains(&lines, "auto · ready · shift+tab ask/auto/plan")
                && lines.iter().any(|line| line.trim() == "❯")
            {
                return Ready;
            }
            return Unknown;
        }
        "pi" => {
            let lines = recent_non_empty(screen, 8);
            if contains(&lines, "↑↓ navigate")
                && contains(&lines, "enter select")
                && contains(&lines, "cancel")
            {
                return Blocked;
            }
            let prompt = lines.iter().rposition(|line| line.trim() == "❯");
            let status = lines.iter().rposition(|line| {
                let lower = line.to_lowercase();
                lower.contains("%/") && lower.contains("(auto)")
            });
            return match (prompt, status) {
                (Some(prompt), Some(status)) if status > prompt => Ready,
                _ => Unknown,
            };
        }
        _ => {}
    }

    let mut ready = false;
    for line in lines {
        let clean = line.trim();
        let lower = clean.to_lowercase();
        ready |= match harness {
            "agy" => lower.contains("? for shortcuts"),
            "claude" => clean.starts_with('❯'),
            "codex" => clean.starts_with('›'),
            "opencode" => lower.contains("ask anything..."),
            _ => false,
        };
    }
    if ready {
        Ready
    } else {
        Unknown
    }
}

pub(crate) fn detect_pane(pane: &dyn Pane) -> DeliveryReadiness {
    let vars = pane.copy_user_vars();
    let process = pane
        .get_foreground_process_name(CachePolicy::AllowStale)
        .unwrap_or_default();
    let harness = vars
        .get("AGENT_HARNESS")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| process.rsplit(['/', '\\']).next())
        .unwrap_or_default();
    let dimensions = pane.get_dimensions();
    let bottom = dimensions.physical_top + dimensions.viewport_rows as isize;
    let top = bottom.saturating_sub(dimensions.viewport_rows as isize);
    let (_, lines) = pane.get_lines(top..bottom);
    let screen = lines
        .iter()
        .map(|line| {
            let mut text = String::new();
            for cell in line.visible_cells() {
                text.push_str(cell.str());
            }
            text.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    detect(harness, &screen)
}

#[cfg(test)]
mod tests {
    use super::{detect, DeliveryReadiness::*};

    #[test]
    fn delivery_requires_a_current_harness_composer() {
        assert_eq!(detect("codex", "› Ask Codex"), Ready);
        assert_eq!(
            detect("codex", "› old prompt\n• Working (2s esc to interrupt)"),
            Busy
        );
        assert_eq!(detect("claude", "❯ old prompt\n⠋ Thinking"), Busy);
        assert_eq!(detect("claude", "❯ old prompt\n✳ Thinking… (1m 23s)"), Busy);
        assert_eq!(
            detect(
                "codex",
                "› old prompt\nPress enter to confirm or esc to go back"
            ),
            Blocked
        );
        assert_eq!(detect("claude", "❯ Try a request"), Ready);
        assert_eq!(detect("opencode", "Ask anything..."), Ready);
        assert_eq!(detect("opencode", "Ask anything...\nstatus ■⬝■⬝"), Busy);
        assert_eq!(detect("agy", "? for shortcuts\nold · 3 tasks"), Busy);
        assert_eq!(
            detect("agy", "? for shortcuts\n⠋ prior glyph\nworking"),
            Ready
        );
        assert_eq!(detect("agy", "? for shortcuts\n⠋ Thinking"), Busy);
        assert_eq!(detect("agy", "? for shortcuts\n· 2 tasks"), Busy);
        assert_eq!(detect("omp", "╭── π  > model\n╰─"), Ready);
        assert_eq!(detect("omp", "╭── π  > model\n╰─\n⠋ Thinking"), Busy);
        assert_eq!(detect("pi", "❯\nmain 0.0%/272k (auto)"), Ready);
        assert_eq!(detect("codex", "ordinary output"), Unknown);
    }
}
