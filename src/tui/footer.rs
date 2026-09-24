//! One footer line. Higher-priority hints stay when the terminal gets narrow;
//! the rest are dropped from the end, never squashed into an unreadable row.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterMode {
    Empty,
    Draft,
    Running,
    Approval,
}

#[derive(Debug, Clone)]
pub struct FooterInput<'a> {
    pub width: usize,
    pub mode: FooterMode,
    pub quit_armed: bool,
    pub queued: usize,
    pub model: &'a str,
    pub sandbox: &'a str,
    pub pet: &'a str,
    pub show_pet: bool,
}

/// Highest priority first. The line is the join of whatever still fits.
pub fn footer_line(input: &FooterInput<'_>) -> String {
    let mut parts = Vec::new();
    if input.quit_armed {
        parts.push("press again to quit".to_string());
    }
    if input.queued > 0 {
        parts.push(format!("queued {}", input.queued));
    }
    parts.push(mode_hint(input.mode).to_string());
    parts.push("? help  ^t tail  ^r search".to_string());
    let context = format!("{}  {}", input.model, input.sandbox);
    parts.push(context);
    if input.show_pet {
        parts.push(input.pet.to_string());
    }

    let mut kept = Vec::new();
    let mut used = 0;
    for (index, part) in parts.iter().enumerate() {
        let extra = if kept.is_empty() { 0 } else { 3 };
        if used + extra + part.chars().count() > input.width && index > 0 {
            break;
        }
        used += extra + part.chars().count();
        kept.push(part.as_str());
    }
    if kept.is_empty() {
        return String::new();
    }
    let mut line = kept.join(" · ");
    if line.chars().count() > input.width {
        line = line.chars().take(input.width).collect();
    }
    line
}

fn mode_hint(mode: FooterMode) -> &'static str {
    match mode {
        FooterMode::Empty => "type a task",
        FooterMode::Draft => "enter sends",
        FooterMode::Running => "esc esc interrupts",
        FooterMode::Approval => "1 once  2 always  3 no",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(width: usize, mode: FooterMode) -> FooterInput<'static> {
        FooterInput {
            width,
            mode,
            quit_armed: false,
            queued: 0,
            model: "gpt-5.5",
            sandbox: "workspace-write",
            pet: "(·‿·)",
            show_pet: false,
        }
    }

    #[test]
    fn a_narrow_footer_keeps_the_mode_and_drops_context() {
        let line = footer_line(&input(28, FooterMode::Running));
        assert!(line.contains("esc esc interrupts"));
        assert!(!line.contains("gpt-5.5"));
        assert!(!line.contains("help"));
    }

    #[test]
    fn quit_and_queue_outrank_the_mode() {
        let mut input = input(40, FooterMode::Draft);
        input.quit_armed = true;
        input.queued = 2;
        let line = footer_line(&input);
        assert!(line.starts_with("press again to quit · queued 2"));
        assert!(!line.contains("gpt-5.5"));
    }

    #[test]
    fn a_wide_footer_keeps_the_context() {
        let line = footer_line(&input(120, FooterMode::Empty));
        assert!(line.contains("type a task"));
        assert!(line.contains("gpt-5.5"));
        assert!(line.contains("workspace-write"));
    }
}
