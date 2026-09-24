//! The event loop. It owns the terminal and the runtime's thread; the widget
//! owns what that looks like.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::runtime::{
    AgentRuntime, ApprovalChoice, ApprovalRequest, RuntimeError, TurnEvent, TurnSink,
};

use super::chatwidget::{ChatWidget, Effect, Key};
use super::composer::{Burst, PasteBurst};
use super::styles::{self, Palette};

const PASTE_LIMIT: usize = 1000;

pub struct SessionMeta {
    pub model: String,
    pub sandbox: String,
}

enum ScreenEvent {
    Turn(TurnEvent),
    Approval(ApprovalRequest),
    Finished(Result<String, String>),
}

struct ScreenSink {
    events: Sender<ScreenEvent>,
    decisions: Receiver<ApprovalChoice>,
}

impl TurnSink for ScreenSink {
    fn emit(&self, event: TurnEvent) {
        let _ = self.events.send(ScreenEvent::Turn(event));
    }

    fn choose(&self, request: ApprovalRequest) -> ApprovalChoice {
        let _ = self.events.send(ScreenEvent::Approval(request));
        self.decisions.recv().unwrap_or(ApprovalChoice::No)
    }
}

pub fn launch(
    runtime: AgentRuntime,
    meta: SessionMeta,
    first_task: Option<String>,
) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let mut widget = ChatWidget::new(meta.model, meta.sandbox, detect_palette());
    let mut burst = PasteBurst::default();
    let mut runtime = Some(runtime);
    let mut worker: Option<Worker> = None;
    let mut quit_at: Option<Instant> = None;

    if let Some(task) = first_task.filter(|task| !task.trim().is_empty()) {
        widget.paste(&task);
        if let Effect::Submit(text) = widget.on_key(Key::Enter) {
            worker = start(&mut runtime, text);
        }
    }

    let result = loop {
        terminal.draw(|frame| widget.draw(frame, frame.area()))?;

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Paste(text) => {
                    let pending = burst.take();
                    if !pending.is_empty() {
                        widget.paste(&pending);
                    }
                    widget.paste(&text);
                }
                Event::Key(key) => {
                    let flush = note_burst(&mut burst, &key);
                    if let Some(ready) = flush {
                        if ready.chars().count() > PASTE_LIMIT {
                            widget.collapse_burst(&ready);
                        }
                    }
                    match widget.on_key(map_key(key)) {
                        Effect::Quit => break Ok(()),
                        Effect::Cancel => {
                            if let Some(worker) = &worker {
                                worker.cancel.store(true, Ordering::Relaxed);
                            }
                        }
                        Effect::Submit(task) => {
                            if worker.is_none() {
                                worker = start(&mut runtime, task);
                            }
                        }
                        Effect::Choice(choice) => {
                            if let Some(worker) = &worker {
                                let _ = worker.decisions.send(choice);
                            }
                        }
                        Effect::None => {}
                    }
                    if burst.pending_chars() > PASTE_LIMIT {
                        let tail = burst.take();
                        widget.collapse_burst(&tail);
                    }
                    if widget.quit_armed() {
                        quit_at = Some(Instant::now());
                    } else {
                        quit_at = None;
                    }
                }
                _ => {}
            }
        }

        if quit_at.is_some_and(|at| at.elapsed() > Duration::from_secs(2)) {
            widget.disarm_quit();
            quit_at = None;
        }

        if let Some(handle) = &worker {
            while let Ok(event) = handle.events.try_recv() {
                apply(&mut widget, event);
            }
            if handle.done_rx.try_recv().is_ok() {
                if let Some(finished) = worker.take() {
                    if let Ok(mut returned) = finished.runtime_rx.try_recv() {
                        returned.resume_next();
                        runtime = Some(returned);
                    }
                }
                if let Some(task) = widget.take_queued() {
                    widget.paste(&task);
                    if let Effect::Submit(text) = widget.on_key(Key::Enter) {
                        worker = start(&mut runtime, text);
                    }
                }
            }
        }

        widget.tick();
    };

    ratatui::restore();
    result
}

fn apply(widget: &mut ChatWidget, event: ScreenEvent) {
    match event {
        ScreenEvent::Turn(TurnEvent::Thinking) => widget.thinking(),
        ScreenEvent::Turn(TurnEvent::Delta(text)) => widget.delta(text),
        ScreenEvent::Turn(TurnEvent::Assistant(text)) => widget.assistant(text),
        ScreenEvent::Turn(TurnEvent::ToolStart { name, arguments }) => {
            widget.tool_start(name, arguments);
        }
        ScreenEvent::Turn(TurnEvent::ToolDone {
            name,
            output,
            failed,
        }) => {
            widget.tool_done(name, output, failed);
        }
        ScreenEvent::Approval(request) => {
            widget.ask(
                request.tool,
                request.preview,
                request.concerns,
                request.flagged,
            );
        }
        ScreenEvent::Finished(result) => {
            let ok = result.is_ok();
            let text = result.unwrap_or_else(|err| err);
            widget.finished(ok, text);
        }
    }
}

struct Worker {
    events: Receiver<ScreenEvent>,
    decisions: Sender<ApprovalChoice>,
    cancel: Arc<AtomicBool>,
    done_rx: Receiver<()>,
    runtime_rx: Receiver<AgentRuntime>,
}

fn start(runtime: &mut Option<AgentRuntime>, task: String) -> Option<Worker> {
    let runtime = runtime.take()?;
    let cancel = runtime.cancel_flag();
    let (event_tx, event_rx) = mpsc::channel();
    let (decision_tx, decision_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let (runtime_tx, runtime_rx) = mpsc::channel();
    let runtime = runtime.with_sink(Box::new(ScreenSink {
        events: event_tx.clone(),
        decisions: decision_rx,
    }));
    thread::spawn(move || {
        let result = runtime
            .run(&task)
            .map_err(|err: RuntimeError| err.to_string());
        let _ = event_tx.send(ScreenEvent::Finished(result));
        let _ = runtime_tx.send(runtime);
        let _ = done_tx.send(());
    });
    Some(Worker {
        events: event_rx,
        decisions: decision_tx,
        cancel,
        done_rx,
        runtime_rx,
    })
}

/// Characters that are not a chord go into the burst detector. A gap returns
/// the burst that just ended, still sitting in the composer as typed keys.
fn note_burst(burst: &mut PasteBurst, key: &KeyEvent) -> Option<String> {
    let KeyCode::Char(ch) = key.code else {
        let pending = burst.take();
        return (!pending.is_empty()).then_some(pending);
    };
    if !key.modifiers.is_empty() {
        return None;
    }
    match burst.push(millis(), ch) {
        Burst::Held => None,
        Burst::Flushed { ready, .. } => Some(ready),
    }
}

fn map_key(key: KeyEvent) -> Key {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(ch) if ctrl => Key::Ctrl(ch),
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => Key::Char('\n'),
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Char(ch) => Key::Char(ch),
        _ => Key::Ctrl('\0'),
    }
}

fn detect_palette() -> Palette {
    let theme = styles::theme_from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref());
    let depth = styles::depth_from_env(
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var_os("NO_COLOR").is_some(),
    );
    styles::palette(theme, depth)
}

fn millis() -> u128 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis()
}
