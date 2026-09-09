//! Bound worker-event work between input polls and share streaming redraws.

use std::{future::Future, io, time::Duration};

use crossterm::event::Event;
use futures_util::{Stream, StreamExt};
use ratatui::{backend::Backend, Terminal};
use tokio::{sync::mpsc, time::Instant};

use super::super::{
    draw, flush_terminal_requests, handle_terminal_event, handle_ui_msg, transcript_content_width,
    App, UiMsg, WorkerCmd,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const EVENT_BUDGET: Duration = Duration::from_millis(2);
const MAX_BATCH: usize = 64;

fn draw_frame<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<usize> {
    let width = terminal.size()?.width as usize;
    app.absorb_pending();
    terminal.draw(|frame| draw(frame, app))?;
    Ok(width)
}

pub(crate) async fn run_loop<B, S, F>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiMsg>,
    mut input: S,
    shutdown: F,
) -> io::Result<()>
where
    B: Backend,
    S: Stream<Item = io::Result<Event>> + Unpin,
    F: Future<Output = ()>,
{
    let mut width = draw_frame(terminal, app)?;
    let mut next_frame = Instant::now() + FRAME_INTERVAL;
    let mut dirty = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(shutdown);

    while !app.quit {
        let mut draw_now = false;
        tokio::select! {
            // Poll input before another worker batch, even under continuous
            // output. A due frame also gets its turn before the next batch.
            biased;
            _ = &mut shutdown => app.quit = true,
            event = input.next() => {
                match event {
                    Some(Ok(event)) => {
                        if let Event::Resize(new_width, _) = &event {
                            width = *new_width as usize;
                        }
                        handle_terminal_event(app, event, worker, width);
                        draw_now = !app.quit;
                    }
                    Some(Err(_)) | None => app.quit = true,
                }
            }
            _ = tokio::time::sleep_until(next_frame), if dirty => draw_now = true,
            _ = ticker.tick(), if app.running()
                || app.cfg.stats.processes() > 0
                || app.scroll_hint_live() => {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
                dirty = true;
            }
            message = ui_rx.recv() => {
                match message {
                    Some(first) => {
                        let started = Instant::now();
                        let mut message = first;
                        for count in 1..=MAX_BATCH {
                            // Stop at interactive boundaries so the user sees
                            // the prompt before any following worker traffic.
                            let interactive = matches!(&message, UiMsg::Approval(_) | UiMsg::Ask(_));
                            let content_width = transcript_content_width(app, width);
                            handle_ui_msg(app, message, worker, content_width);
                            // Preserve the per-event history transitions from
                            // the old loop without paying for a frame each time.
                            app.absorb_pending();
                            dirty = true;
                            if interactive {
                                draw_now = true;
                                break;
                            }
                            if app.quit || count == MAX_BATCH || started.elapsed() >= EVENT_BUDGET {
                                break;
                            }
                            match ui_rx.try_recv() {
                                Ok(next) => message = next,
                                Err(_) => break,
                            }
                        }
                    }
                    None => {
                        // Do not lose the final queued update when the sender
                        // closes before its scheduled frame.
                        draw_now = dirty;
                        app.quit = true;
                    }
                }
            }
        }

        // Clipboard control sequences must stay outside the cell renderer.
        flush_terminal_requests(app)?;
        if draw_now {
            width = draw_frame(terminal, app)?;
            next_frame = Instant::now() + FRAME_INTERVAL;
            dirty = false;
        }
    }
    Ok(())
}
